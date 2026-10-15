use std::sync::{Arc, Mutex};
use std::time::Duration;

use events::{ActorType, ExpectedVersion, NewEvent, WorkflowRef};
use serde_json::json;

use super::{
    DeliveryDispatcher, DispatchError, ProductionDispatcher, ReplyOrigin, Router,
    dispatch_away_message, dispatch_spoken_line, handle_next_delivery, handle_next_delivery_inner,
    production_dispatcher, project_and_publish_agent_snapshot, route_message, run_event_handler,
    stored_envelope,
};
use crate::agent::domain::{
    AgentEvent, AgentIncarnation, AgentMonitorHealthChanged, AgentPaneDeparted,
    AgentPaneObservation, AgentPaneObserved, CompletionSummaryUpdate,
};
use crate::agent::snapshot::AgentSnapshotHub;
use crate::inbound::AgentAddress;
use crate::inbound::RouteReport;
use crate::outbound::fake::{CapturedLogs, FakeEnvironment};
use crate::outbound::{DeliveryOutcome, SystemEnvironment};
use crate::store::{
    AwayMessageRequested, HaroldStore, InboundMessage, PendingDelivery, SpokenLineRequested,
    TurnCompleted, append_agent_events, append_away_message_requested, append_inbound_message,
    append_messaging_paused_changed, append_spoken_line_requested, append_turn_completed,
};
use crate::telemetry::{self, ResolvedBy, Routing};

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

    let outcome = system_dispatcher().dispatch_with_pause(&delivery, None, true);
    assert!(matches!(outcome, Ok(DeliveryOutcome::Skipped)));

    let outcome = system_dispatcher().dispatch_with_pause(&delivery, None, false);
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

        let outcome = route_message(&message, "event-1", None, |text, flag| {
            routed = Some((text.to_string(), flag));
            Ok(not_owned_report())
        });

        assert!(matches!(outcome, Ok(DeliveryOutcome::Skipped)));
        assert_eq!(routed, Some(("[studio] carry on".to_string(), forwarded)));
    }
}

#[test]
fn routing_failure_of_a_stored_reply_is_retryable() {
    let message = InboundMessage {
        text: "carry on".into(),
        forwarded: false,
    };

    let outcome = route_message(&message, "event-1", None, |_, _| {
        Err("tmux failed".to_string())
    });

    assert!(matches!(outcome, Err(DispatchError::Retryable(error)) if error == "tmux failed"));
}

