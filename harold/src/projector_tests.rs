use std::sync::{Arc, Mutex};
use std::time::Duration;

use events::{ActorType, ExpectedVersion, NewEvent, WorkflowRef};
use serde_json::json;

use super::{
    DeliveryDispatcher, DispatchError, ProductionDispatcher, dispatch_away_message,
    handle_next_delivery, handle_next_delivery_inner, production_dispatcher,
    project_and_publish_agent_snapshot, route_message, run_event_handler,
};
use crate::agent::domain::{
    AgentEvent, AgentIncarnation, AgentMonitorHealthChanged, AgentPaneDeparted,
    AgentPaneObservation, AgentPaneObserved, CompletionSummaryUpdate,
};
use crate::agent::snapshot::AgentSnapshotHub;
use crate::inbound::AgentAddress;
use crate::outbound::fake::{CapturedLogs, FakeEnvironment};
use crate::outbound::{DeliveryOutcome, SystemEnvironment};
use crate::store::{
    AwayMessageRequested, HaroldStore, InboundMessage, PendingDelivery, TurnCompleted,
    append_agent_events, append_away_message_requested, append_inbound_message,
    append_messaging_paused_changed, append_turn_completed,
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

/// The dispatcher the daemon runs. Use it only where the event cannot reach a send.
fn system_dispatcher() -> ProductionDispatcher {
    ProductionDispatcher::new(Arc::new(SystemEnvironment), None)
}

/// The daemon's dispatcher over a fake environment the test keeps a handle to.
fn dispatcher_with(environment: &Arc<FakeEnvironment>) -> ProductionDispatcher {
    ProductionDispatcher::new(environment.clone(), None)
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
            forwarded: false,
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
            forwarded: false,
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
            forwarded: false,
        },
    )
    .await
    .unwrap();

    assert!(
        handle_next_delivery(&store, Arc::new(system_dispatcher()))
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
            forwarded: false,
        },
    )
    .await
    .unwrap();

    assert!(
        handle_next_delivery(&store, Arc::new(system_dispatcher()))
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
            forwarded: false,
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

#[test]
fn paused_inbound_delivery_is_skipped_without_routing() {
    // The payload is deliberately unroutable: were the gate removed, this fails on the
    // payload instead of relaying text to a real pane.
    let delivery = PendingDelivery {
        event_id: "event-1".into(),
        event_version: events::EventStreamVersion::new(1).unwrap(),
        event_type: "InboundMessageReceived".into(),
        payload: json!(null),
        trace_id: "trace".into(),
    };

    let outcome = system_dispatcher().dispatch_with_pause(&delivery, true);
    assert!(matches!(outcome, Ok(DeliveryOutcome::Skipped)));

    let outcome = system_dispatcher().dispatch_with_pause(&delivery, false);
    assert!(matches!(outcome, Err(DispatchError::Permanent(_))));
}

#[tokio::test]
async fn projecting_a_pause_change_sets_the_messaging_switch() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(&directory.0).await.unwrap();
    let snapshots = AgentSnapshotHub::new(store.load_agent_snapshot().await.unwrap());
    assert!(!crate::outbound::is_messaging_paused());

    append_messaging_paused_changed(&store, true).await.unwrap();
    project_and_publish_agent_snapshot(&store, &snapshots, 500)
        .await
        .unwrap();
    assert!(crate::outbound::is_messaging_paused());

    append_messaging_paused_changed(&store, false)
        .await
        .unwrap();
    project_and_publish_agent_snapshot(&store, &snapshots, 500)
        .await
        .unwrap();
    assert!(!crate::outbound::is_messaging_paused());
}

#[test]
fn production_dispatcher_reads_the_messaging_switch() {
    // Unroutable on purpose, as above: without the switch this fails on the payload.
    let delivery = PendingDelivery {
        event_id: "event-1".into(),
        event_version: events::EventStreamVersion::new(1).unwrap(),
        event_type: "InboundMessageReceived".into(),
        payload: json!(null),
        trace_id: "trace".into(),
    };

    crate::outbound::set_messaging_paused(true);
    let outcome = system_dispatcher().dispatch(&delivery);
    assert!(matches!(outcome, Ok(DeliveryOutcome::Skipped)));

    crate::outbound::set_messaging_paused(false);
    let outcome = system_dispatcher().dispatch(&delivery);
    assert!(matches!(outcome, Err(DispatchError::Permanent(_))));
}

#[test]
fn stored_reply_reaches_routing_with_its_forwarded_flag() {
    for forwarded in [false, true] {
        let message = InboundMessage {
            text: "[studio] carry on".into(),
            forwarded,
        };
        let mut routed = None;

        let outcome = route_message(&message, |text, flag| {
            routed = Some((text.to_string(), flag));
            Ok(DeliveryOutcome::Delivered)
        });

        assert!(matches!(outcome, Ok(DeliveryOutcome::Delivered)));
        assert_eq!(routed, Some(("[studio] carry on".to_string(), forwarded)));
    }
}

