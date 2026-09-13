use std::sync::{Arc, Mutex};
use std::time::Duration;

use events::{ActorType, ExpectedVersion, NewEvent, WorkflowRef};
use serde_json::json;

use super::{
    DeliveryDispatcher, DispatchError, ProductionDispatcher, handle_next_delivery,
    project_and_publish_agent_snapshot, run_event_handler,
};
use crate::agent::domain::{
    AgentEvent, AgentIncarnation, AgentPaneObservation, AgentPaneObserved, CompletionSummaryUpdate,
};
use crate::agent::snapshot::AgentSnapshotHub;
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
    let snapshots = AgentSnapshotHub::new(store.load_agent_snapshot().await.unwrap());
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
    let task = tokio::spawn(run_event_handler(store, snapshots, shutdown_rx));

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

#[tokio::test]
async fn projection_publishes_only_committed_agent_snapshot_changes() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(&directory.0).await.unwrap();
    let hub = AgentSnapshotHub::new(store.load_agent_snapshot().await.unwrap());
    let mut receiver = hub.subscribe();
    append_agent_events(
        &store,
        vec![AgentEvent::PaneObserved(AgentPaneObserved {
            pane: AgentPaneObservation {
                incarnation: AgentIncarnation {
                    pane_id: "%12".into(),
                    pane_pid: 120,
                    agent_pid: 121,
                    agent_started_at_ms: 2_000,
                    provider_id: "codex".into(),
                },
                tmux_target: "harold:2.2".into(),
                session_name: "harold".into(),
                window_index: 2,
                pane_index: 2,
                working_directory: "/work/harold".into(),
                provider_display_name: "Codex".into(),
                observed_at_ms: 2_100,
            },
        })],
    )
    .await
    .unwrap();

    store.fail_projection_before_checkpoint_for_test();
    assert!(
        project_and_publish_agent_snapshot(&store, &hub, 500)
            .await
            .is_err()
    );
    assert!(receiver.has_changed().is_ok_and(|changed| !changed));

    let batch = project_and_publish_agent_snapshot(&store, &hub, 500)
        .await
        .unwrap();
    assert!(batch.snapshot_changed);
    receiver
        .changed()
        .await
        .expect("committed snapshot published");
    let published = receiver.borrow_and_update().clone();
    assert_eq!(published.through_event_version, batch.through_event_version);
    assert_eq!(published.panes[0].pane.incarnation.pane_id, "%12");

    let empty = project_and_publish_agent_snapshot(&store, &hub, 500)
        .await
        .unwrap();
    assert_eq!(empty.applied, 0);
    assert!(receiver.has_changed().is_ok_and(|changed| !changed));
}

#[tokio::test]
async fn post_commit_snapshot_load_failure_is_retried_on_the_next_noop_cycle() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(&directory.0).await.unwrap();
    let hub = AgentSnapshotHub::new(store.load_agent_snapshot().await.unwrap());
    let mut receiver = hub.subscribe();
    append_agent_events(
        &store,
        vec![AgentEvent::PaneObserved(AgentPaneObserved {
            pane: AgentPaneObservation {
                incarnation: AgentIncarnation {
                    pane_id: "%13".into(),
                    pane_pid: 130,
                    agent_pid: 131,
                    agent_started_at_ms: 3_000,
                    provider_id: "codex".into(),
                },
                tmux_target: "harold:3.1".into(),
                session_name: "harold".into(),
                window_index: 3,
                pane_index: 1,
                working_directory: "/work/harold".into(),
                provider_display_name: "Codex".into(),
                observed_at_ms: 3_100,
            },
        })],
    )
    .await
    .unwrap();
    store.fail_next_snapshot_load_for_test();

    assert!(
        project_and_publish_agent_snapshot(&store, &hub, 500)
            .await
            .is_err()
    );
    assert_eq!(store.last_processed_version().await.unwrap().get(), 1);
    assert!(receiver.has_changed().is_ok_and(|changed| !changed));

    let retry = project_and_publish_agent_snapshot(&store, &hub, 500)
        .await
        .unwrap();
    assert_eq!(retry.applied, 0);
    tokio::time::timeout(Duration::from_millis(100), receiver.changed())
        .await
        .expect("committed snapshot was not retried")
        .expect("snapshot hub closed");
    let published = receiver.borrow_and_update().clone();
    assert_eq!(published.through_event_version.get(), 1);
    assert_eq!(published.panes[0].pane.incarnation.pane_id, "%13");
}

#[tokio::test]
async fn non_agent_batch_publishes_the_advanced_durable_checkpoint() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(&directory.0).await.unwrap();
    let hub = AgentSnapshotHub::new(store.load_agent_snapshot().await.unwrap());
    let mut receiver = hub.subscribe();
    append_inbound_message(
        &store,
        &InboundMessage {
            text: "continue".into(),
        },
    )
    .await
    .unwrap();

    let batch = project_and_publish_agent_snapshot(&store, &hub, 500)
        .await
        .unwrap();

    assert!(!batch.snapshot_changed);
    assert_eq!(batch.through_event_version.get(), 1);
    tokio::time::timeout(Duration::from_millis(100), receiver.changed())
        .await
        .expect("checkpoint was not published")
        .expect("snapshot hub closed");
    assert_eq!(receiver.borrow_and_update().through_event_version.get(), 1);
    assert!(receiver.borrow().panes.is_empty());
}