/// What routing returns for a reply another Mac owns. It is not counted, so these tests
/// leave the process-wide phone book as it was.
fn not_owned_report() -> RouteReport {
    RouteReport {
        outcome: DeliveryOutcome::Skipped,
        routing: Routing::NotOwned,
    }
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

    let outcome = system_dispatcher().dispatch_with_pause(&delivery, None, true);
    assert!(matches!(outcome, Ok(DeliveryOutcome::Skipped)));

    let outcome = system_dispatcher().dispatch_with_pause(&delivery, None, false);
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

    let dropped = dispatcher.dispatch_with_pause(
        &away_delivery(json!(pod_message("Pod 3 is blocked"))),
        None,
        true,
    );
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

// ---------------------------------------------------------------------------
// Spoken lines: away device when the screen is locked, the desk otherwise
// ---------------------------------------------------------------------------

fn spoken_delivery(payload: serde_json::Value) -> PendingDelivery {
    PendingDelivery {
        event_id: "event-1".into(),
        event_version: events::EventStreamVersion::new(1).unwrap(),
        event_type: "SpokenLineRequested".into(),
        payload,
        trace_id: "trace".into(),
    }
}

fn chief_line(text: &str) -> SpokenLineRequested {
    SpokenLineRequested {
        pane_id: "%1".into(),
        text: text.into(),
    }
}

fn dispatch_line(
    environment: &FakeEnvironment,
    line: &SpokenLineRequested,
    messaging_paused: bool,
    voice_muted: bool,
) -> Result<DeliveryOutcome, DispatchError> {
    dispatch_spoken_line(
        &spoken_delivery(json!(line)),
        messaging_paused,
        voice_muted,
        |_| None,
        environment,
    )
}

#[test]
fn locked_screen_sends_the_line_to_the_away_device_and_logs_no_text() {
    let environment = FakeEnvironment::locked("");
    let logs = CapturedLogs::start();

    let outcome = dispatch_line(
        &environment,
        &chief_line("All pods are green."),
        false,
        false,
    );

    assert!(matches!(outcome, Ok(DeliveryOutcome::Delivered)));
    assert_eq!(environment.voice_notes(), ["All pods are green."]);
    assert!(environment.spoken_lines().is_empty());
    assert!(
        environment.reply_targets().is_empty(),
        "a voice note sets no reply target"
    );
    assert!(
        logs.text()
            .contains("spoken line sent to the away device pane_id=%1")
    );
    assert!(!logs.text().contains("All pods are green."));
}

#[test]
fn locked_screen_with_messaging_paused_drops_the_line() {
    let environment = FakeEnvironment::locked("");
    let logs = CapturedLogs::start();

    let outcome = dispatch_line(&environment, &chief_line("hi"), true, false);

    assert!(matches!(outcome, Ok(DeliveryOutcome::Skipped)));
    assert!(environment.voice_notes().is_empty());
    assert!(
        logs.text()
            .contains("spoken line dropped (messaging paused) pane_id=%1")
    );
}

#[test]
fn unlocked_screen_with_voice_on_says_exactly_the_line() {
    let environment = FakeEnvironment::new("");

    let outcome = dispatch_line(
        &environment,
        &chief_line("Release install is done."),
        true,
        false,
    );

    assert!(matches!(outcome, Ok(DeliveryOutcome::Delivered)));
    assert_eq!(environment.spoken_lines(), ["Release install is done."]);
    assert!(environment.voice_notes().is_empty());
}

#[test]
fn unlocked_screen_with_voice_off_drops_the_line_and_runs_nothing() {
    let environment = FakeEnvironment::new("");
    let logs = CapturedLogs::start();

    let outcome = dispatch_line(&environment, &chief_line("hi"), false, true);

    assert!(matches!(outcome, Ok(DeliveryOutcome::Skipped)));
    assert!(environment.spoken_lines().is_empty());
    assert!(environment.voice_notes().is_empty());
    assert!(
        logs.text()
            .contains("spoken line dropped (voice off) pane_id=%1")
    );
}

#[test]
fn failed_voice_note_is_retried_and_said_at_the_desk_once_unlocked() {
    let environment = FakeEnvironment::locked("");
    environment.set_fail_sends(true);
    let line = chief_line("hi");

    let first = dispatch_line(&environment, &line, false, false);
    assert!(matches!(first, Err(DispatchError::Retryable(_))));

    environment.set_fail_sends(false);
    environment.set_screen_locked(false);
    let retry = dispatch_line(&environment, &line, false, false);
    assert!(matches!(retry, Ok(DeliveryOutcome::Delivered)));
    assert_eq!(environment.spoken_lines(), ["hi"]);
    assert!(environment.voice_notes().is_empty());
}

#[test]
fn failed_speak_at_the_desk_is_retried_and_says_nothing() {
    let environment = FakeEnvironment::new("");
    environment.set_fail_sends(true);

    let outcome = dispatch_line(&environment, &chief_line("hi"), false, false);

    assert!(matches!(outcome, Err(DispatchError::Retryable(_))));
    assert!(environment.spoken_lines().is_empty());
}

#[test]
fn malformed_spoken_line_payload_is_a_terminal_failure() {
    let environment = FakeEnvironment::locked("");
    let outcome = dispatch_spoken_line(
        &spoken_delivery(json!({"pane_id": 1})),
        false,
        false,
        |_| None,
        &environment,
    );
    assert!(
        matches!(outcome, Err(DispatchError::Permanent(error)) if error.starts_with("invalid SpokenLineRequested payload"))
    );
}

#[tokio::test]
async fn spoken_line_from_outside_the_chiefs_folder_is_dropped_without_its_text() {
    let directory = TestDirectory::new();
    let (store, snapshots, environment, dispatcher) =
        only_from_chief(&directory, chief_and_pod()).await;
    append_spoken_line_requested(
        &store,
        &SpokenLineRequested {
            pane_id: "%2".into(),
            text: "secret words".into(),
        },
    )
    .await
    .unwrap();
    append_spoken_line_requested(&store, &chief_line("All pods are green."))
        .await
        .unwrap();

    drain(&store, &snapshots, &dispatcher).await;

    // The screen is locked in `dispatcher_over`: the chief's line becomes a voice note.
    assert_eq!(environment.voice_notes(), ["All pods are green."]);
    assert!(store.next_pending_delivery().await.unwrap().is_none());
}

#[test]
fn dropped_spoken_line_logs_the_reason_and_the_pane_only() {
    let environment = FakeEnvironment::locked("");
    let logs = CapturedLogs::start();
    let line = SpokenLineRequested {
        pane_id: "%2".into(),
        text: "secret words".into(),
    };

    let outcome = dispatch_spoken_line(
        &spoken_delivery(json!(line)),
        false,
        false,
        |_| Some("spoken line dropped (lines come from /work/chief-of-pods only)".into()),
        &environment,
    );

    assert!(matches!(outcome, Ok(DeliveryOutcome::Skipped)));
    assert!(environment.voice_notes().is_empty());
    assert!(
        logs.text()
            .contains("spoken line dropped (lines come from /work/chief-of-pods only) pane_id=%2")
    );
    assert!(!logs.text().contains("secret words"));
}

// ---------------------------------------------------------------------------
// Phone requests
// ---------------------------------------------------------------------------

/// The daemon's dispatcher, never paused: other tests flip the process-wide pause switch.
struct Unpaused(ProductionDispatcher);

impl DeliveryDispatcher for Unpaused {
    fn dispatch(&self, delivery: &PendingDelivery) -> Result<DeliveryOutcome, DispatchError> {
        self.0.dispatch_with_pause(delivery, None, false)
    }

    fn dispatch_reply(
        &self,
        delivery: &PendingDelivery,
        origin: &ReplyOrigin,
    ) -> Result<DeliveryOutcome, DispatchError> {
        self.0.dispatch_with_pause(delivery, Some(origin), false)
    }
}

/// A store in a test directory that is removed with it.
struct Fixture {
    store: HaroldStore,
    _directory: TestDirectory,
}

/// A store and the daemon's dispatcher with `router` in place of tmux routing.
async fn fixture_with_router(router: Router) -> (Fixture, Arc<dyn DeliveryDispatcher>) {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(&directory.0).await.unwrap();
    let dispatcher =
        ProductionDispatcher::new(Arc::new(SystemEnvironment), None).with_router(router);
    (
        Fixture {
            store,
            _directory: directory,
        },
        Arc::new(Unpaused(dispatcher)),
    )
}

/// A pane id no other test uses.
fn unique_pane() -> String {
    format!("%t{}", uuid::Uuid::new_v4().simple())
}

/// A channel name no other test uses, so this test's metrics are its own.
fn unique_channel() -> &'static str {
    Box::leak(format!("imessage-{}", uuid::Uuid::new_v4().simple()).into_boxed_str())
}