#[test]
fn routing_failure_of_a_stored_reply_is_retryable() {
    let message = InboundMessage {
        text: "carry on".into(),
        forwarded: false,
    };

    let outcome = route_message(&message, |_, _| Err("tmux failed".to_string()));

    assert!(matches!(outcome, Err(DispatchError::Retryable(error)) if error == "tmux failed"));
}

fn away_delivery(payload: serde_json::Value) -> PendingDelivery {
    PendingDelivery {
        event_id: "event-1".into(),
        event_version: events::EventStreamVersion::new(1).unwrap(),
        event_type: "AwayMessageRequested".into(),
        payload,
        trace_id: "trace".into(),
    }
}

fn pod_message(text: &str) -> AwayMessageRequested {
    AwayMessageRequested {
        pane_id: "%47".into(),
        pane_label: "pods:0.1".into(),
        text: text.into(),
    }
}

#[test]
fn away_message_is_sent_as_stored_and_then_its_pane_is_recorded() {
    for text in [
        "Pod 3 is blocked",
        "Deploy now?",
        "See https://example.com/a?b=1",
    ] {
        let steps = Mutex::new(Vec::new());
        let mut recorded: Option<AgentAddress> = None;

        let outcome = dispatch_away_message(
            &away_delivery(json!(pod_message(text))),
            false,
            |_| None,
            |message| {
                steps.lock().unwrap().push(format!("send {}", message.text));
                assert_eq!(message, &pod_message(text));
                Ok(())
            },
            |source| {
                steps.lock().unwrap().push("record".to_string());
                recorded = Some(source);
            },
        );

        assert!(matches!(outcome, Ok(DeliveryOutcome::Delivered)));
        assert_eq!(
            *steps.lock().unwrap(),
            [format!("send {text}"), "record".to_string()]
        );
        let recorded = recorded.expect("the source pane is recorded");
        assert_eq!(recorded.pane_id(), "%47");
        assert_eq!(recorded.label(), "pods:0.1");
    }
}

#[test]
fn failed_away_message_send_is_retryable_and_records_no_pane() {
    let outcome = dispatch_away_message(
        &away_delivery(json!(pod_message("Pod 3 is blocked"))),
        false,
        |_| None,
        |_| Err("channel down".to_string()),
        |_| panic!("a failed send must not record its pane"),
    );

    assert!(matches!(outcome, Err(DispatchError::Retryable(error)) if error == "channel down"));
}

#[test]
fn paused_away_message_is_skipped_without_sending() {
    let outcome = dispatch_away_message(
        &away_delivery(json!(pod_message("Pod 3 is blocked"))),
        true,
        |_| panic!("a paused message must not be checked against the folder"),
        |_| panic!("a paused message must not be sent"),
        |_| panic!("a paused message must not record its pane"),
    );

    assert!(matches!(outcome, Ok(DeliveryOutcome::Skipped)));
}

#[test]
fn malformed_away_message_payload_is_a_terminal_failure() {
    let outcome = dispatch_away_message(
        &away_delivery(json!({"pane_id": 7})),
        false,
        |_| panic!("a malformed message must not be checked against the folder"),
        |_| panic!("a malformed message must not be sent"),
        |_| panic!("a malformed message must not record its pane"),
    );

    assert!(
        matches!(outcome, Err(DispatchError::Permanent(error)) if error.starts_with("invalid AwayMessageRequested payload"))
    );
}

#[test]
fn away_message_the_drop_check_names_is_skipped_without_sending() {
    let mut checked = None;

    let outcome = dispatch_away_message(
        &away_delivery(json!(pod_message("Pod 3 is blocked"))),
        false,
        |pane_id| {
            checked = Some(pane_id.to_string());
            Some("away message dropped (messages come from /work/chief-of-pods only)".to_string())
        },
        |_| panic!("a dropped message must not be sent"),
        |_| panic!("a dropped message must not record its pane"),
    );

    assert!(matches!(outcome, Ok(DeliveryOutcome::Skipped)));
    assert_eq!(checked.as_deref(), Some("%47"));
}

#[test]
fn production_dispatcher_handles_away_messages_and_reads_the_pause() {
    // The payload is unreadable on purpose: were either gate removed, this fails on the
    // payload and never reaches the real away channel.
    let delivery = away_delivery(json!(null));

    let outcome = system_dispatcher().dispatch_with_pause(&delivery, true);
    assert!(matches!(outcome, Ok(DeliveryOutcome::Skipped)));

    let outcome = system_dispatcher().dispatch_with_pause(&delivery, false);
    assert!(
        matches!(outcome, Err(DispatchError::Permanent(error)) if error.starts_with("invalid AwayMessageRequested payload"))
    );
}

