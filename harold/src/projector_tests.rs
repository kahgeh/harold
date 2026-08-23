use std::sync::{Arc, Mutex};
use std::time::Duration;

use events::{ActorType, ExpectedVersion, NewEvent, WorkflowRef};
use serde_json::json;

use super::{
    DeliveryDispatcher, DispatchError, ProductionDispatcher, handle_next_delivery,
    run_event_handler,
};
use crate::agent::domain::{
    AgentEvent, AgentIncarnation, AgentPaneObservation, AgentPaneObserved, CompletionSummaryUpdate,
};
use crate::outbound::DeliveryOutcome;
use crate::store::{
    HaroldStore, InboundMessage, PendingDelivery, TurnCompleted, append_agent_events,
    append_inbound_message, append_turn_completed,
};

struct TestDirectory(std::path::PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("harold-handler-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("create test directory");
        Self(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Default)]
struct RecordingDispatcher {
    event_types: Mutex<Vec<String>>,
}

impl DeliveryDispatcher for RecordingDispatcher {
    fn dispatch(&self, delivery: &PendingDelivery) -> Result<DeliveryOutcome, DispatchError> {
        self.event_types
            .lock()
            .unwrap()
            .push(delivery.event_type.clone());
        Ok(DeliveryOutcome::Delivered)
    }
}

struct FailingDispatcher;

impl DeliveryDispatcher for FailingDispatcher {
    fn dispatch(&self, _delivery: &PendingDelivery) -> Result<DeliveryOutcome, DispatchError> {
        Err(DispatchError::Retryable("temporary failure".into()))
    }
}

#[tokio::test]
async fn handler_dispatches_staged_events_in_stream_version_order() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(&directory.0).await.unwrap();
    append_turn_completed(
        &store,
        &TurnCompleted {
            pane_id: "%4".into(),
            pane_label: "harold:0.4".into(),
            last_user_prompt: "Run the task".into(),
            assistant_message: "Task complete".into(),
            main_context: "harold".into(),
            agent_incarnation: None,
            work_summary: CompletionSummaryUpdate::Unchanged,
        },
    )
    .await
    .unwrap();
    append_inbound_message(
        &store,
        &InboundMessage {
            text: "continue".into(),
        },
    )
    .await
    .unwrap();
    let dispatcher = Arc::new(RecordingDispatcher::default());

    assert!(
        handle_next_delivery(&store, dispatcher.clone())
            .await
            .unwrap()
    );
    assert!(
        handle_next_delivery(&store, dispatcher.clone())
            .await
            .unwrap()
    );
    assert!(
        !handle_next_delivery(&store, dispatcher.clone())
            .await
            .unwrap()
    );

    assert_eq!(
        *dispatcher.event_types.lock().unwrap(),
        ["TurnCompleted", "InboundMessageReceived"]
    );
}

#[tokio::test]
async fn idle_handler_stops_when_shutdown_channel_closes() {
    let directory = TestDirectory::new();
    let store = Arc::new(HaroldStore::open(&directory.0).await.unwrap());
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
    let task = tokio::spawn(run_event_handler(store, shutdown_rx));

    drop(shutdown_tx);

    tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("handler did not stop")
        .expect("handler task panicked");
}

#[tokio::test]
async fn retryable_delivery_failure_remains_pending_for_a_later_attempt() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(&directory.0).await.unwrap();
    append_inbound_message(
        &store,
        &InboundMessage {
            text: "retry me".into(),
        },
    )
    .await
    .unwrap();

    let error = handle_next_delivery(&store, Arc::new(FailingDispatcher))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("temporary failure"));

    assert!(
        handle_next_delivery(&store, Arc::new(RecordingDispatcher::default()))
            .await
            .unwrap()
    );
    assert!(store.next_pending_delivery().await.unwrap().is_none());
}

#[tokio::test]
async fn permanently_invalid_event_does_not_block_later_deliveries() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(&directory.0).await.unwrap();
    store
        .stream()
        .append(
            ExpectedVersion::Any,
            [NewEvent {
                r#type: "Unknown".into(),
                payload: json!({"unexpected": true}),
                workflow_kind: None,
                workflow: WorkflowRef::None,
                request_id: None,
                actor_id: "system:test".into(),
                actor_type: ActorType::System,
            }],
        )
        .await
        .unwrap();
    append_inbound_message(
        &store,
        &InboundMessage {
            text: "still deliver me".into(),
        },
    )
    .await
    .unwrap();

    assert!(
        handle_next_delivery(&store, Arc::new(ProductionDispatcher))
            .await
            .unwrap()
    );
    let next = store.next_pending_delivery().await.unwrap().unwrap();
    assert_eq!(next.event_version.get(), 2);
}

#[tokio::test]
async fn malformed_known_event_does_not_block_a_valid_later_delivery() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(&directory.0).await.unwrap();
    store
        .stream()
        .append(
            ExpectedVersion::Any,
            [NewEvent {
                r#type: "TurnCompleted".into(),
                payload: json!({"pane_id": 7}),
                workflow_kind: None,
                workflow: WorkflowRef::None,
                request_id: None,
                actor_id: "system:test".into(),
                actor_type: ActorType::System,
            }],
        )
        .await
        .unwrap();
    append_inbound_message(
        &store,
        &InboundMessage {
            text: "deliver after malformed event".into(),
        },
    )
    .await
    .unwrap();

    assert!(
        handle_next_delivery(&store, Arc::new(ProductionDispatcher))
            .await
            .unwrap()
    );

    let dispatcher = Arc::new(RecordingDispatcher::default());
    assert!(
        handle_next_delivery(&store, dispatcher.clone())
            .await
            .unwrap()
    );
    assert_eq!(
        *dispatcher.event_types.lock().unwrap(),
        ["InboundMessageReceived"]
    );
    assert!(store.next_pending_delivery().await.unwrap().is_none());
}

#[tokio::test]
async fn agent_only_events_are_projected_without_creating_a_delivery() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(&directory.0).await.unwrap();
    append_agent_events(
        &store,
        vec![AgentEvent::PaneObserved(AgentPaneObserved {
            pane: AgentPaneObservation {
                incarnation: AgentIncarnation {
                    pane_id: "%11".into(),
                    pane_pid: 11,
                    agent_pid: 12,
                    agent_started_at_ms: 1_000,
                    provider_id: "codex".into(),
                },
                tmux_target: "harold:2.1".into(),
                session_name: "harold".into(),
                window_index: 2,
                pane_index: 1,
                working_directory: "/work/harold".into(),
                provider_display_name: "Codex".into(),
                observed_at_ms: 100,
            },
        })],
    )
    .await
    .unwrap();

    assert!(
        !handle_next_delivery(&store, Arc::new(RecordingDispatcher::default()))
            .await
            .unwrap()
    );
    assert_eq!(store.last_processed_version().await.unwrap().get(), 1);
    assert_eq!(store.load_agent_snapshot().await.unwrap().panes.len(), 1);
    assert!(store.next_pending_delivery().await.unwrap().is_none());
}