/// The report of a reply Harold typed into `pane`, choosing it as the latest notifier.
fn typed_into(pane: &str) -> RouteReport {
    RouteReport {
        outcome: DeliveryOutcome::Delivered,
        routing: Routing::Typed {
            pane_id: pane.to_string(),
            resolved_by: ResolvedBy::LatestNotifier,
            tagged: false,
            typed_at: time::OffsetDateTime::now_utc(),
        },
    }
}

/// A router that types every reply into `pane`.
fn typing_router(pane: &str) -> Router {
    let pane = pane.to_string();
    Arc::new(move |_, _| Ok(typed_into(&pane)))
}

/// Stores a reply as the listener does: the request opens at `reply_at`, and the reply is
/// stored inside `harold.reply_stored`, so the stored event records the request's trace.
async fn store_reply_as_listener(
    store: &HaroldStore,
    channel: &'static str,
    reply_at: time::OffsetDateTime,
    text: &str,
) {
    let message = InboundMessage {
        text: text.into(),
        forwarded: false,
    };
    let stored = telemetry::store_phone_reply(channel, reply_at, async {
        append_inbound_message(store, &message).await.is_ok()
    })
    .await;
    assert!(stored);
}

/// The first stored event's trace id and stored time, read as the event handler reads
/// them.
async fn first_envelope(store: &HaroldStore) -> ReplyOrigin {
    let (trace_id, created_at) =
        stored_envelope(store, events::EventStreamVersion::new(1).unwrap())
            .await
            .expect("the first event can be read");
    ReplyOrigin {
        trace_id,
        created_at,
    }
}