/// Sends through a fake that fails once, then succeeds.
struct FlakyAwayDispatcher {
    fail_next: std::sync::atomic::AtomicBool,
    sent: Mutex<Vec<String>>,
}

impl DeliveryDispatcher for FlakyAwayDispatcher {
    fn dispatch(&self, delivery: &PendingDelivery) -> Result<DeliveryOutcome, DispatchError> {
        dispatch_away_message(
            delivery,
            false,
            |_| None,
            |message| {
                if self
                    .fail_next
                    .swap(false, std::sync::atomic::Ordering::SeqCst)
                {
                    return Err("channel down".to_string());
                }
                self.sent.lock().unwrap().push(message.text.clone());
                Ok(())
            },
            |_| {},
        )
    }
}

#[tokio::test]
async fn failed_away_message_stays_pending_and_is_sent_on_the_next_attempt() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(&directory.0).await.unwrap();
    append_away_message_requested(&store, &pod_message("Pod 3 is blocked"))
        .await
        .unwrap();
    let dispatcher = Arc::new(FlakyAwayDispatcher {
        fail_next: std::sync::atomic::AtomicBool::new(true),
        sent: Mutex::new(Vec::new()),
    });

    let error = handle_next_delivery(&store, dispatcher.clone())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("channel down"));
    assert!(store.next_pending_delivery().await.unwrap().is_some());

    assert!(
        handle_next_delivery(&store, dispatcher.clone())
            .await
            .unwrap()
    );
    assert_eq!(*dispatcher.sent.lock().unwrap(), ["Pod 3 is blocked"]);
    assert!(store.next_pending_delivery().await.unwrap().is_none());
}

#[tokio::test]
async fn away_message_is_staged_and_dispatched_in_stream_version_order() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(&directory.0).await.unwrap();
    append_away_message_requested(&store, &pod_message("first"))
        .await
        .unwrap();
    append_inbound_message(
        &store,
        &InboundMessage {
            text: "second".into(),
            forwarded: false,
        },
    )
    .await
    .unwrap();
    let dispatcher = Arc::new(RecordingDispatcher::default());

    while handle_next_delivery(&store, dispatcher.clone())
        .await
        .unwrap()
    {}

    assert_eq!(
        *dispatcher.event_types.lock().unwrap(),
        ["AwayMessageRequested", "InboundMessageReceived"]
    );
}

#[tokio::test]
async fn malformed_away_message_does_not_block_a_valid_later_delivery() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(&directory.0).await.unwrap();
    store
        .stream()
        .append(
            ExpectedVersion::Any,
            [NewEvent {
                r#type: "AwayMessageRequested".into(),
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
            text: "deliver after malformed message".into(),
            forwarded: false,
        },
    )
    .await
    .unwrap();

    // The real dispatcher is safe here: the payload cannot be read, so nothing is sent.
    assert!(
        handle_next_delivery(&store, Arc::new(system_dispatcher()))
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

#[test]
fn away_message_is_sent_through_the_dispatchers_environment() {
    let environment = Arc::new(FakeEnvironment::locked(""));

    let outcome = dispatcher_with(&environment)
        .dispatch(&away_delivery(json!(pod_message("Pod 3 is blocked"))));

    assert!(matches!(outcome, Ok(DeliveryOutcome::Delivered)));
    assert_eq!(environment.away_messages(), ["Pod 3 is blocked"]);
    assert_eq!(environment.reply_targets(), ["%47"]);
}

#[test]
fn turn_completed_is_sent_through_the_dispatchers_environment() {
    let environment = Arc::new(FakeEnvironment::locked(""));
    let delivery = PendingDelivery {
        event_id: "event-1".into(),
        event_version: events::EventStreamVersion::new(1).unwrap(),
        event_type: "TurnCompleted".into(),
        payload: json!(crate::outbound::fake::turn("%47", "build it")),
        trace_id: "trace".into(),
    };

    let outcome = dispatcher_with(&environment).dispatch(&delivery);

    assert!(matches!(outcome, Ok(DeliveryOutcome::Delivered)));
    assert_eq!(environment.delivered().len(), 1);
    assert_eq!(environment.reply_targets(), ["%47"]);
}

// ---------------------------------------------------------------------------
// Rule A: the dispatcher marks a pane when it sends the pane's message
// ---------------------------------------------------------------------------

fn turn_delivery(pane_id: &str, prompt: &str) -> PendingDelivery {
    PendingDelivery {
        event_id: "event-1".into(),
        event_version: events::EventStreamVersion::new(1).unwrap(),
        event_type: "TurnCompleted".into(),
        payload: json!(crate::outbound::fake::turn(pane_id, prompt)),
        trace_id: "trace".into(),
    }
}

#[test]
fn pane_that_sent_a_message_gets_no_update_for_that_turn_only() {
    let environment = Arc::new(FakeEnvironment::locked(""));
    let dispatcher = dispatcher_with(&environment);

    // pod_message comes from pane %47.
    let sent = dispatcher.dispatch(&away_delivery(json!(pod_message("Pod 3 is blocked"))));
    assert!(matches!(sent, Ok(DeliveryOutcome::Delivered)));
    let update = dispatcher.dispatch(&turn_delivery("%47", "watch the pods"));
    assert!(matches!(update, Ok(DeliveryOutcome::Skipped)));

    assert_eq!(environment.away_messages(), ["Pod 3 is blocked"]);
    assert!(environment.delivered().is_empty());
    // The message made %47 the reply target; the left-out update recorded nothing more.
    assert_eq!(environment.reply_targets(), ["%47"]);

    let next = dispatcher.dispatch(&turn_delivery("%47", "carry on"));
    assert!(matches!(next, Ok(DeliveryOutcome::Delivered)));
    assert_eq!(environment.delivered().len(), 1);
}

#[test]
fn two_messages_in_one_turn_are_both_sent_and_the_update_is_left_out() {
    let environment = Arc::new(FakeEnvironment::locked(""));
    let dispatcher = dispatcher_with(&environment);

    for text in ["first", "second"] {
        let sent = dispatcher.dispatch(&away_delivery(json!(pod_message(text))));
        assert!(matches!(sent, Ok(DeliveryOutcome::Delivered)));
    }
    let update = dispatcher.dispatch(&turn_delivery("%47", "watch the pods"));

    assert!(matches!(update, Ok(DeliveryOutcome::Skipped)));
    assert_eq!(environment.away_messages(), ["first", "second"]);
    assert!(environment.delivered().is_empty());
}

#[test]
fn message_dropped_by_the_pause_leaves_no_mark() {
    let environment = Arc::new(FakeEnvironment::locked(""));
    let dispatcher = dispatcher_with(&environment);

    let dropped = dispatcher
        .dispatch_with_pause(&away_delivery(json!(pod_message("Pod 3 is blocked"))), true);
    assert!(matches!(dropped, Ok(DeliveryOutcome::Skipped)));
    assert!(environment.away_messages().is_empty());

    let update = dispatcher.dispatch(&turn_delivery("%47", "watch the pods"));
    assert!(matches!(update, Ok(DeliveryOutcome::Delivered)));
}

#[test]
fn update_10_minutes_after_the_message_is_sent() {
    let environment = Arc::new(FakeEnvironment::locked(""));
    let dispatcher = dispatcher_with(&environment);

    dispatcher
        .dispatch(&away_delivery(json!(pod_message("Pod 3 is blocked"))))
        .unwrap();
    environment.advance(Duration::from_secs(600));
    let update = dispatcher.dispatch(&turn_delivery("%47", "watch the pods"));

    assert!(matches!(update, Ok(DeliveryOutcome::Delivered)));
}

#[tokio::test]
async fn update_waits_behind_a_message_whose_send_fails_and_is_then_left_out() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(&directory.0).await.unwrap();
    append_away_message_requested(&store, &pod_message("Pod 3 is blocked"))
        .await
        .unwrap();
    append_turn_completed(
        &store,
        &crate::outbound::fake::turn("%47", "watch the pods"),
    )
    .await
    .unwrap();
    let environment = Arc::new(FakeEnvironment::locked(""));
    let dispatcher: Arc<dyn DeliveryDispatcher> = Arc::new(dispatcher_with(&environment));

    environment.set_fail_sends(true);
    let error = handle_next_delivery(&store, dispatcher.clone())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("channel down"));
    assert!(environment.delivered().is_empty());

    environment.set_fail_sends(false);
    while handle_next_delivery(&store, dispatcher.clone())
        .await
        .unwrap()
    {}

    assert_eq!(environment.away_messages(), ["Pod 3 is blocked"]);
    assert!(environment.delivered().is_empty());
    assert!(store.next_pending_delivery().await.unwrap().is_none());
}

// ---------------------------------------------------------------------------
// Rule B: the dispatcher reads the tracked agent panes from the snapshot hub
// ---------------------------------------------------------------------------

const ONLY_FROM_CHIEF: &str = "[notify]\naway_messages_only_from = \"/work/chief-of-pods\"";

fn incarnation(pane_id: &str, pid: u32) -> AgentIncarnation {
    AgentIncarnation {
        pane_id: pane_id.into(),
        pane_pid: pid,
        agent_pid: pid + 1,
        agent_started_at_ms: 1_000,
        provider_id: "codex".into(),
    }
}

fn agent_observed(pane_id: &str, pid: u32, folder: &str) -> AgentEvent {
    AgentEvent::PaneObserved(AgentPaneObserved {
        pane: AgentPaneObservation {
            incarnation: incarnation(pane_id, pid),
            tmux_target: format!("work:0.{pid}"),
            session_name: "work".into(),
            window_index: 0,
            pane_index: pid,
            working_directory: folder.into(),
            provider_display_name: "Codex".into(),
            observed_at_ms: 100,
        },
    })
}