/// The exported spans called `name` in the trace with id `trace_id`.
fn spans_in_trace(name: &str, trace_id: &str) -> Vec<opentelemetry_sdk::trace::SpanData> {
    telemetry::test_support::spans()
        .into_iter()
        .filter(|span| span.name == name && span.span_context.trace_id().to_string() == trace_id)
        .collect()
}

/// The one exported `harold.phone_request` span that typed into `pane`.
fn phone_request_typed_into(pane: &str) -> opentelemetry_sdk::trace::SpanData {
    let mut found: Vec<_> = telemetry::test_support::spans()
        .into_iter()
        .filter(|span| {
            span.name == "harold.phone_request"
                && telemetry::test_support::span_attr(span, "pane_id").as_deref() == Some(pane)
        })
        .collect();
    assert_eq!(found.len(), 1, "phone requests typed into {pane}");
    found.remove(0)
}

fn attr(span: &opentelemetry_sdk::trace::SpanData, key: &str) -> String {
    telemetry::test_support::span_attr(span, key).unwrap_or_default()
}

#[tokio::test]
async fn a_routed_reply_records_typed_with_latency_from_its_own_time() {
    telemetry::test_support::install();
    let pane = unique_pane();
    let channel = unique_channel();
    let (fixture, dispatcher) = fixture_with_router(typing_router(&pane)).await;
    let sent = time::OffsetDateTime::now_utc() - time::Duration::seconds(8);
    store_reply_as_listener(&fixture.store, channel, sent, "hi").await;

    assert!(
        handle_next_delivery(&fixture.store, dispatcher)
            .await
            .unwrap()
    );

    let span = phone_request_typed_into(&pane);
    assert_eq!(attr(&span, "outcome"), "typed");
    assert_eq!(attr(&span, "channel"), channel);
    assert_eq!(attr(&span, "resolved_by"), "latest_notifier");
    assert_eq!(span.start_time, std::time::SystemTime::from(sent));
    let took = span.end_time.duration_since(span.start_time).unwrap();
    assert!(took >= Duration::from_secs(8), "{took:?}");
    let latency = telemetry::test_support::histogram_sum(
        "harold.phone_request.latency",
        &[("channel", channel), ("resolved_by", "latest_notifier")],
    )
    .expect("the latency is recorded");
    assert!(latency >= 8.0, "{latency}");
    assert_eq!(
        telemetry::test_support::counter_value(
            "harold.phone_requests",
            &[
                ("channel", channel),
                ("outcome", "typed"),
                ("resolved_by", "latest_notifier"),
            ],
        ),
        Some(1)
    );
    let trace_id = span.span_context.trace_id().to_string();
    let routes = spans_in_trace("harold.route", &trace_id);
    assert_eq!(routes.len(), 1);
    assert_eq!(attr(&routes[0], "resolved_by"), "latest_notifier");
    assert_eq!(
        first_envelope(&fixture.store).await.trace_id.as_deref(),
        Some(trace_id.as_str())
    );
    assert!(
        telemetry::open_reply_for(&trace_id).is_none(),
        "a finished request is no longer open"
    );
}