/// A store with `events` appended, a hub over it, and the daemon's dispatcher reading
/// that hub with the screen locked and `overrides` (TOML) laid over the default settings.
async fn dispatcher_over(
    directory: &TestDirectory,
    overrides: &str,
    events: Vec<AgentEvent>,
) -> (
    HaroldStore,
    AgentSnapshotHub,
    Arc<FakeEnvironment>,
    Arc<dyn DeliveryDispatcher>,
) {
    let store = HaroldStore::open(&directory.0).await.unwrap();
    append_agent_events(&store, events).await.unwrap();
    let snapshots = AgentSnapshotHub::new(store.load_agent_snapshot().await.unwrap());
    let environment = Arc::new(FakeEnvironment::locked(overrides));
    // Built the way `run_event_handler` builds it, so these tests cover the daemon's wiring
    // of the hub into the dispatcher.
    let dispatcher: Arc<dyn DeliveryDispatcher> =
        Arc::new(production_dispatcher(environment.clone(), &snapshots));
    (store, snapshots, environment, dispatcher)
}

/// As `dispatcher_over`, with the message folder set to /work/chief-of-pods.
async fn only_from_chief(
    directory: &TestDirectory,
    events: Vec<AgentEvent>,
) -> (
    HaroldStore,
    AgentSnapshotHub,
    Arc<FakeEnvironment>,
    Arc<dyn DeliveryDispatcher>,
) {
    dispatcher_over(directory, ONLY_FROM_CHIEF, events).await
}

/// Pane %1 runs an agent in the update folder, pane %2 runs one in another folder.
fn chief_and_pod() -> Vec<AgentEvent> {
    vec![
        agent_observed("%1", 10, "/work/chief-of-pods"),
        agent_observed("%2", 20, "/work/harold"),
    ]
}

async fn drain(
    store: &HaroldStore,
    snapshots: &AgentSnapshotHub,
    dispatcher: &Arc<dyn DeliveryDispatcher>,
) {
    while handle_next_delivery_inner(store, Arc::clone(dispatcher), Some(snapshots))
        .await
        .unwrap()
    {}
}

#[tokio::test]
async fn update_from_outside_the_update_folder_is_handled_without_a_phone_message() {
    let directory = TestDirectory::new();
    let (store, snapshots, environment, dispatcher) =
        only_from_chief(&directory, chief_and_pod()).await;
    append_turn_completed(&store, &crate::outbound::fake::turn("%2", "build it"))
        .await
        .unwrap();

    drain(&store, &snapshots, &dispatcher).await;

    assert!(environment.delivered().is_empty());
    assert!(environment.reply_targets().is_empty());
    assert!(store.next_pending_delivery().await.unwrap().is_none());
}

#[tokio::test]
async fn update_from_the_update_folder_is_sent() {
    let directory = TestDirectory::new();
    let (store, snapshots, environment, dispatcher) =
        only_from_chief(&directory, chief_and_pod()).await;
    append_turn_completed(&store, &crate::outbound::fake::turn("%1", "report"))
        .await
        .unwrap();

    drain(&store, &snapshots, &dispatcher).await;

    assert_eq!(environment.delivered().len(), 1);
    assert_eq!(environment.reply_targets(), ["%1"]);
}

#[tokio::test]
async fn two_agents_in_the_update_folder_both_send_their_updates() {
    let directory = TestDirectory::new();
    let mut events = chief_and_pod();
    events.push(agent_observed("%3", 30, "/work/chief-of-pods/"));
    let (store, snapshots, environment, dispatcher) = only_from_chief(&directory, events).await;
    for pane_id in ["%1", "%3"] {
        append_turn_completed(&store, &crate::outbound::fake::turn(pane_id, "report"))
            .await
            .unwrap();
    }

    drain(&store, &snapshots, &dispatcher).await;

    assert_eq!(environment.reply_targets(), ["%1", "%3"]);
}

#[tokio::test]
async fn update_is_sent_once_the_agent_in_the_update_folder_has_exited() {
    let directory = TestDirectory::new();
    let mut events = chief_and_pod();
    events.push(AgentEvent::PaneDeparted(AgentPaneDeparted {
        incarnation: incarnation("%1", 10),
        observed_at_ms: 200,
    }));
    let (store, snapshots, environment, dispatcher) = only_from_chief(&directory, events).await;
    append_turn_completed(&store, &crate::outbound::fake::turn("%2", "build it"))
        .await
        .unwrap();

    drain(&store, &snapshots, &dispatcher).await;

    assert_eq!(environment.reply_targets(), ["%2"]);
}