#[tokio::test]
async fn a_reply_with_no_pane_records_unavailable() {
    telemetry::test_support::install();
    let channel = unique_channel();
    let router: Router = Arc::new(|_, _| {
        Ok(RouteReport {
            outcome: DeliveryOutcome::Skipped,
            routing: Routing::NoPane,
        })
    });
    let (fixture, dispatcher) = fixture_with_router(router).await;
    store_reply_as_listener(
        &fixture.store,
        channel,
        time::OffsetDateTime::now_utc(),
        "hi",
    )
    .await;

    assert!(
        handle_next_delivery(&fixture.store, dispatcher)
            .await
            .unwrap()
    );

    let trace_id = first_envelope(&fixture.store).await.trace_id.unwrap();
    let requests = spans_in_trace("harold.phone_request", &trace_id);
    assert_eq!(requests.len(), 1);
    assert_eq!(attr(&requests[0], "outcome"), "unavailable");
    assert_eq!(attr(&requests[0], "resolved_by"), "none");
    assert_eq!(
        telemetry::test_support::counter_value(
            "harold.phone_requests",
            &[
                ("channel", channel),
                ("outcome", "unavailable"),
                ("resolved_by", "none"),
            ],
        ),
        Some(1)
    );
    assert_eq!(
        telemetry::test_support::histogram_sum(
            "harold.phone_request.latency",
            &[("channel", channel), ("resolved_by", "none")],
        ),
        None,
        "a reply that was never typed has no latency"
    );
}

#[tokio::test]
async fn a_relay_that_fails_then_succeeds_is_one_request() {
    telemetry::test_support::install();
    let pane = unique_pane();
    let channel = unique_channel();
    let failed_once = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let router: Router = {
        let pane = pane.clone();
        let failed_once = Arc::clone(&failed_once);
        Arc::new(move |_, _| {
            if !failed_once.swap(true, std::sync::atomic::Ordering::SeqCst) {
                return Err("tmux failed".to_string());
            }
            Ok(typed_into(&pane))
        })
    };
    let (fixture, dispatcher) = fixture_with_router(router).await;
    store_reply_as_listener(
        &fixture.store,
        channel,
        time::OffsetDateTime::now_utc(),
        "hi",
    )
    .await;
    let trace_id = first_envelope(&fixture.store).await.trace_id.unwrap();

    let error = handle_next_delivery(&fixture.store, Arc::clone(&dispatcher))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("tmux failed"));
    assert!(
        telemetry::open_reply_for(&trace_id).is_some(),
        "the request stays open for the retry"
    );
    assert!(spans_in_trace("harold.phone_request", &trace_id).is_empty());

    assert!(
        handle_next_delivery(&fixture.store, dispatcher)
            .await
            .unwrap()
    );

    let requests = spans_in_trace("harold.phone_request", &trace_id);
    assert_eq!(requests.len(), 1);
    assert_eq!(attr(&requests[0], "outcome"), "typed");
    assert_eq!(attr(&requests[0], "pane_id"), pane);
    assert_eq!(spans_in_trace("harold.route", &trace_id).len(), 2);
    assert_eq!(
        telemetry::test_support::counter_value(
            "harold.phone_requests",
            &[
                ("channel", channel),
                ("outcome", "typed"),
                ("resolved_by", "latest_notifier"),
            ],
        ),
        Some(1)
    );
}