#[tokio::test]
async fn update_is_sent_while_harold_cannot_list_the_tmux_panes() {
    let directory = TestDirectory::new();
    let mut events = chief_and_pod();
    events.push(AgentEvent::MonitorHealthChanged(
        AgentMonitorHealthChanged {
            component: "inventory".into(),
            healthy: false,
            reason_code: "timeout".into(),
            observed_at_ms: 200,
        },
    ));
    let (store, snapshots, environment, dispatcher) = only_from_chief(&directory, events).await;
    append_turn_completed(&store, &crate::outbound::fake::turn("%2", "build it"))
        .await
        .unwrap();

    drain(&store, &snapshots, &dispatcher).await;

    assert_eq!(environment.reply_targets(), ["%2"]);
}

#[tokio::test]
async fn failure_of_another_monitor_component_leaves_the_update_folder_rule_on() {
    let directory = TestDirectory::new();
    let mut events = chief_and_pod();
    events.push(AgentEvent::MonitorHealthChanged(
        AgentMonitorHealthChanged {
            component: "screen".into(),
            healthy: false,
            reason_code: "capture_failed".into(),
            observed_at_ms: 200,
        },
    ));
    let (store, snapshots, environment, dispatcher) = only_from_chief(&directory, events).await;
    append_turn_completed(&store, &crate::outbound::fake::turn("%2", "build it"))
        .await
        .unwrap();

    drain(&store, &snapshots, &dispatcher).await;

    // Harold can still list the panes, so the agent in the update folder counts.
    assert!(environment.delivered().is_empty());
    assert!(environment.reply_targets().is_empty());
    assert!(store.next_pending_delivery().await.unwrap().is_none());
}

#[tokio::test]
async fn agent_that_starts_in_the_update_folder_is_seen_by_the_next_update() {
    let directory = TestDirectory::new();
    let (store, snapshots, environment, dispatcher) =
        only_from_chief(&directory, vec![agent_observed("%2", 20, "/work/harold")]).await;

    // The chief starts after the hub was created: the handler's projection publishes it.
    append_agent_events(
        &store,
        vec![agent_observed("%1", 10, "/work/chief-of-pods")],
    )
    .await
    .unwrap();
    append_turn_completed(&store, &crate::outbound::fake::turn("%2", "build it"))
        .await
        .unwrap();

    drain(&store, &snapshots, &dispatcher).await;

    assert!(environment.delivered().is_empty());
}

// ---------------------------------------------------------------------------
// Rule B for requested messages: a pane outside the folder has its message dropped
// ---------------------------------------------------------------------------

fn message_from(pane_id: &str, text: &str) -> AwayMessageRequested {
    AwayMessageRequested {
        pane_id: pane_id.into(),
        pane_label: format!("work:{pane_id}"),
        text: text.into(),
    }
}

async fn request_message(store: &HaroldStore, pane_id: &str, text: &str) {
    append_away_message_requested(store, &message_from(pane_id, text))
        .await
        .unwrap();
}

fn chief_departed() -> AgentEvent {
    AgentEvent::PaneDeparted(AgentPaneDeparted {
        incarnation: incarnation("%1", 10),
        observed_at_ms: 200,
    })
}

#[tokio::test]
async fn requested_message_from_outside_the_folder_is_dropped() {
    let directory = TestDirectory::new();
    let (store, snapshots, environment, dispatcher) =
        only_from_chief(&directory, chief_and_pod()).await;
    request_message(&store, "%2", "Pod 3 is blocked").await;

    drain(&store, &snapshots, &dispatcher).await;

    assert!(environment.away_messages().is_empty());
    assert!(environment.reply_targets().is_empty());
    assert!(store.next_pending_delivery().await.unwrap().is_none());

    // The drop set no mark. Once the chief has gone the folder rule no longer applies,
    // so only a mark could leave out the pane's turn-end update, and the update is sent.
    append_agent_events(&store, vec![chief_departed()])
        .await
        .unwrap();
    append_turn_completed(&store, &crate::outbound::fake::turn("%2", "build it"))
        .await
        .unwrap();

    drain(&store, &snapshots, &dispatcher).await;

    assert_eq!(environment.delivered().len(), 1);
    assert_eq!(environment.reply_targets(), ["%2"]);
    assert!(environment.away_messages().is_empty());
}

#[tokio::test]
async fn requested_message_from_inside_the_folder_is_sent() {
    let directory = TestDirectory::new();
    let (store, snapshots, environment, dispatcher) =
        only_from_chief(&directory, chief_and_pod()).await;
    request_message(&store, "%1", "All pods are green").await;

    drain(&store, &snapshots, &dispatcher).await;

    assert_eq!(environment.away_messages(), ["All pods are green"]);
    assert_eq!(environment.reply_targets(), ["%1"]);
}

#[tokio::test]
async fn two_agents_in_the_folder_both_send_their_messages() {
    let directory = TestDirectory::new();
    let mut events = chief_and_pod();
    events.push(agent_observed("%3", 30, "/work/chief-of-pods/"));
    let (store, snapshots, environment, dispatcher) = only_from_chief(&directory, events).await;
    request_message(&store, "%1", "first").await;
    request_message(&store, "%3", "second").await;

    drain(&store, &snapshots, &dispatcher).await;

    assert_eq!(environment.away_messages(), ["first", "second"]);
    assert_eq!(environment.reply_targets(), ["%1", "%3"]);
}

#[tokio::test]
async fn requested_message_from_a_subfolder_is_dropped() {
    let directory = TestDirectory::new();
    let mut events = chief_and_pod();
    events.push(agent_observed("%4", 40, "/work/chief-of-pods/sub"));
    let (store, snapshots, environment, dispatcher) = only_from_chief(&directory, events).await;
    request_message(&store, "%4", "Pod 3 is blocked").await;

    drain(&store, &snapshots, &dispatcher).await;

    assert!(environment.away_messages().is_empty());
    assert!(store.next_pending_delivery().await.unwrap().is_none());
}

#[tokio::test]
async fn requested_message_from_outside_the_folder_is_dropped_with_the_screen_unlocked() {
    let directory = TestDirectory::new();
    let (store, snapshots, environment, dispatcher) =
        only_from_chief(&directory, chief_and_pod()).await;
    environment.set_screen_locked(false);
    request_message(&store, "%2", "Pod 3 is blocked").await;

    drain(&store, &snapshots, &dispatcher).await;

    assert!(environment.away_messages().is_empty());
    assert!(environment.reply_targets().is_empty());
    assert!(store.next_pending_delivery().await.unwrap().is_none());
}

#[tokio::test]
async fn requested_message_is_sent_while_no_agent_is_in_the_folder() {
    let directory = TestDirectory::new();
    let (store, snapshots, environment, dispatcher) =
        only_from_chief(&directory, vec![agent_observed("%2", 20, "/work/harold")]).await;
    request_message(&store, "%2", "Pod 3 is blocked").await;

    drain(&store, &snapshots, &dispatcher).await;

    assert_eq!(environment.away_messages(), ["Pod 3 is blocked"]);
    assert_eq!(environment.reply_targets(), ["%2"]);
}

#[tokio::test]
async fn requested_message_is_sent_without_the_setting() {
    let directory = TestDirectory::new();
    let (store, snapshots, environment, dispatcher) =
        dispatcher_over(&directory, "", chief_and_pod()).await;
    request_message(&store, "%2", "Pod 3 is blocked").await;

    drain(&store, &snapshots, &dispatcher).await;

    assert_eq!(environment.away_messages(), ["Pod 3 is blocked"]);
    assert_eq!(environment.reply_targets(), ["%2"]);
}

#[tokio::test]
async fn requested_message_is_sent_while_harold_cannot_list_the_tmux_panes() {
    let directory = TestDirectory::new();
    let mut events = chief_and_pod();
    events.push(AgentEvent::MonitorHealthChanged(
        AgentMonitorHealthChanged {
            component: "inventory".into(),
            healthy: false,
            reason_code: "timeout".into(),
            observed_at_ms: 200,
        },
    ));
    let (store, snapshots, environment, dispatcher) = only_from_chief(&directory, events).await;
    request_message(&store, "%2", "Pod 3 is blocked").await;

    drain(&store, &snapshots, &dispatcher).await;

    assert_eq!(environment.away_messages(), ["Pod 3 is blocked"]);
    assert_eq!(environment.reply_targets(), ["%2"]);
}

#[tokio::test]
async fn message_whose_send_failed_is_dropped_once_an_agent_runs_in_the_folder() {
    let directory = TestDirectory::new();
    let (store, snapshots, environment, dispatcher) =
        only_from_chief(&directory, vec![agent_observed("%2", 20, "/work/harold")]).await;
    request_message(&store, "%2", "Pod 3 is blocked").await;

    // No agent is in the folder, so the message is tried, and the send fails.
    environment.set_fail_sends(true);
    let error = handle_next_delivery_inner(&store, Arc::clone(&dispatcher), Some(&snapshots))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("channel down"));
    assert!(store.next_pending_delivery().await.unwrap().is_some());

    // The chief starts and the channel works again: the next try drops the message.
    environment.set_fail_sends(false);
    append_agent_events(
        &store,
        vec![agent_observed("%1", 10, "/work/chief-of-pods")],
    )
    .await
    .unwrap();

    drain(&store, &snapshots, &dispatcher).await;

    assert!(environment.away_messages().is_empty());
    assert!(environment.reply_targets().is_empty());
    assert!(store.next_pending_delivery().await.unwrap().is_none());
}