#[tokio::test]
async fn a_reply_with_no_open_request_starts_at_its_event_time() {
    telemetry::test_support::install();
    let pane = unique_pane();
    let (fixture, dispatcher) = fixture_with_router(typing_router(&pane)).await;
    // Stored outside any span, as a member stores a reply its hub forwarded.
    append_inbound_message(
        &fixture.store,
        &InboundMessage {
            text: "carry on".into(),
            forwarded: true,
        },
    )
    .await
    .unwrap();
    let envelope = first_envelope(&fixture.store).await;
    assert_eq!(envelope.trace_id, None);

    assert!(
        handle_next_delivery(&fixture.store, dispatcher)
            .await
            .unwrap()
    );

    let span = phone_request_typed_into(&pane);
    assert_eq!(attr(&span, "channel"), "forwarded");
    assert_eq!(attr(&span, "outcome"), "typed");
    assert_eq!(
        span.start_time,
        std::time::SystemTime::from(envelope.created_at)
    );
    let trace_id = span.span_context.trace_id().to_string();
    assert_eq!(spans_in_trace("harold.route", &trace_id).len(), 1);
}

#[tokio::test]
async fn a_reply_that_is_not_forwarded_and_has_no_open_request_is_on_the_unknown_channel() {
    telemetry::test_support::install();
    let pane = unique_pane();
    let (fixture, dispatcher) = fixture_with_router(typing_router(&pane)).await;
    append_inbound_message(
        &fixture.store,
        &InboundMessage {
            text: "carry on".into(),
            forwarded: false,
        },
    )
    .await
    .unwrap();

    assert!(
        handle_next_delivery(&fixture.store, dispatcher)
            .await
            .unwrap()
    );

    assert_eq!(attr(&phone_request_typed_into(&pane), "channel"), "unknown");
}

#[tokio::test]
async fn a_failed_relay_of_a_reply_with_no_open_request_is_still_one_request() {
    telemetry::test_support::install();
    let pane = unique_pane();
    let failed_once = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let router: Router = {
        let pane = pane.clone();
        let failed_once = Arc::clone(&failed_once);
        Arc::new(move |_, _| {
            if !failed_once.swap(true, std::sync::atomic::Ordering::SeqCst) {
                return Err("tmux failed".to_string());
            }
            Ok(typed_into(&pane))
        })
    };
    let (fixture, dispatcher) = fixture_with_router(router).await;
    append_inbound_message(
        &fixture.store,
        &InboundMessage {
            text: "carry on".into(),
            forwarded: true,
        },
    )
    .await
    .unwrap();
    let envelope = first_envelope(&fixture.store).await;

    assert!(
        handle_next_delivery(&fixture.store, Arc::clone(&dispatcher))
            .await
            .is_err()
    );
    assert!(
        handle_next_delivery(&fixture.store, dispatcher)
            .await
            .unwrap()
    );

    let span = phone_request_typed_into(&pane);
    let trace_id = span.span_context.trace_id().to_string();
    assert_eq!(spans_in_trace("harold.phone_request", &trace_id).len(), 1);
    assert_eq!(spans_in_trace("harold.route", &trace_id).len(), 2);
    assert_eq!(
        span.start_time,
        std::time::SystemTime::from(envelope.created_at)
    );
}

#[tokio::test]
async fn a_paused_reply_finishes_its_open_request_without_counting_it() {
    telemetry::test_support::install();
    let channel = unique_channel();
    let directory = TestDirectory::new();
    let store = HaroldStore::open(&directory.0).await.unwrap();
    store_reply_as_listener(&store, channel, time::OffsetDateTime::now_utc(), "hi").await;
    let envelope = first_envelope(&store).await;
    let trace_id = envelope.trace_id.clone().unwrap();
    store.project_unhandled_events(10).await.unwrap();
    let delivery = store.next_pending_delivery().await.unwrap().unwrap();
    let outcome = system_dispatcher().dispatch_with_pause(&delivery, Some(&envelope), true);

    assert!(matches!(outcome, Ok(DeliveryOutcome::Skipped)));
    let requests = spans_in_trace("harold.phone_request", &trace_id);
    assert_eq!(requests.len(), 1);
    assert_eq!(attr(&requests[0], "outcome"), "paused");
    assert!(telemetry::open_reply_for(&trace_id).is_none());
    assert_eq!(
        telemetry::test_support::counter_value("harold.phone_requests", &[("channel", channel)]),
        None,
        "a paused reply is not counted"
    );
}