#[tokio::test]
async fn message_stored_while_the_chief_ran_is_sent_once_the_chief_has_exited() {
    let directory = TestDirectory::new();
    let (store, snapshots, environment, dispatcher) =
        only_from_chief(&directory, chief_and_pod()).await;
    request_message(&store, "%2", "Pod 3 is blocked").await;
    // The chief exits before the handler comes to the message.
    append_agent_events(&store, vec![chief_departed()])
        .await
        .unwrap();

    drain(&store, &snapshots, &dispatcher).await;

    assert_eq!(environment.away_messages(), ["Pod 3 is blocked"]);
    assert_eq!(environment.reply_targets(), ["%2"]);
}

/// The dispatcher of `only_from_chief` once a handler round has published pane %1 (in the
/// folder) and pane %2 (outside it) to the hub. For tests that read the log: they call
/// `dispatch` on the test thread, the only thread `CapturedLogs` sees.
async fn chief_and_pod_published(
    directory: &TestDirectory,
) -> (Arc<FakeEnvironment>, Arc<dyn DeliveryDispatcher>) {
    let (store, snapshots, environment, dispatcher) =
        only_from_chief(directory, chief_and_pod()).await;
    drain(&store, &snapshots, &dispatcher).await;
    (environment, dispatcher)
}

#[tokio::test]
async fn dropped_message_is_logged_with_its_pane_and_without_its_text() {
    let directory = TestDirectory::new();
    let (environment, dispatcher) = chief_and_pod_published(&directory).await;
    let logs = CapturedLogs::start();

    let outcome = dispatcher.dispatch(&away_delivery(json!(message_from(
        "%2",
        "Pod 3 is blocked"
    ))));

    assert!(matches!(outcome, Ok(DeliveryOutcome::Skipped)));
    assert!(environment.away_messages().is_empty());
    let text = logs.text();
    assert!(
        text.contains(
            "away message dropped (messages come from /work/chief-of-pods only) pane_id=%2"
        ),
        "{text}"
    );
    assert!(!text.contains("Pod 3 is blocked"), "{text}");
}

#[tokio::test]
async fn update_after_a_dropped_message_is_left_out_by_the_folder_rule_not_the_mark() {
    let directory = TestDirectory::new();
    let (environment, dispatcher) = chief_and_pod_published(&directory).await;
    let logs = CapturedLogs::start();

    let dropped = dispatcher.dispatch(&away_delivery(json!(message_from(
        "%2",
        "Pod 3 is blocked"
    ))));
    let update = dispatcher.dispatch(&turn_delivery("%2", "build it"));

    assert!(matches!(dropped, Ok(DeliveryOutcome::Skipped)));
    assert!(matches!(update, Ok(DeliveryOutcome::Skipped)));
    assert!(environment.delivered().is_empty());
    assert!(environment.reply_targets().is_empty());
    let text = logs.text();
    assert!(
        text.contains(
            "notification skipped (updates come from /work/chief-of-pods only) pane_id=%2"
        ),
        "{text}"
    );
    assert!(
        !text.contains("pane messaged the phone this turn"),
        "{text}"
    );
}

#[tokio::test]
async fn paused_message_from_outside_the_folder_is_skipped_by_the_pause() {
    let directory = TestDirectory::new();
    let (environment, dispatcher) = chief_and_pod_published(&directory).await;
    let logs = CapturedLogs::start();

    crate::outbound::set_messaging_paused(true);
    let outcome = dispatcher.dispatch(&away_delivery(json!(message_from(
        "%2",
        "Pod 3 is blocked"
    ))));
    crate::outbound::set_messaging_paused(false);

    assert!(matches!(outcome, Ok(DeliveryOutcome::Skipped)));
    assert!(environment.away_messages().is_empty());
    let text = logs.text();
    assert!(
        text.contains("away message skipped (messaging paused)"),
        "{text}"
    );
    assert!(!text.contains("away message dropped"), "{text}");
}

#[tokio::test]
async fn agent_in_the_folder_that_sent_a_message_gets_no_update_for_that_turn() {
    let directory = TestDirectory::new();
    let (environment, dispatcher) = chief_and_pod_published(&directory).await;
    let logs = CapturedLogs::start();

    let sent = dispatcher.dispatch(&away_delivery(json!(message_from(
        "%1",
        "All pods are green"
    ))));
    let update = dispatcher.dispatch(&turn_delivery("%1", "watch the pods"));

    assert!(matches!(sent, Ok(DeliveryOutcome::Delivered)));
    assert!(matches!(update, Ok(DeliveryOutcome::Skipped)));
    assert_eq!(environment.away_messages(), ["All pods are green"]);
    assert!(environment.delivered().is_empty());
    assert_eq!(environment.reply_targets(), ["%1"]);
    let text = logs.text();
    assert!(
        text.contains("notification skipped (pane messaged the phone this turn) pane_id=%1"),
        "{text}"
    );
    assert!(!text.contains("away message dropped"), "{text}");
}
