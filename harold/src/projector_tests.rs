use std::cell::Cell;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;

use crate::settings::Channel;

use super::{
    ChiefTyper, DeliveryDispatcher, DispatchError, ProductionDispatcher, ReplyOrigin, Router,
    dispatch_away_message, dispatch_spoken_line, dispatch_spoken_line_with_app,
    handle_next_delivery, handle_next_delivery_inner, production_dispatcher,
    project_and_publish_agent_snapshot, route_message, run_event_handler, stored_envelope,
};
use crate::agent::domain::{
    AgentEvent, AgentIncarnation, AgentMonitorHealthChanged, AgentPaneDeparted,
    AgentPaneObservation, AgentPaneObserved, CompletionSummaryUpdate,
};
use crate::agent::snapshot::AgentSnapshotHub;
use crate::inbound::AgentAddress;
use crate::inbound::RouteReport;
use crate::inbound::ownership::agent_message_text;
use crate::outbound::fake::{CapturedLogs, FakeEnvironment};
use crate::outbound::notify_tests::{FAKE_MACHINE, FakeApp, FakeAwayText};
use crate::outbound::{AppDelivery, AppRoute, NotPushedReason, ChiefApp, DeliveryOutcome, SystemEnvironment};
use crate::store::{
    AwayMessageRequested, EventVersion, HaroldStore, InboundMessage, NewEvent, PendingDelivery,
    SpokenLineRequested, TurnCompleted, WriteFault, append_agent_events,
    append_away_message_requested, append_inbound_message, append_messaging_paused_changed,
    append_spoken_line_requested, append_turn_completed,
};
use crate::telemetry::{self, ResolvedBy, Routing};
use crate::voice_input::{Speaker, say_in_phone_call_over};

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

/// Fails each delivery whose `event_type` is `failing_type`, retryably, and delivers the
/// rest. Records every event type it is handed.
struct FailsOneType {
    failing_type: &'static str,
    handed: Mutex<Vec<String>>,
}

impl FailsOneType {
    fn new(failing_type: &'static str) -> Self {
        Self {
            failing_type,
            handed: Mutex::new(Vec::new()),
        }
    }
}

impl DeliveryDispatcher for FailsOneType {
    fn dispatch(&self, delivery: &PendingDelivery) -> Result<DeliveryOutcome, DispatchError> {
        self.handed
            .lock()
            .unwrap()
            .push(delivery.event_type.clone());
        if delivery.event_type == self.failing_type {
            return Err(DispatchError::Retryable(
                "osascript said: secret text".into(),
            ));
        }
        Ok(DeliveryOutcome::Delivered)
    }
}

/// A turn end and then a reply, stored in that order.
async fn a_turn_end_then_a_reply(store: &HaroldStore) {
    append_turn_completed(
        store,
        &TurnCompleted {
            pane_id: "%4".into(),
            pane_label: "harold:0.4".into(),
            last_user_prompt: "Run the task".into(),
            assistant_message: "private answer".into(),
            main_context: "harold".into(),
            agent_incarnation: None,
            work_summary: CompletionSummaryUpdate::Unchanged,
        },
    )
    .await
    .unwrap();
    append_inbound_message(
        store,
        &InboundMessage {
            text: "continue".into(),
            forwarded: false,
        },
    )
    .await
    .unwrap();
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
    let task = tokio::spawn(run_event_handler(store, snapshots, None, shutdown_rx));

    drop(shutdown_tx);

    tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("handler did not stop")
        .expect("handler task panicked");
}

/// Records when each delivery was handed to it, by event type. Fails `failing_type`
/// retryably. Says on `done` when another type is delivered, or when `failing_type` is
/// handed an 11th time (a give-up that never came).
struct TimedDispatcher {
    failing_type: &'static str,
    handed: Mutex<Vec<(String, tokio::time::Instant)>>,
    done: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    first_failure: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

impl TimedDispatcher {
    fn new(
        failing_type: &'static str,
    ) -> (
        Arc<Self>,
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Receiver<()>,
    ) {
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let (first_tx, first_rx) = tokio::sync::oneshot::channel();
        let dispatcher = Arc::new(Self {
            failing_type,
            handed: Mutex::new(Vec::new()),
            done: Mutex::new(Some(done_tx)),
            first_failure: Mutex::new(Some(first_tx)),
        });
        (dispatcher, done_rx, first_rx)
    }

    fn handed(&self) -> Vec<(String, tokio::time::Instant)> {
        self.handed.lock().unwrap().clone()
    }
}

impl DeliveryDispatcher for TimedDispatcher {
    fn dispatch(&self, delivery: &PendingDelivery) -> Result<DeliveryOutcome, DispatchError> {
        let mut handed = self.handed.lock().unwrap();
        handed.push((delivery.event_type.clone(), tokio::time::Instant::now()));
        let failing = delivery.event_type == self.failing_type;
        let tries = handed
            .iter()
            .filter(|(kind, _)| kind == self.failing_type)
            .count();
        if let Some(first) = self.first_failure.lock().unwrap().take() {
            let _ = first.send(());
        }
        if (!failing || tries > 10)
            && let Some(done) = self.done.lock().unwrap().take()
        {
            let _ = done.send(());
        }
        if failing {
            return Err(DispatchError::Retryable("send failed".into()));
        }
        Ok(DeliveryOutcome::Delivered)
    }
}

#[tokio::test(start_paused = true)]
async fn a_failing_delivery_waits_doubling_times_then_the_queue_moves_on() {
    let directory = TestDirectory::new();
    let store = Arc::new(HaroldStore::open(&directory.0).await.unwrap());
    a_turn_end_then_a_reply(&store).await;
    let (dispatcher, done, _first) = TimedDispatcher::new("TurnCompleted");
    let (alerts, mut alert_rx) = crate::alerts::AlertQueue::new();
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
    let task = tokio::spawn(super::run_deliveries(
        Arc::clone(&store),
        dispatcher.clone(),
        None,
        Some(alerts),
        shutdown_rx,
    ));

    done.await.unwrap();
    drop(shutdown_tx);
    task.await.unwrap();

    let handed = dispatcher.handed();
    let kinds: Vec<&str> = handed.iter().map(|(kind, _)| kind.as_str()).collect();
    // Ten tries of the turn end, then the reply: nothing later is tried meanwhile.
    assert_eq!(kinds[..10], ["TurnCompleted"; 10]);
    assert_eq!(kinds[10..], ["InboundMessageReceived"]);
    let gaps: Vec<Duration> = handed
        .windows(2)
        .take(9)
        .map(|pair| pair[1].1 - pair[0].1)
        .collect();
    for (gap, expected) in gaps.iter().zip([1, 2, 4, 8, 16, 32, 60, 60, 60]) {
        let expected = Duration::from_secs(expected);
        assert!(
            *gap >= expected && *gap < expected + Duration::from_millis(200),
            "gaps {gaps:?}"
        );
    }
    // The reply goes at once after the give-up, not after another wait.
    assert!(handed[10].1 - handed[9].1 < Duration::from_millis(200));
    let (line, _) = alert_rx.try_recv().unwrap();
    assert_eq!(line, "harold alert: delivery-given-up firing");
}

#[tokio::test(start_paused = true)]
async fn snapshots_keep_publishing_while_a_row_waits() {
    let directory = TestDirectory::new();
    let store = Arc::new(HaroldStore::open(&directory.0).await.unwrap());
    let hub = AgentSnapshotHub::new(store.load_agent_snapshot().await.unwrap());
    let mut receiver = hub.subscribe();
    a_turn_end_then_a_reply(&store).await;
    let (dispatcher, _done, first_failure) = TimedDispatcher::new("TurnCompleted");
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
    let task = tokio::spawn(super::run_deliveries(
        Arc::clone(&store),
        dispatcher.clone(),
        Some(hub.clone()),
        None,
        shutdown_rx,
    ));
    first_failure.await.unwrap();
    let _ = receiver.borrow_and_update();

    append_agent_events(&store, vec![agent_observed("%42", 420, "/work/harold")])
        .await
        .unwrap();
    receiver.changed().await.unwrap();

    // Published during the first wait: the turn end has had one try only.
    assert_eq!(dispatcher.handed().len(), 1);
    assert!(
        receiver
            .borrow()
            .panes
            .iter()
            .any(|pane| pane.pane.incarnation.pane_id == "%42")
    );
    drop(shutdown_tx);
    task.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn shutdown_ends_a_retry_wait_at_once() {
    let directory = TestDirectory::new();
    let store = Arc::new(HaroldStore::open(&directory.0).await.unwrap());
    let hub = AgentSnapshotHub::new(store.load_agent_snapshot().await.unwrap());
    let mut receiver = hub.subscribe();
    a_turn_end_then_a_reply(&store).await;
    let (dispatcher, _done, first_failure) = TimedDispatcher::new("TurnCompleted");
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
    let task = tokio::spawn(super::run_deliveries(
        Arc::clone(&store),
        dispatcher.clone(),
        Some(hub.clone()),
        None,
        shutdown_rx,
    ));
    first_failure.await.unwrap();
    let failed_at = dispatcher.handed()[0].1;
    let _ = receiver.borrow_and_update();
    // A snapshot published after the failure shows the handler has reached its wait, so
    // the shutdown below ends the wait and not the cycle before it.
    append_agent_events(&store, vec![agent_observed("%42", 420, "/work/harold")])
        .await
        .unwrap();
    receiver.changed().await.unwrap();
    assert_eq!(dispatcher.handed().len(), 1);

    drop(shutdown_tx);
    task.await.unwrap();

    assert!(tokio::time::Instant::now() - failed_at < Duration::from_secs(1));
    assert_eq!(dispatcher.handed().len(), 1);
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

#[test]
fn waits_double_from_one_second_to_a_minute() {
    let waits: Vec<u64> = (1..=9).map(|n| super::retry_wait(n).as_secs()).collect();
    assert_eq!(waits, [1, 2, 4, 8, 16, 32, 60, 60, 60]);
}

#[tokio::test]
async fn a_delivery_failing_ten_times_is_given_up_and_the_next_is_delivered() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(&directory.0).await.unwrap();
    a_turn_end_then_a_reply(&store).await;
    let dispatcher = Arc::new(FailsOneType::new("TurnCompleted"));
    let logs = CapturedLogs::start();

    for failure in 1..=9 {
        let error = handle_next_delivery(&store, dispatcher.clone())
            .await
            .unwrap_err();
        let super::HandlerError::Delivery { retry_in, .. } = error else {
            panic!("failure {failure} was {error}");
        };
        assert_eq!(retry_in, super::retry_wait(failure));
    }
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

    let handed = dispatcher.handed.lock().unwrap().clone();
    assert_eq!(handed.len(), 11);
    assert!(handed[..10].iter().all(|kind| kind == "TurnCompleted"));
    assert_eq!(handed[10], "InboundMessageReceived");
    let text = logs.text();
    assert_eq!(text.matches("delivery given up").count(), 1, "{text}");
    for field in [
        "event_type=\"TurnCompleted\"",
        "event_version=1",
        "outcome=\"given_up\"",
        "reason=\"too_many_tries\"",
    ] {
        assert!(text.contains(field), "{field} missing: {text}");
    }
    // Neither the error's text nor the payload's reaches the new line.
    let given_up_line = text
        .lines()
        .find(|line| line.contains("delivery given up"))
        .unwrap();
    assert!(!given_up_line.contains("secret text"), "{given_up_line}");
    assert!(!given_up_line.contains("private answer"), "{given_up_line}");
    assert!(!given_up_line.contains("event_id"), "{given_up_line}");
}

#[tokio::test]
async fn a_delivery_that_fails_twice_then_succeeds_is_delivered_and_not_given_up() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(&directory.0).await.unwrap();
    append_inbound_message(
        &store,
        &InboundMessage {
            text: "third time".into(),
            forwarded: false,
        },
    )
    .await
    .unwrap();
    let logs = CapturedLogs::start();

    for _ in 0..2 {
        assert!(
            handle_next_delivery(&store, Arc::new(FailingDispatcher))
                .await
                .is_err()
        );
    }
    assert!(
        handle_next_delivery(&store, Arc::new(RecordingDispatcher::default()))
            .await
            .unwrap()
    );

    assert!(store.next_pending_delivery().await.unwrap().is_none());
    assert!(!logs.text().contains("delivery given up"));
}

#[tokio::test]
async fn failures_before_a_restart_count_toward_the_ten() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(&directory.0).await.unwrap();
    a_turn_end_then_a_reply(&store).await;
    let dispatcher = Arc::new(FailsOneType::new("TurnCompleted"));
    for _ in 0..7 {
        assert!(
            handle_next_delivery(&store, dispatcher.clone())
                .await
                .is_err()
        );
    }
    drop(store);

    let store = HaroldStore::open(&directory.0).await.unwrap();
    assert_eq!(
        store
            .next_pending_delivery()
            .await
            .unwrap()
            .unwrap()
            .attempt_count,
        7
    );
    assert!(
        handle_next_delivery(&store, dispatcher.clone())
            .await
            .is_err()
    );
    assert!(
        handle_next_delivery(&store, dispatcher.clone())
            .await
            .is_err()
    );
    assert!(
        handle_next_delivery(&store, dispatcher.clone())
            .await
            .unwrap()
    );

    let next = store.next_pending_delivery().await.unwrap().unwrap();
    assert_eq!(next.event_type, "InboundMessageReceived");
}

#[tokio::test]
async fn giving_up_queues_the_alert_line_when_the_address_is_on() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(&directory.0).await.unwrap();
    a_turn_end_then_a_reply(&store).await;
    let dispatcher = Arc::new(FailsOneType::new("TurnCompleted"));
    let (alerts, mut alert_rx) = crate::alerts::AlertQueue::new();

    for _ in 0..9 {
        assert!(
            super::handle_next_delivery_alerting(&store, dispatcher.clone(), &alerts)
                .await
                .is_err()
        );
        assert!(alert_rx.try_recv().is_err(), "no line before giving up");
    }
    assert!(
        super::handle_next_delivery_alerting(&store, dispatcher.clone(), &alerts)
            .await
            .unwrap()
    );

    let (line, _) = alert_rx.try_recv().unwrap();
    assert_eq!(line, "harold alert: delivery-given-up firing");
    assert!(alert_rx.try_recv().is_err());
}

#[tokio::test]
async fn permanently_invalid_event_does_not_block_later_deliveries() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(&directory.0).await.unwrap();
    let logs = CapturedLogs::start();
    store
        .stream()
        .append([NewEvent::new("Unknown", json!({"unexpected": true}))])
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
    // Undeliverable on its first try, and not as a give-up.
    assert!(!logs.text().contains("delivery given up"));
}

#[tokio::test]
async fn an_undecodable_record_does_not_block_later_events() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(&directory.0).await.unwrap();
    let raw_version = store
        .stream()
        .append_raw_for_test(b"not json".to_vec())
        .await
        .unwrap();
    append_inbound_message(
        &store,
        &InboundMessage {
            text: "deliver after the undecodable record".into(),
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

    let undecodable = store.stream().read(raw_version).await.unwrap().unwrap();
    assert_eq!(undecodable.r#type, "UndecodableEvent");
    let row = store
        .outbox_row_for_test(&undecodable.id.to_string())
        .await
        .unwrap()
        .expect("the undecodable record has an outbox row");
    assert!(row.delivered_at_ms.is_some(), "marked undeliverable");
    assert_eq!(row.attempt_count, 1);
}

#[tokio::test]
async fn malformed_known_event_does_not_block_a_valid_later_delivery() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(&directory.0).await.unwrap();
    store
        .stream()
        .append([NewEvent::new("TurnCompleted", json!({"pane_id": 7}))])
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

/// An event id no other test uses. A stored reply's phone request is kept under
/// `event:<id>` in a process-wide map, so a shared id would let tests see each other's.
fn unique_event_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[test]
fn paused_inbound_delivery_is_skipped_without_routing() {
    // The payload is deliberately unroutable: were the gate removed, this fails on the
    // payload instead of relaying text to a real pane.
    let delivery = PendingDelivery {
        event_id: unique_event_id(),
        event_version: EventVersion::new(1).unwrap(),
        event_type: "InboundMessageReceived".into(),
        payload: json!(null),
        trace_id: "trace".into(),
        attempt_count: 0,
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
        event_id: unique_event_id(),
        event_version: EventVersion::new(1).unwrap(),
        event_type: "InboundMessageReceived".into(),
        payload: json!(null),
        trace_id: "trace".into(),
        attempt_count: 0,
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

        let outcome = route_message(&message, &unique_event_id(), None, |text, flag| {
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

    let outcome = route_message(&message, &unique_event_id(), None, |_, _| {
        Err("tmux failed".to_string())
    });

    assert!(matches!(outcome, Err(DispatchError::Retryable(error)) if error == "tmux failed"));
}

/// What routing returns for a reply another Mac owns. It is not counted, so these tests
/// leave the process-wide phone book as it was.
fn not_owned_report() -> RouteReport {
    RouteReport {
        outcome: DeliveryOutcome::Skipped,
        routing: Routing::NotOwned { tagged: false },
    }
}

/// A hold by a channel's list, as the dispatch functions are given one.
fn listed_hold(line: &str, channel: Channel) -> crate::outbound::left_out::Held {
    crate::outbound::left_out::Held::not_listed(line.to_string(), channel, "Dev/test".to_string())
}

fn away_delivery(payload: serde_json::Value) -> PendingDelivery {
    PendingDelivery {
        event_id: "event-1".into(),
        event_version: EventVersion::new(1).unwrap(),
        event_type: "AwayMessageRequested".into(),
        payload,
        trace_id: "trace".into(),
        attempt_count: 0,
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
            Some(listed_hold(
                "agent message dropped (not in notify.imessage)",
                Channel::Imessage,
            ))
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
        .append([NewEvent::new("AwayMessageRequested", json!({"pane_id": 7}))])
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
        event_version: EventVersion::new(1).unwrap(),
        event_type: "TurnCompleted".into(),
        payload: json!(crate::outbound::fake::turn("%47", "build it")),
        trace_id: "trace".into(),
        attempt_count: 0,
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
        event_version: EventVersion::new(1).unwrap(),
        event_type: "TurnCompleted".into(),
        payload: json!(crate::outbound::fake::turn(pane_id, prompt)),
        trace_id: "trace".into(),
        attempt_count: 0,
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
// The channel lists: the dispatcher reads the tracked agent panes from the snapshot hub
// ---------------------------------------------------------------------------

const ONLY_FROM_CHIEF: &str = "[notify]\nchief = \"/work/chief-of-pods\"\nimessage = [\"/work/chief-of-pods\"]\nmachine_audio_output = [\"/work/chief-of-pods\"]";

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
    let dispatcher: Arc<dyn DeliveryDispatcher> = Arc::new(production_dispatcher(
        environment.clone(),
        &snapshots,
        None,
        None,
    ));
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
    while handle_next_delivery_inner(store, Arc::clone(dispatcher), Some(snapshots), None)
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
async fn update_stays_held_back_once_the_agent_in_the_update_folder_has_exited() {
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

    assert!(environment.reply_targets().is_empty());
}

#[tokio::test]
async fn update_stays_held_back_while_harold_cannot_list_the_tmux_panes() {
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

    assert!(environment.reply_targets().is_empty());
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
// The channel lists for requested messages: a pane outside the folder has its message dropped
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

    // The drop set no mark. The pane is still off the list once the chief has gone, so the
    // update is held back by the list, as before.
    append_agent_events(&store, vec![chief_departed()])
        .await
        .unwrap();
    append_turn_completed(&store, &crate::outbound::fake::turn("%2", "build it"))
        .await
        .unwrap();

    drain(&store, &snapshots, &dispatcher).await;

    assert!(environment.delivered().is_empty());
    assert!(environment.reply_targets().is_empty());
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
async fn requested_message_is_dropped_while_no_agent_is_in_the_folder() {
    let directory = TestDirectory::new();
    let (store, snapshots, environment, dispatcher) =
        only_from_chief(&directory, vec![agent_observed("%2", 20, "/work/harold")]).await;
    request_message(&store, "%2", "Pod 3 is blocked").await;

    drain(&store, &snapshots, &dispatcher).await;

    assert!(environment.away_messages().is_empty());
    assert!(environment.reply_targets().is_empty());
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
async fn requested_message_off_the_list_is_dropped_while_harold_cannot_list_the_tmux_panes() {
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

    assert!(environment.away_messages().is_empty());
    assert!(environment.reply_targets().is_empty());
}

#[tokio::test]
async fn listed_agents_message_whose_send_failed_is_sent_on_the_retry() {
    let directory = TestDirectory::new();
    let (store, snapshots, environment, dispatcher) =
        only_from_chief(&directory, chief_and_pod()).await;
    request_message(&store, "%1", "Pod 3 is blocked").await;

    environment.set_fail_sends(true);
    let error = handle_next_delivery_inner(&store, Arc::clone(&dispatcher), Some(&snapshots), None)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("channel down"));
    assert!(store.next_pending_delivery().await.unwrap().is_some());

    environment.set_fail_sends(false);
    drain(&store, &snapshots, &dispatcher).await;

    assert_eq!(environment.away_messages(), ["Pod 3 is blocked"]);
    assert_eq!(environment.reply_targets(), ["%1"]);
}

#[tokio::test]
async fn message_stored_while_the_chief_ran_is_still_dropped_once_the_chief_has_exited() {
    let directory = TestDirectory::new();
    let (store, snapshots, environment, dispatcher) =
        only_from_chief(&directory, chief_and_pod()).await;
    request_message(&store, "%2", "Pod 3 is blocked").await;
    // The chief exits before the handler comes to the message.
    append_agent_events(&store, vec![chief_departed()])
        .await
        .unwrap();

    drain(&store, &snapshots, &dispatcher).await;

    assert!(environment.away_messages().is_empty());
    assert!(environment.reply_targets().is_empty());
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
        text.contains("agent message dropped (not in notify.imessage) pane_id=%2"),
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
    assert!(matches!(
        update,
        Ok(DeliveryOutcome::HeldBack {
            reason: "not_listed"
        })
    ));
    assert!(environment.delivered().is_empty());
    assert!(environment.reply_targets().is_empty());
    let text = logs.text();
    assert!(
        text.contains("notification skipped (not in notify.imessage) pane_id=%2"),
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
}

// ---------------------------------------------------------------------------
// Spoken lines: away device when the screen is locked, the desk otherwise
// ---------------------------------------------------------------------------

fn spoken_delivery(payload: serde_json::Value) -> PendingDelivery {
    PendingDelivery {
        event_id: "event-1".into(),
        event_version: EventVersion::new(1).unwrap(),
        event_type: "SpokenLineRequested".into(),
        payload,
        trace_id: "trace".into(),
        attempt_count: 0,
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
        |_| {
            Some(listed_hold(
                "spoken line dropped (not in notify.imessage)",
                Channel::Imessage,
            ))
        },
        &environment,
    );

    assert!(matches!(outcome, Ok(DeliveryOutcome::Skipped)));
    assert!(environment.voice_notes().is_empty());
    assert!(
        logs.text()
            .contains("spoken line dropped (not in notify.imessage) pane_id=%2")
    );
    assert!(!logs.text().contains("secret words"));
}

/// A chief's line on a locked screen with the chief app answering, and `in_call` as the open
/// call's answer; `in_call_asked` is how many times the call was asked.
fn dispatch_line_with_call(
    environment: &FakeEnvironment,
    app: &FakeApp,
    away: &FakeAwayText,
    in_call: Option<bool>,
) -> (Result<DeliveryOutcome, DispatchError>, usize) {
    let asked = Cell::new(0);
    let outcome = dispatch_spoken_line_with_app(
        &spoken_delivery(json!(chief_line("hi"))),
        false,
        false,
        |_| None,
        |_| None,
        environment,
        |_| {
            Some(ChiefApp {
                app,
                away,
                away_allowed: true,
            })
        },
        |_, text| {
            assert_eq!(text, "hi");
            asked.set(asked.get() + 1);
            in_call
        },
        |_, _| {},
        |_| {
            listed_hold(
                "spoken line dropped (not in notify.imessage)",
                Channel::Imessage,
            )
        },
    );
    (outcome, asked.get())
}

#[test]
fn a_spoken_line_an_open_call_played_is_not_pushed_or_sent() {
    let environment = FakeEnvironment::locked("");
    let app = FakeApp::new(AppDelivery::Pushed);
    let away = FakeAwayText::default();

    let (outcome, asked) = dispatch_line_with_call(&environment, &app, &away, Some(true));

    assert!(matches!(outcome, Ok(DeliveryOutcome::Delivered)));
    assert_eq!(asked, 1);
    assert!(app.delivered().is_empty());
    assert!(away.sends().is_empty());
    assert!(environment.spoken_lines().is_empty());
    assert!(environment.voice_notes().is_empty());
}

#[test]
fn a_spoken_line_the_call_did_not_play_goes_by_the_app_route() {
    let environment = FakeEnvironment::locked("");
    let app = FakeApp::new(AppDelivery::Pushed);
    let away = FakeAwayText::default();

    let (outcome, asked) = dispatch_line_with_call(&environment, &app, &away, Some(false));

    assert!(matches!(outcome, Ok(DeliveryOutcome::Delivered)));
    assert_eq!(asked, 1);
    assert_eq!(app.delivered(), [("event-1".to_string(), "hi".to_string())]);
}

#[test]
fn a_spoken_line_with_no_call_open_goes_by_the_app_route() {
    let environment = FakeEnvironment::locked("");
    let app = FakeApp::new(AppDelivery::Pushed);
    let away = FakeAwayText::default();

    let (outcome, asked) = dispatch_line_with_call(&environment, &app, &away, None);

    assert!(matches!(outcome, Ok(DeliveryOutcome::Delivered)));
    assert_eq!(asked, 1);
    assert_eq!(app.delivered(), [("event-1".to_string(), "hi".to_string())]);
}

/// The daemon asks only the phone's calls (`say_in_phone_call`), never the desk's: with no
/// phone call open the line goes by the screen lock's rules, as before the phone's calls,
/// even while a desk call is open.
#[test]
fn a_spoken_line_with_no_phone_call_open_goes_by_the_lock_rules() {
    for (messaging_paused, delivered) in [(false, true), (true, false)] {
        let environment = FakeEnvironment::locked("");
        let app = FakeApp::new(AppDelivery::Pushed);
        let away = FakeAwayText::default();

        let outcome = dispatch_spoken_line_with_app(
            &spoken_delivery(json!(chief_line("hi"))),
            messaging_paused,
            false,
            |_| None,
            |_| None,
            &environment,
            |_| {
                Some(ChiefApp {
                    app: &app,
                    away: &away,
                    away_allowed: true,
                })
            },
            |_, text| say_in_phone_call_over(None, text, Speaker::Chief),
            |_, _| {},
            |_| {
                listed_hold(
                    "spoken line dropped (not in notify.imessage)",
                    Channel::Imessage,
                )
            },
        );

        if delivered {
            assert!(matches!(outcome, Ok(DeliveryOutcome::Delivered)));
            assert_eq!(app.delivered(), [("event-1".to_string(), "hi".to_string())]);
        } else {
            assert!(matches!(outcome, Ok(DeliveryOutcome::Skipped)));
            assert!(app.delivered().is_empty());
        }
        assert!(away.sends().is_empty());
        assert!(environment.spoken_lines().is_empty());
    }
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
    let (trace_id, created_at) = stored_envelope(store, EventVersion::new(1).unwrap())
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
async fn a_given_up_reply_is_counted_unavailable() {
    telemetry::test_support::install();
    let channel = unique_channel();
    let router: Router = Arc::new(|_, _| Err("tmux failed".into()));
    let (fixture, dispatcher) = fixture_with_router(router).await;
    store_reply_as_listener(
        &fixture.store,
        channel,
        time::OffsetDateTime::now_utc(),
        "hi",
    )
    .await;

    for _ in 0..9 {
        assert!(
            handle_next_delivery(&fixture.store, dispatcher.clone())
                .await
                .is_err()
        );
    }
    assert!(
        handle_next_delivery(&fixture.store, dispatcher.clone())
            .await
            .unwrap()
    );

    let trace_id = first_envelope(&fixture.store).await.trace_id.unwrap();
    let requests = spans_in_trace("harold.phone_request", &trace_id);
    assert_eq!(requests.len(), 1);
    assert_eq!(attr(&requests[0], "outcome"), "unavailable");
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
}

#[tokio::test]
async fn a_given_up_reply_with_no_listener_request_is_counted_unavailable() {
    use opentelemetry::trace::TraceContextExt as _;
    telemetry::test_support::install();
    let router: Router = Arc::new(|_, _| Err("tmux failed".into()));
    let (fixture, dispatcher) = fixture_with_router(router).await;
    // Stored as after a restart: no listener opened a request for it.
    append_inbound_message(
        &fixture.store,
        &InboundMessage {
            text: "hi".into(),
            forwarded: true,
        },
    )
    .await
    .unwrap();
    // Projected as the handler projects it, so its outbox row exists to read.
    fixture.store.project_unhandled_events(500).await.unwrap();
    let event_id = fixture
        .store
        .next_pending_delivery()
        .await
        .unwrap()
        .unwrap()
        .event_id;

    for _ in 0..9 {
        assert!(
            handle_next_delivery(&fixture.store, dispatcher.clone())
                .await
                .is_err()
        );
    }
    let open = telemetry::open_reply_for(&format!("event:{event_id}"))
        .expect("the first try opened a request under the event's key");
    let trace_id = open.cx.span().span_context().trace_id().to_string();
    assert!(
        handle_next_delivery(&fixture.store, dispatcher.clone())
            .await
            .unwrap()
    );

    let requests = spans_in_trace("harold.phone_request", &trace_id);
    assert_eq!(requests.len(), 1);
    assert_eq!(attr(&requests[0], "outcome"), "unavailable");
    assert_eq!(attr(&requests[0], "channel"), "forwarded");
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

// ---------------------------------------------------------------------------
// Turn-end deliveries
// ---------------------------------------------------------------------------

/// A turn-end delivery for `pane_id` under the event id `event_id`.
fn turn_end_delivery(event_id: &str, pane_id: &str) -> PendingDelivery {
    PendingDelivery {
        event_id: event_id.into(),
        ..turn_delivery(pane_id, "build it")
    }
}

/// Dispatches a turn end on `pane_id` through `dispatcher`, on this thread, inside the
/// spans the event handler opens around it.
fn dispatch_turn_end(
    dispatcher: &dyn DeliveryDispatcher,
    event_id: &str,
    pane_id: &str,
) -> Result<DeliveryOutcome, DispatchError> {
    let event = tracing::info_span!("event", event_type = "TurnCompleted");
    super::dispatch_in_spans(
        dispatcher,
        &turn_end_delivery(event_id, pane_id),
        None,
        &event,
        false,
    )
    .0
}

/// Delivers a turn end on `pane_id` as the event handler does, on this thread, through the
/// daemon's dispatcher over the fake `environment`.
pub(crate) fn dispatch_turn_end_with_fake_environment(
    pane_id: &str,
    environment: &Arc<FakeEnvironment>,
) -> Result<DeliveryOutcome, DispatchError> {
    let event_id = uuid::Uuid::new_v4().to_string();
    dispatch_turn_end(&Unpaused(dispatcher_with(environment)), &event_id, pane_id)
}

/// The daemon's dispatcher over the fake `environment`, never paused, with `router` in
/// place of tmux routing.
pub(crate) fn fake_dispatcher(
    environment: &Arc<FakeEnvironment>,
    router: Router,
) -> Arc<dyn DeliveryDispatcher> {
    Arc::new(Unpaused(dispatcher_with(environment).with_router(router)))
}

/// A live span standing in for a turn's `harold.turn_complete`: its trace id, span id
/// and context.
fn a_live_turn() -> (
    opentelemetry::trace::TraceId,
    opentelemetry::trace::SpanId,
    opentelemetry::Context,
) {
    use opentelemetry::trace::TraceContextExt as _;
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;
    telemetry::test_support::install();
    let turn = tracing::info_span!(target: telemetry::TRACE_TARGET, "test.turn_complete");
    let cx = turn.context();
    let (trace_id, span_id) = {
        let span = cx.span();
        let context = span.span_context();
        (context.trace_id(), context.span_id())
    };
    (trace_id, span_id, cx)
}

/// A live turn, kept for the turn end `event_id`. Returns the span's trace and span ids.
fn keep_a_turn_for(
    event_id: uuid::Uuid,
) -> (opentelemetry::trace::TraceId, opentelemetry::trace::SpanId) {
    let (trace_id, span_id, cx) = a_live_turn();
    telemetry::keep_turn_delivery(event_id, cx);
    (trace_id, span_id)
}

/// The exported `harold.delivery` spans whose parent is the span `parent`.
fn deliveries_under(
    parent: opentelemetry::trace::SpanId,
) -> Vec<opentelemetry_sdk::trace::SpanData> {
    telemetry::test_support::spans()
        .into_iter()
        .filter(|span| span.name == "harold.delivery" && span.parent_span_id == parent)
        .collect()
}

#[test]
fn a_given_up_turn_end_records_given_up_and_keeps_no_trace() {
    let event_id = uuid::Uuid::new_v4();
    let (_, parent) = keep_a_turn_for(event_id);
    let environment = Arc::new(FakeEnvironment::new(""));
    let dispatcher = Unpaused(dispatcher_with(&environment));
    let pane = unique_pane();
    environment.set_fail_sends(true);

    let event = tracing::info_span!("event", event_type = "TurnCompleted");
    let (result, _) = super::dispatch_in_spans(
        &dispatcher,
        &turn_end_delivery(&event_id.to_string(), &pane),
        None,
        &event,
        true,
    );

    assert!(matches!(result, Err(DispatchError::Retryable(_))));
    let outcomes: Vec<String> = deliveries_under(parent)
        .iter()
        .map(|span| attr(span, "outcome"))
        .collect();
    assert_eq!(outcomes, ["given_up"]);
    assert!(telemetry::take_turn_delivery(&event_id.to_string()).is_none());
}

#[test]
fn a_turn_end_delivery_continues_the_trace_kept_for_its_event() {
    let event_id = uuid::Uuid::new_v4();
    let (trace_id, parent) = keep_a_turn_for(event_id);
    let environment = Arc::new(FakeEnvironment::new(""));
    let dispatcher = Unpaused(dispatcher_with(&environment));

    let outcome = dispatch_turn_end(&dispatcher, &event_id.to_string(), &unique_pane());

    assert!(matches!(outcome, Ok(DeliveryOutcome::Delivered)));
    let deliveries = deliveries_under(parent);
    assert_eq!(deliveries.len(), 1);
    assert_eq!(deliveries[0].span_context.trace_id(), trace_id);
    assert_eq!(attr(&deliveries[0], "kind"), "turn_end");
    assert_eq!(attr(&deliveries[0], "channel"), "tts");
    assert_eq!(attr(&deliveries[0], "outcome"), "delivered");
    assert!(
        telemetry::take_turn_delivery(&event_id.to_string()).is_none(),
        "a delivered turn end is no longer kept"
    );
}

#[test]
fn a_turn_end_sent_while_away_records_the_away_channel() {
    for (overrides, channel) in [
        ("", "imessage"),
        ("[notify]\naway_channel = \"telegram\"", "telegram"),
    ] {
        let event_id = uuid::Uuid::new_v4();
        let (_, parent) = keep_a_turn_for(event_id);
        let environment = Arc::new(FakeEnvironment::locked(overrides));

        let outcome = dispatch_turn_end(
            &Unpaused(dispatcher_with(&environment)),
            &event_id.to_string(),
            &unique_pane(),
        );

        assert!(matches!(outcome, Ok(DeliveryOutcome::Delivered)));
        let deliveries = deliveries_under(parent);
        assert_eq!(deliveries.len(), 1, "{channel}");
        assert_eq!(attr(&deliveries[0], "channel"), channel);
    }
}

#[test]
fn a_skipped_turn_end_records_skipped() {
    let event_id = uuid::Uuid::new_v4();
    let (_, parent) = keep_a_turn_for(event_id);
    let environment = Arc::new(FakeEnvironment::new(""));
    environment.set_session_attached(true);

    let outcome = dispatch_turn_end(
        &Unpaused(dispatcher_with(&environment)),
        &event_id.to_string(),
        &unique_pane(),
    );

    assert!(matches!(outcome, Ok(DeliveryOutcome::Skipped)));
    let deliveries = deliveries_under(parent);
    assert_eq!(deliveries.len(), 1);
    assert_eq!(attr(&deliveries[0], "outcome"), "skipped");
}

#[test]
fn a_failed_turn_end_records_failed_and_its_retry_stays_in_the_trace() {
    let event_id = uuid::Uuid::new_v4();
    let (trace_id, parent) = keep_a_turn_for(event_id);
    let environment = Arc::new(FakeEnvironment::new(""));
    let dispatcher = Unpaused(dispatcher_with(&environment));
    let pane = unique_pane();

    environment.set_fail_sends(true);
    let failed = dispatch_turn_end(&dispatcher, &event_id.to_string(), &pane);
    environment.set_fail_sends(false);
    let retried = dispatch_turn_end(&dispatcher, &event_id.to_string(), &pane);

    assert!(matches!(failed, Err(DispatchError::Retryable(_))));
    assert!(matches!(retried, Ok(DeliveryOutcome::Delivered)));
    let deliveries = deliveries_under(parent);
    let outcomes: Vec<String> = deliveries
        .iter()
        .map(|span| attr(span, "outcome"))
        .collect();
    assert_eq!(outcomes, ["failed", "delivered"]);
    assert!(
        deliveries
            .iter()
            .all(|span| span.span_context.trace_id() == trace_id)
    );
}

#[test]
fn a_turn_end_with_no_kept_trace_starts_its_own() {
    telemetry::test_support::install();
    let environment = Arc::new(FakeEnvironment::new(""));
    let pane = unique_pane();
    let before = telemetry::test_support::spans().len();

    let outcome = dispatch_turn_end_with_fake_environment(&pane, &environment);

    assert!(matches!(outcome, Ok(DeliveryOutcome::Delivered)));
    assert_eq!(
        environment.delivered(),
        [(crate::outbound::OutboundChannel::Tts, pane)]
    );
    let started_own = telemetry::test_support::spans()
        .into_iter()
        .skip(before)
        .any(|span| {
            span.name == "harold.delivery"
                && span.parent_span_id == opentelemetry::trace::SpanId::INVALID
                && attr(&span, "outcome") == "delivered"
        });
    assert!(started_own);
}

#[test]
fn only_a_turn_end_gets_a_delivery_span() {
    use opentelemetry::trace::TraceContextExt as _;
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;
    telemetry::test_support::install();
    let environment = Arc::new(FakeEnvironment::locked(""));
    let dispatcher = Unpaused(dispatcher_with(&environment));
    let around = tracing::info_span!(target: telemetry::TRACE_TARGET, "test.around");
    let parent = around.context().span().span_context().span_id();

    let outcome = around.in_scope(|| {
        let event = tracing::info_span!("event", event_type = "AwayMessageRequested");
        super::dispatch_in_spans(
            &dispatcher,
            &away_delivery(json!(pod_message("Pod 3 is blocked"))),
            None,
            &event,
            false,
        )
        .0
    });

    assert!(matches!(outcome, Ok(DeliveryOutcome::Delivered)));
    assert!(deliveries_under(parent).is_empty());
}

#[test]
fn a_turn_end_delivered_before_its_event_id_is_kept_joins_the_turn_by_its_pane() {
    let (trace_id, parent, cx) = a_live_turn();
    let pane = unique_pane();
    let event_id = uuid::Uuid::new_v4();
    let environment = Arc::new(FakeEnvironment::new(""));
    // The handler keeps the turn under its pane before it asks the monitor to store it.
    telemetry::keep_pending_turn(&pane, cx.clone());

    // The event handler delivers the stored turn end before the handler has its event id.
    let outcome = dispatch_turn_end(
        &Unpaused(dispatcher_with(&environment)),
        &event_id.to_string(),
        &pane,
    );
    telemetry::settle_pending_turn(&pane, event_id, &cx);

    assert!(matches!(outcome, Ok(DeliveryOutcome::Delivered)));
    let deliveries = deliveries_under(parent);
    assert_eq!(deliveries.len(), 1);
    assert_eq!(deliveries[0].span_context.trace_id(), trace_id);
    assert!(
        telemetry::take_turn_delivery(&event_id.to_string()).is_none(),
        "a turn its delivery already joined is not kept again"
    );

    // A later turn end on the same pane does not join this turn.
    let later = dispatch_turn_end(
        &Unpaused(dispatcher_with(&Arc::new(FakeEnvironment::new("")))),
        &uuid::Uuid::new_v4().to_string(),
        &pane,
    );
    assert!(matches!(later, Ok(DeliveryOutcome::Delivered)));
    assert_eq!(deliveries_under(parent).len(), 1);
}

#[test]
fn a_turn_stored_before_its_delivery_is_kept_by_its_event_id_only() {
    let (_, parent, cx) = a_live_turn();
    let pane = unique_pane();
    let event_id = uuid::Uuid::new_v4();
    telemetry::keep_pending_turn(&pane, cx.clone());
    telemetry::settle_pending_turn(&pane, event_id, &cx);

    // Another turn end on the pane, with another event id, does not take it.
    let other = dispatch_turn_end(
        &Unpaused(dispatcher_with(&Arc::new(FakeEnvironment::new("")))),
        &uuid::Uuid::new_v4().to_string(),
        &pane,
    );
    assert!(matches!(other, Ok(DeliveryOutcome::Delivered)));
    assert!(deliveries_under(parent).is_empty());

    let own = dispatch_turn_end(
        &Unpaused(dispatcher_with(&Arc::new(FakeEnvironment::new("")))),
        &event_id.to_string(),
        &pane,
    );
    assert!(matches!(own, Ok(DeliveryOutcome::Delivered)));
    assert_eq!(deliveries_under(parent).len(), 1);
}

#[test]
fn a_turn_whose_store_failed_is_not_joined_by_a_later_turn_end() {
    let (_, parent, cx) = a_live_turn();
    let pane = unique_pane();
    telemetry::keep_pending_turn(&pane, cx.clone());
    telemetry::forget_pending_turn(&pane, &cx);

    let later = dispatch_turn_end(
        &Unpaused(dispatcher_with(&Arc::new(FakeEnvironment::new("")))),
        &uuid::Uuid::new_v4().to_string(),
        &pane,
    );
    assert!(matches!(later, Ok(DeliveryOutcome::Delivered)));
    assert!(deliveries_under(parent).is_empty());
}

/// Whether an exported log record with the body `body` carries the trace `trace_id`.
fn logged_in_trace(body: &str, trace_id: opentelemetry::trace::TraceId) -> bool {
    telemetry::test_support::logs().iter().any(|log| {
        log.record
            .body()
            .is_some_and(|value| format!("{value:?}").contains(body))
            && log.record.trace_context().map(|trace| trace.trace_id) == Some(trace_id)
    })
}

#[tokio::test]
async fn an_undeliverable_turn_end_is_logged_inside_its_trace() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(&directory.0).await.unwrap();
    let appended = store
        .stream()
        .append([NewEvent::new("TurnCompleted", json!({"pane_id": 7}))])
        .await
        .unwrap();
    let (trace_id, _) = keep_a_turn_for(appended.events[0].id);
    let logs = CapturedLogs::start();

    assert!(
        handle_next_delivery(&store, Arc::new(system_dispatcher()))
            .await
            .unwrap()
    );

    assert!(logged_in_trace(
        "event is permanently undeliverable",
        trace_id
    ));
    // harold.log writes the line as before, with no span prefix.
    assert!(
        logs.text()
            .contains("WARN harold::projector: event is permanently undeliverable"),
        "{}",
        logs.text()
    );
}

#[tokio::test]
async fn a_turn_end_failing_ten_times_records_nine_failed_deliveries_then_given_up() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(&directory.0).await.unwrap();
    let appended = store
        .stream()
        .append([NewEvent::new("TurnCompleted", json!({"pane_id": "%4"}))])
        .await
        .unwrap();
    let (_, parent) = keep_a_turn_for(appended.events[0].id);
    let dispatcher: Arc<dyn DeliveryDispatcher> = Arc::new(FailingDispatcher);

    for _ in 1..=9 {
        assert!(
            handle_next_delivery(&store, dispatcher.clone())
                .await
                .is_err()
        );
    }
    assert!(
        handle_next_delivery(&store, dispatcher.clone())
            .await
            .unwrap()
    );

    let outcomes: Vec<String> = deliveries_under(parent)
        .iter()
        .map(|span| attr(span, "outcome"))
        .collect();
    let mut expected = vec!["failed"; 9];
    expected.push("given_up");
    assert_eq!(outcomes, expected);
}

// ---------------------------------------------------------------------------
// A failing store
// ---------------------------------------------------------------------------

struct PermanentDispatcher;

impl DeliveryDispatcher for PermanentDispatcher {
    fn dispatch(&self, _delivery: &PendingDelivery) -> Result<DeliveryOutcome, DispatchError> {
        Err(DispatchError::Permanent("bad payload".into()))
    }
}

/// Stores one reply and returns its outbox row's event id.
async fn one_reply(store: &HaroldStore) -> String {
    append_inbound_message(
        store,
        &InboundMessage {
            text: "carry on".into(),
            forwarded: false,
        },
    )
    .await
    .unwrap();
    store.project_unhandled_events(10).await.unwrap();
    store
        .next_pending_delivery()
        .await
        .unwrap()
        .expect("the reply is pending")
        .event_id
}

async fn outbox_row(store: &HaroldStore, event_id: &str) -> crate::store::OutboxRow {
    store
        .outbox_row_for_test(event_id)
        .await
        .unwrap()
        .expect("the outbox row")
}

#[tokio::test(start_paused = true)]
async fn a_sent_row_whose_mark_delivered_fails_is_sent_once_and_marked_after_recovery() {
    let directory = TestDirectory::new();
    let store = Arc::new(HaroldStore::open(&directory.0).await.unwrap());
    let event_id = one_reply(&store).await;
    store.fault_state_writes_for_test(Some(WriteFault::Fail));
    let dispatcher = Arc::new(RecordingDispatcher::default());
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
    let task = tokio::spawn(super::run_deliveries(
        Arc::clone(&store),
        dispatcher.clone(),
        None,
        None,
        shutdown_rx,
    ));

    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(dispatcher.event_types.lock().unwrap().len(), 1);
    assert!(store.is_failing());
    assert_eq!(outbox_row(&store, &event_id).await.delivered_at_ms, None);

    tokio::time::sleep(Duration::from_secs(28)).await;
    assert_eq!(dispatcher.event_types.lock().unwrap().len(), 1);
    store.fault_state_writes_for_test(None);
    tokio::time::sleep(Duration::from_secs(2)).await;

    assert!(
        outbox_row(&store, &event_id)
            .await
            .delivered_at_ms
            .is_some()
    );
    assert_eq!(dispatcher.event_types.lock().unwrap().len(), 1);
    assert!(!store.is_failing());
    drop(shutdown_tx);
    task.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_mark_delivered_that_never_returns_fails_after_30_seconds() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(&directory.0).await.unwrap();
    one_reply(&store).await;
    store.fault_state_writes_for_test(Some(WriteFault::Hang));
    let started = tokio::time::Instant::now();

    let error = handle_next_delivery(&store, Arc::new(RecordingDispatcher::default()))
        .await
        .unwrap_err();

    assert!(
        matches!(error, super::HandlerError::NotMarkedDelivered { .. }),
        "{error}"
    );
    assert!(started.elapsed() >= Duration::from_secs(30));
    assert!(store.is_failing());
}

#[tokio::test(start_paused = true)]
async fn a_failing_record_delivery_failure_marks_the_store_failing_and_keeps_the_try_count() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(&directory.0).await.unwrap();
    let event_id = one_reply(&store).await;
    store.fault_state_writes_for_test(Some(WriteFault::Fail));

    let error = handle_next_delivery(&store, Arc::new(FailingDispatcher))
        .await
        .unwrap_err();

    assert!(matches!(error, super::HandlerError::Store(_)), "{error}");
    assert!(store.is_failing());
    let row = outbox_row(&store, &event_id).await;
    assert_eq!(row.attempt_count, 0);
    assert_eq!(row.delivered_at_ms, None);
}

#[tokio::test(start_paused = true)]
async fn a_failing_mark_undeliverable_marks_the_store_failing_and_keeps_the_try_count() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(&directory.0).await.unwrap();
    let event_id = one_reply(&store).await;
    store.fault_state_writes_for_test(Some(WriteFault::Fail));

    let error = handle_next_delivery(&store, Arc::new(PermanentDispatcher))
        .await
        .unwrap_err();

    assert!(matches!(error, super::HandlerError::Store(_)), "{error}");
    assert!(store.is_failing());
    let row = outbox_row(&store, &event_id).await;
    assert_eq!(row.attempt_count, 0);
    assert_eq!(row.delivered_at_ms, None);
}

#[tokio::test(start_paused = true)]
async fn a_give_up_whose_mark_undeliverable_fails_says_nothing_until_it_is_stored() {
    let logs = CapturedLogs::start();
    let directory = TestDirectory::new();
    let store = HaroldStore::open(&directory.0).await.unwrap();
    let event_id = one_reply(&store).await;
    let (alerts, mut alert_rx) = crate::alerts::AlertQueue::new();
    for _ in 0..9 {
        assert!(
            super::handle_next_delivery_alerting(&store, Arc::new(FailingDispatcher), &alerts)
                .await
                .is_err()
        );
    }
    store.fault_state_writes_for_test(Some(WriteFault::Fail));

    let error = super::handle_next_delivery_alerting(&store, Arc::new(FailingDispatcher), &alerts)
        .await
        .unwrap_err();

    assert!(matches!(error, super::HandlerError::Store(_)), "{error}");
    assert!(!logs.text().contains("delivery given up"));
    assert!(alert_rx.try_recv().is_err());
    assert_eq!(outbox_row(&store, &event_id).await.attempt_count, 9);

    store.fault_state_writes_for_test(None);
    tokio::time::advance(Duration::from_secs(30)).await;
    assert!(
        super::handle_next_delivery_alerting(&store, Arc::new(FailingDispatcher), &alerts)
            .await
            .unwrap()
    );
    assert_eq!(logs.text().matches("delivery given up").count(), 1);
    let (line, _) = alert_rx.try_recv().unwrap();
    assert_eq!(line, "harold alert: delivery-given-up firing");
    assert!(!store.is_failing());
}

#[tokio::test(start_paused = true)]
async fn a_failing_store_sends_nothing_until_it_recovers() {
    let directory = TestDirectory::new();
    let store = Arc::new(HaroldStore::open(&directory.0).await.unwrap());
    one_reply(&store).await;
    store.fault_log_flush_for_test(Some(WriteFault::Hang));
    assert!(
        append_messaging_paused_changed(&store, false)
            .await
            .is_err()
    );
    store.fault_log_flush_for_test(None);
    let dispatcher = Arc::new(RecordingDispatcher::default());
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
    let task = tokio::spawn(super::run_deliveries(
        Arc::clone(&store),
        dispatcher.clone(),
        None,
        None,
        shutdown_rx,
    ));

    tokio::time::sleep(Duration::from_secs(10)).await;
    assert!(dispatcher.event_types.lock().unwrap().is_empty());
    tokio::time::sleep(Duration::from_secs(20)).await;
    append_messaging_paused_changed(&store, false)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;

    assert_eq!(
        *dispatcher.event_types.lock().unwrap(),
        ["InboundMessageReceived"]
    );
    drop(shutdown_tx);
    task.await.unwrap();
}

// ---------------------------------------------------------------------------
// The app route: with a web app set, the chief's messages go to the app
// ---------------------------------------------------------------------------

/// The daemon's dispatcher with a fake web app and a fake away text send, over a hub
/// with `events`, the screen locked and the message folder set to /work/chief-of-pods.
struct WithApp {
    environment: Arc<FakeEnvironment>,
    app: Arc<FakeApp>,
    away: Arc<FakeAwayText>,
    dispatcher: ProductionDispatcher,
}

async fn with_app(directory: &TestDirectory, events: Vec<AgentEvent>, app: FakeApp) -> WithApp {
    with_app_settings(directory, events, app, ONLY_FROM_CHIEF).await
}

/// `with_app` with `overrides` as the `[notify]` settings.
async fn with_app_settings(
    directory: &TestDirectory,
    events: Vec<AgentEvent>,
    app: FakeApp,
    overrides: &str,
) -> WithApp {
    let store = HaroldStore::open(&directory.0).await.unwrap();
    append_agent_events(&store, events).await.unwrap();
    let snapshots = AgentSnapshotHub::new(store.load_agent_snapshot().await.unwrap());
    project_and_publish_agent_snapshot(&store, &snapshots, 500)
        .await
        .unwrap();
    let environment = Arc::new(FakeEnvironment::locked(overrides));
    let app = Arc::new(app);
    let away = Arc::new(FakeAwayText::default());
    let route: Arc<dyn AppRoute> = app.clone();
    let dispatcher = production_dispatcher(environment.clone(), &snapshots, Some(route), None)
        .with_away_text(away.clone());
    WithApp {
        environment,
        app,
        away,
        dispatcher,
    }
}

fn inventory_unhealthy() -> AgentEvent {
    AgentEvent::MonitorHealthChanged(AgentMonitorHealthChanged {
        component: "inventory".into(),
        healthy: false,
        reason_code: "timeout".into(),
        observed_at_ms: 200,
    })
}

#[tokio::test]
async fn chief_away_message_goes_to_the_app_only() {
    let directory = TestDirectory::new();
    let with = with_app(
        &directory,
        chief_and_pod(),
        FakeApp::new(AppDelivery::Pushed),
    )
    .await;

    let outcome = with.dispatcher.dispatch(&away_delivery(json!(message_from(
        "%1",
        "All pods are green"
    ))));

    assert!(matches!(outcome, Ok(DeliveryOutcome::Delivered)));
    assert!(with.environment.away_messages().is_empty());
    assert!(with.away.sends().is_empty());
    assert_eq!(
        with.app.delivered(),
        [("event-1".to_string(), "All pods are green".to_string())]
    );
    // iMessage did not carry it, so replies are not routed to it.
    assert!(with.environment.reply_targets().is_empty());

    // The pane's mark is still set: its turn-end update for this turn is left out.
    let update = with.dispatcher.dispatch(&turn_delivery("%1", "report"));
    assert!(matches!(update, Ok(DeliveryOutcome::Skipped)));
    assert_eq!(with.app.delivered().len(), 1);
    assert!(with.environment.delivered().is_empty());
}

#[tokio::test]
async fn chief_away_message_not_pushed_goes_to_imessage_once() {
    let directory = TestDirectory::new();
    let with = with_app(
        &directory,
        chief_and_pod(),
        FakeApp::new(AppDelivery::NotPushed(NotPushedReason::NoSubscriptions)),
    )
    .await;

    let outcome = with.dispatcher.dispatch(&away_delivery(json!(message_from(
        "%1",
        "All pods are green"
    ))));

    assert!(matches!(outcome, Ok(DeliveryOutcome::Delivered)));
    let sends = with.away.sends();
    assert_eq!(sends.len(), 1);
    assert!(sends[0].starts_with("(app unreachable) "), "{sends:?}");
    assert_eq!(
        sends[0],
        format!(
            "(app unreachable) {}",
            agent_message_text(FAKE_MACHINE, "work:%1", "All pods are green")
        )
    );
    assert!(with.environment.away_messages().is_empty());
    assert_eq!(with.app.delivered().len(), 1);
    assert_eq!(with.environment.reply_targets(), ["%1"]);
}

#[tokio::test]
async fn other_panes_messages_are_handled_as_today() {
    let mut chief_gone = chief_and_pod();
    chief_gone.push(chief_departed());
    // %2 is on no list, so its message is dropped and its update left out, whether or not
    // the chief runs.
    for (events, expected) in [
        (chief_and_pod(), DeliveryOutcome::Skipped),
        (chief_gone, DeliveryOutcome::Skipped),
    ] {
        let directory = TestDirectory::new();
        let with = with_app(
            &directory,
            events.clone(),
            FakeApp::new(AppDelivery::Pushed),
        )
        .await;
        let other_directory = TestDirectory::new();
        let (store, snapshots, environment, without) =
            only_from_chief(&other_directory, events).await;
        project_and_publish_agent_snapshot(&store, &snapshots, 500)
            .await
            .unwrap();

        let message = away_delivery(json!(message_from("%2", "Pod 3 is blocked")));
        let outcomes: Vec<(DeliveryOutcome, DeliveryOutcome)> = [
            message,
            turn_delivery("%2", "build it"),
            turn_delivery("%2", "test it"),
        ]
        .iter()
        .map(|delivery| {
            (
                with.dispatcher.dispatch(delivery).unwrap(),
                without.dispatch(delivery).unwrap(),
            )
        })
        .collect();

        assert_eq!(outcomes[0], (expected, expected));
        for (routed, as_today) in &outcomes {
            assert_eq!(routed, as_today);
        }
        assert_eq!(
            with.environment.away_messages(),
            environment.away_messages()
        );
        assert_eq!(with.environment.delivered(), environment.delivered());
        assert_eq!(
            with.environment.reply_targets(),
            environment.reply_targets()
        );
        assert!(with.app.delivered().is_empty());
        assert!(with.away.sends().is_empty());
    }
}

#[tokio::test]
async fn locked_chief_spoken_line_goes_to_the_app_as_text_and_makes_no_voice_note() {
    let directory = TestDirectory::new();
    let with = with_app(
        &directory,
        chief_and_pod(),
        FakeApp::new(AppDelivery::Pushed),
    )
    .await;

    let outcome = with
        .dispatcher
        .dispatch(&spoken_delivery(json!(chief_line("All pods are green."))));

    assert!(matches!(outcome, Ok(DeliveryOutcome::Delivered)));
    assert!(with.environment.voice_notes().is_empty());
    assert!(with.environment.spoken_lines().is_empty());
    assert_eq!(
        with.app.delivered(),
        [("event-1".to_string(), "All pods are green.".to_string())]
    );
    assert!(with.away.sends().is_empty());
}

#[tokio::test]
async fn locked_chief_spoken_line_not_pushed_goes_to_imessage_as_text() {
    let directory = TestDirectory::new();
    let with = with_app(
        &directory,
        chief_and_pod(),
        FakeApp::new(AppDelivery::NotPushed(NotPushedReason::NoSubscriptions)),
    )
    .await;

    let outcome = with
        .dispatcher
        .dispatch(&spoken_delivery(json!(chief_line("All pods are green."))));

    assert!(matches!(outcome, Ok(DeliveryOutcome::Delivered)));
    assert_eq!(with.away.sends(), ["(app unreachable) All pods are green."]);
    assert!(with.environment.voice_notes().is_empty());
    assert!(with.environment.spoken_lines().is_empty());
}

#[tokio::test]
async fn unlocked_chief_spoken_line_is_said_at_the_desk_as_today() {
    let directory = TestDirectory::new();
    let with = with_app(
        &directory,
        chief_and_pod(),
        FakeApp::new(AppDelivery::Pushed),
    )
    .await;
    with.environment.set_screen_locked(false);

    let outcome =
        with.dispatcher
            .dispatch_with_pause(&spoken_delivery(json!(chief_line("hi"))), None, false);

    assert!(matches!(outcome, Ok(DeliveryOutcome::Delivered)));
    assert_eq!(with.environment.spoken_lines(), ["hi"]);
    assert!(with.app.delivered().is_empty());
    assert!(with.away.sends().is_empty());
}

#[tokio::test]
async fn chief_message_goes_to_imessage_as_today_while_the_listing_fails() {
    let directory = TestDirectory::new();
    let mut events = chief_and_pod();
    events.push(inventory_unhealthy());
    let with = with_app(&directory, events, FakeApp::new(AppDelivery::Pushed)).await;

    let message = with.dispatcher.dispatch(&away_delivery(json!(message_from(
        "%1",
        "All pods are green"
    ))));
    let line = with
        .dispatcher
        .dispatch(&spoken_delivery(json!(chief_line("hi"))));

    assert!(matches!(message, Ok(DeliveryOutcome::Delivered)));
    assert!(matches!(line, Ok(DeliveryOutcome::Delivered)));
    assert_eq!(with.environment.away_messages(), ["All pods are green"]);
    assert_eq!(with.environment.voice_notes(), ["hi"]);
    assert_eq!(with.environment.reply_targets(), ["%1"]);
    assert!(with.app.delivered().is_empty());
    assert!(with.away.sends().is_empty());
}

#[tokio::test]
async fn a_failed_thread_write_is_retryable_and_sends_nothing() {
    let directory = TestDirectory::new();
    let with = with_app(&directory, chief_and_pod(), FakeApp::failing()).await;

    for delivery in [
        away_delivery(json!(message_from("%1", "All pods are green"))),
        turn_delivery("%1", "report"),
        spoken_delivery(json!(chief_line("hi"))),
    ] {
        let outcome = with.dispatcher.dispatch(&delivery);
        assert!(
            matches!(outcome, Err(DispatchError::Retryable(_))),
            "{}: {outcome:?}",
            delivery.event_type
        );
    }

    assert!(with.away.sends().is_empty());
    assert!(with.environment.away_messages().is_empty());
    assert!(with.environment.delivered().is_empty());
    assert!(with.environment.voice_notes().is_empty());
    assert!(with.environment.reply_targets().is_empty());
}

#[tokio::test]
async fn a_chief_turn_end_records_the_app_as_its_channel_and_the_fallback_as_today() {
    for (delivery, channel) in [
        (AppDelivery::Pushed, "app"),
        (AppDelivery::NotPushed(NotPushedReason::NoSubscriptions), "imessage"),
    ] {
        let directory = TestDirectory::new();
        let with = with_app(&directory, chief_and_pod(), FakeApp::new(delivery)).await;
        let event_id = uuid::Uuid::new_v4();
        let (_, parent) = keep_a_turn_for(event_id);

        let outcome = dispatch_turn_end(&Unpaused(with.dispatcher), &event_id.to_string(), "%1");

        assert!(
            matches!(outcome, Ok(DeliveryOutcome::Delivered)),
            "{channel}"
        );
        assert_eq!(with.app.delivered().len(), 1, "{channel}");
        let deliveries = deliveries_under(parent);
        assert_eq!(deliveries.len(), 1, "{channel}");
        assert_eq!(attr(&deliveries[0], "channel"), channel);
    }
}

// ---------------------------------------------------------------------------
// A reply typed in the web app goes straight to the chief
// ---------------------------------------------------------------------------

/// Each `(pane_id, text)` a recording typer was handed, oldest first.
type TypedKeys = Arc<Mutex<Vec<(String, String)>>>;

/// Each `(text, forwarded)` a recording router was handed, oldest first.
type RoutedTexts = Arc<Mutex<Vec<(String, bool)>>>;

/// A typer that records each `(pane_id, text)` it is handed and types nothing.
fn recording_typer() -> (TypedKeys, ChiefTyper) {
    let typed = TypedKeys::default();
    let keys = typed.clone();
    let typer = Arc::new(move |pane_id: &str, text: &str| {
        keys.lock()
            .unwrap()
            .push((pane_id.to_string(), text.to_string()));
        Ok(())
    });
    (typed, typer)
}

/// A router that records each `(text, forwarded)` it is handed, and reports the reply as
/// another Mac's so the process-wide phone book is left as it was.
fn recording_router() -> (RoutedTexts, Router) {
    let routed = RoutedTexts::default();
    let calls = routed.clone();
    let router: Router = Arc::new(move |text: &str, forwarded: bool| {
        calls.lock().unwrap().push((text.to_string(), forwarded));
        Ok(not_owned_report())
    });
    (routed, router)
}

/// The chief's agent in pane `chief_pane`, in /work/chief-of-pods, and a pod's in %2.
fn chief_in(chief_pane: &str) -> Vec<AgentEvent> {
    vec![
        agent_observed(chief_pane, 10, "/work/chief-of-pods"),
        agent_observed("%2", 20, "/work/harold"),
    ]
}

/// The daemon's dispatcher with `app`, over a hub with `events`, with a recording typer
/// and router in place of tmux.
struct ToChief {
    app: Arc<FakeApp>,
    typed: TypedKeys,
    routed: RoutedTexts,
    dispatcher: ProductionDispatcher,
}

async fn to_chief(directory: &TestDirectory, events: Vec<AgentEvent>, app: FakeApp) -> ToChief {
    let with = with_app(directory, events, app).await;
    let (typed, typer) = recording_typer();
    let (routed, router) = recording_router();
    ToChief {
        app: with.app,
        typed,
        routed,
        dispatcher: with.dispatcher.with_router(router).with_chief_typer(typer),
    }
}

fn inbound_delivery(event_id: &str, payload: serde_json::Value) -> PendingDelivery {
    PendingDelivery {
        event_id: event_id.into(),
        event_version: EventVersion::new(1).unwrap(),
        event_type: "InboundMessageReceived".into(),
        payload,
        trace_id: "trace".into(),
        attempt_count: 0,
    }
}

fn app_reply(text: &str) -> serde_json::Value {
    json!({ "text": text, "forwarded": false, "to_chief": true })
}

#[tokio::test]
async fn a_to_chief_reply_is_typed_into_the_chief_and_nowhere_else() {
    let directory = TestDirectory::new();
    let pane = "%app-chief-typed";
    let with = to_chief(
        &directory,
        chief_in(pane),
        FakeApp::new(AppDelivery::Pushed),
    )
    .await;
    let event_id = unique_event_id();

    let outcome = with.dispatcher.dispatch_with_pause(
        &inbound_delivery(&event_id, app_reply("[harold] hello")),
        None,
        false,
    );

    assert!(
        matches!(outcome, Ok(DeliveryOutcome::Delivered)),
        "{outcome:?}"
    );
    assert_eq!(
        *with.typed.lock().unwrap(),
        [(pane.to_string(), "📱 [harold] hello".to_string())]
    );
    assert!(with.routed.lock().unwrap().is_empty());
    assert_eq!(
        with.app.reply_states(),
        [(event_id, crate::store::AppReplyState::Typed)]
    );
    assert!(with.app.delivered().is_empty());
    let (_, started_by, _) =
        telemetry::take_prompt(pane, time::OffsetDateTime::now_utc()).expect("a prompt is kept");
    assert_eq!(started_by, telemetry::StartedBy::Phone);
}

#[tokio::test]
async fn a_to_chief_reply_with_no_chief_types_nothing_and_says_so() {
    let two_chiefs = vec![
        agent_observed("%app-chief-a", 10, "/work/chief-of-pods"),
        agent_observed("%app-chief-b", 11, "/work/chief-of-pods"),
    ];
    for events in [vec![agent_observed("%2", 20, "/work/harold")], two_chiefs] {
        let directory = TestDirectory::new();
        let with = to_chief(&directory, events, FakeApp::new(AppDelivery::Pushed)).await;
        let event_id = unique_event_id();

        let outcome = with.dispatcher.dispatch_with_pause(
            &inbound_delivery(&event_id, app_reply("hello")),
            None,
            false,
        );

        assert!(
            matches!(outcome, Ok(DeliveryOutcome::Skipped)),
            "{outcome:?}"
        );
        assert!(with.typed.lock().unwrap().is_empty());
        assert!(with.routed.lock().unwrap().is_empty());
        assert_eq!(
            with.app.reply_states(),
            [(event_id, crate::store::AppReplyState::NotSentNoChief)]
        );
    }
}

#[tokio::test]
async fn a_stored_reply_without_to_chief_routes_as_before() {
    let directory = TestDirectory::new();
    let with = to_chief(
        &directory,
        chief_in("%app-chief-not-to"),
        FakeApp::new(AppDelivery::Pushed),
    )
    .await;

    let outcome = with.dispatcher.dispatch_with_pause(
        &inbound_delivery(
            &unique_event_id(),
            json!({ "text": "[pane] hi", "forwarded": false }),
        ),
        None,
        false,
    );

    assert!(
        matches!(outcome, Ok(DeliveryOutcome::Skipped)),
        "{outcome:?}"
    );
    assert_eq!(
        *with.routed.lock().unwrap(),
        [("[pane] hi".to_string(), false)]
    );
    assert!(with.typed.lock().unwrap().is_empty());
    assert!(with.app.reply_states().is_empty());
}

#[tokio::test]
async fn a_paused_to_chief_reply_is_not_typed_and_says_so() {
    let directory = TestDirectory::new();
    let with = to_chief(
        &directory,
        chief_in("%app-chief-paused"),
        FakeApp::new(AppDelivery::Pushed),
    )
    .await;
    let event_id = unique_event_id();

    let outcome = with.dispatcher.dispatch_with_pause(
        &inbound_delivery(&event_id, app_reply("hello")),
        None,
        true,
    );

    assert!(
        matches!(outcome, Ok(DeliveryOutcome::Skipped)),
        "{outcome:?}"
    );
    assert!(with.typed.lock().unwrap().is_empty());
    assert!(with.routed.lock().unwrap().is_empty());
    assert_eq!(
        with.app.reply_states(),
        [(event_id, crate::store::AppReplyState::NotSentPaused)]
    );
}

#[tokio::test]
async fn a_state_write_failure_after_typing_does_not_retype() {
    let directory = TestDirectory::new();
    let pane = "%app-chief-state-fails";
    let with = to_chief(
        &directory,
        chief_in(pane),
        FakeApp::new(AppDelivery::Pushed).failing_reply_states(),
    )
    .await;

    let outcome = with.dispatcher.dispatch_with_pause(
        &inbound_delivery(&unique_event_id(), app_reply("hello")),
        None,
        false,
    );

    assert!(
        matches!(outcome, Ok(DeliveryOutcome::Delivered)),
        "{outcome:?}"
    );
    assert_eq!(
        *with.typed.lock().unwrap(),
        [(pane.to_string(), "📱 hello".to_string())]
    );
    assert_eq!(with.app.reply_states().len(), 1);
}

#[tokio::test]
async fn a_to_chief_reply_while_the_listing_fails_is_retried_and_types_nothing() {
    let directory = TestDirectory::new();
    let mut events = chief_in("%app-chief-listing");
    events.push(inventory_unhealthy());
    let with = to_chief(&directory, events, FakeApp::new(AppDelivery::Pushed)).await;

    let outcome = with.dispatcher.dispatch_with_pause(
        &inbound_delivery(&unique_event_id(), app_reply("hello")),
        None,
        false,
    );

    assert!(
        matches!(outcome, Err(DispatchError::Retryable(_))),
        "{outcome:?}"
    );
    assert!(with.typed.lock().unwrap().is_empty());
    assert!(with.routed.lock().unwrap().is_empty());
    assert!(with.app.reply_states().is_empty());
}

#[tokio::test]
async fn a_to_chief_reply_with_no_app_is_still_typed() {
    let directory = TestDirectory::new();
    let pane = "%app-chief-no-app";
    let store = HaroldStore::open(&directory.0).await.unwrap();
    append_agent_events(&store, chief_in(pane)).await.unwrap();
    let snapshots = AgentSnapshotHub::new(store.load_agent_snapshot().await.unwrap());
    project_and_publish_agent_snapshot(&store, &snapshots, 500)
        .await
        .unwrap();
    let environment = Arc::new(FakeEnvironment::locked(ONLY_FROM_CHIEF));
    let (typed, typer) = recording_typer();
    let dispatcher =
        production_dispatcher(environment, &snapshots, None, None).with_chief_typer(typer);

    let outcome = dispatcher.dispatch_with_pause(
        &inbound_delivery(&unique_event_id(), app_reply("hello")),
        None,
        false,
    );

    assert!(
        matches!(outcome, Ok(DeliveryOutcome::Delivered)),
        "{outcome:?}"
    );
    assert_eq!(
        *typed.lock().unwrap(),
        [(pane.to_string(), "📱 hello".to_string())]
    );
}

// ---------------------------------------------------------------------------
// End to end: no app content reaches a log line or an export
// ---------------------------------------------------------------------------

const APP_CANARY: &str = "APP-CANARY-7731";
const APP_LOGIN: &str = "chief@example.com";

/// The whole app path with a canary: the reply is posted to the app's real router and
/// typed into the chief, and the chief's answer goes through the real `WebAppRoute` and
/// is pushed to a fake push service. Then no `harold.log` line written on the runtime's
/// threads, and no exported log record or span, holds the canary, the push address, the
/// phone's keys or Harold's public key.
///
/// The exporter is process-wide, so the search of the exports covers every test's. Other
/// tests send the same canary and must not leak it either: a failure here is a real
/// leak, wherever it came from. The `harold.log` lines are this test's own: each thread of
/// its runtime, and the test's thread, captures what it writes.
#[test]
fn the_canary_reaches_no_log_line_and_no_export() {
    thread_local! {
        static CAPTURE: std::cell::RefCell<Option<CapturedLogs>> =
            const { std::cell::RefCell::new(None) };
    }
    telemetry::test_support::install();
    let harold_log = Arc::new(Mutex::new(String::new()));
    let kept = Arc::clone(&harold_log);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .on_thread_start(|| {
            CAPTURE.with(|capture| *capture.borrow_mut() = Some(CapturedLogs::start()));
        })
        .on_thread_stop(move || {
            CAPTURE.with(|capture| {
                if let Some(lines) = capture.borrow_mut().take() {
                    kept.lock().unwrap().push_str(&lines.text());
                }
            });
        })
        .build()
        .unwrap();
    let on_test_thread = CapturedLogs::start();

    let secrets = runtime.block_on(run_the_app_path_with_a_canary());
    // Every thread has stopped, and handed in its lines, once the runtime has.
    drop(runtime);
    let harold_log = format!("{}{}", harold_log.lock().unwrap(), on_test_thread.text());

    let logs: Vec<String> = telemetry::test_support::logs()
        .iter()
        .map(|log| format!("{log:?}"))
        .collect();
    let spans: Vec<String> = telemetry::test_support::spans()
        .iter()
        .map(|span| format!("{span:?}"))
        .collect();
    // The path wrote and exported its lines, so the searches below read them.
    for line in ["app reply stored", "app push sent"] {
        assert!(harold_log.contains(line), "{line}: {harold_log}");
        assert!(logs.iter().any(|log| log.contains(line)), "{line}");
    }
    for secret in &secrets {
        assert!(
            !harold_log.contains(secret.as_str()),
            "harold.log holds {secret}: {harold_log}"
        );
        for log in &logs {
            assert!(
                !log.contains(secret.as_str()),
                "a log record holds {secret}: {log}"
            );
        }
        for span in &spans {
            assert!(
                !span.contains(secret.as_str()),
                "a span holds {secret}: {span}"
            );
        }
    }
}

/// Posts the canary to the app, types it into an idle chief, and pushes the chief's
/// answer that holds it, checking each step. Returns what no log may hold: the canary,
/// the push address, the phone's keys and Harold's public key.
async fn run_the_app_path_with_a_canary() -> Vec<String> {
    use crate::store::{AppReplyState, AppSender, append_app_push_subscribed};
    use crate::web_app::fake_push::FakePushService;
    use crate::web_app::origin::loopback_hosts;
    use crate::web_app::push::{Pusher, VapidKey, vapid_key_path};
    use crate::web_app::route::WebAppRoute;
    use crate::web_app::{AppState, router};

    let directory = TestDirectory::new();
    let pane = "%app-canary-chief";
    let store = Arc::new(HaroldStore::open(&directory.0).await.unwrap());
    append_agent_events(&store, chief_in(pane)).await.unwrap();
    let snapshots = AgentSnapshotHub::new(store.load_agent_snapshot().await.unwrap());
    project_and_publish_agent_snapshot(&store, &snapshots, 500)
        .await
        .unwrap();

    // A phone that turned alerts on, at a push service that takes every push.
    let service = FakePushService::start(201).await;
    let subscription = service.subscription(1);
    append_app_push_subscribed(&store, &subscription)
        .await
        .unwrap();
    let key = VapidKey::load_or_create(&vapid_key_path(&directory.0.join("harold.db")))
        .expect("create the VAPID key");
    let public_key = key.public_key_base64url();
    let pusher = Arc::new(Pusher::new(key, APP_LOGIN));
    let route: Arc<dyn AppRoute> = Arc::new(WebAppRoute::new(Arc::clone(&store), pusher));

    // The app's real router, behind its login check, on a free local port.
    let (_shutdown, shutdown_rx) = tokio::sync::watch::channel(());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = AppState {
        store: Arc::clone(&store),
        allowed_login: Arc::from(APP_LOGIN),
        public_key: Arc::from(public_key.as_str()),
        loopback_hosts: Arc::from(loopback_hosts("127.0.0.1:0", addr)),
        shutdown: shutdown_rx,
        calls: None,
        identity: Default::default(),
    };
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router(state)).await;
    });

    let (typed, typer) = recording_typer();
    let (routed, chat_router) = recording_router();
    let away = Arc::new(FakeAwayText::default());
    let environment = Arc::new(FakeEnvironment::locked(ONLY_FROM_CHIEF));
    let dispatcher: Arc<dyn DeliveryDispatcher> = Arc::new(Unpaused(
        production_dispatcher(environment.clone(), &snapshots, Some(route), None)
            .with_router(chat_router)
            .with_chief_typer(typer)
            .with_away_text(away.clone()),
    ));

    // The reply, typed in the app and handed to the chief, with the headers Datastar's
    // `@post` sends from the app's own page.
    let response = reqwest::Client::new()
        .post(format!("http://{addr}/replies"))
        .header("Tailscale-User-Login", APP_LOGIN)
        .header("sec-fetch-site", "same-origin")
        .header("origin", format!("http://{addr}"))
        .header("datastar-request", "true")
        .header("content-type", "application/json")
        .body(json!({ "reply": APP_CANARY }).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    response.text().await.unwrap();
    while handle_next_delivery(&store, Arc::clone(&dispatcher))
        .await
        .unwrap()
    {}

    // The chief's answer, pushed to the phone.
    let answer = format!("{APP_CANARY} from the chief");
    request_message(&store, pane, &answer).await;
    while handle_next_delivery(&store, Arc::clone(&dispatcher))
        .await
        .unwrap()
    {}
    server.abort();

    assert_eq!(
        *typed.lock().unwrap(),
        [(pane.to_string(), format!("📱 {APP_CANARY}"))]
    );
    assert!(routed.lock().unwrap().is_empty());
    assert_eq!(service.requests().len(), 1, "the push was sent");
    assert!(
        away.sends().is_empty(),
        "pushed, so iMessage carries nothing"
    );
    assert!(environment.away_messages().is_empty());
    let rows = store.app_messages_since(0).await.unwrap();
    let rows: Vec<_> = rows
        .iter()
        .map(|row| (row.sender, row.text.as_str(), row.state))
        .collect();
    assert_eq!(
        rows,
        [
            (AppSender::User, APP_CANARY, Some(AppReplyState::Typed)),
            (AppSender::Chief, answer.as_str(), None),
        ]
    );

    vec![
        APP_CANARY.to_owned(),
        subscription.endpoint,
        subscription.p256dh,
        subscription.auth,
        public_key,
    ]
}

// ---------------------------------------------------------------------------
// The channel lists at the dispatcher: each phone step and the desk check their own
// ---------------------------------------------------------------------------

const CHIEF_ON_APP_ONLY: &str = "[notify]\nchief = \"/work/chief-of-pods\"\nweb_app = [\"/work/chief-of-pods\"]\nimessage = []\nmachine_audio_output = [\"/work/chief-of-pods\"]";

#[tokio::test]
async fn chief_message_the_app_cannot_push_is_dropped_when_the_chief_is_not_on_imessage() {
    let directory = TestDirectory::new();
    let with = with_app_settings(
        &directory,
        chief_and_pod(),
        FakeApp::new(AppDelivery::NotPushed(NotPushedReason::NoSubscriptions)),
        CHIEF_ON_APP_ONLY,
    )
    .await;
    let logs = CapturedLogs::start();

    let outcome = with.dispatcher.dispatch(&away_delivery(json!(message_from(
        "%1",
        "All pods are green"
    ))));

    assert!(matches!(outcome, Ok(DeliveryOutcome::Skipped)));
    assert!(with.away.sends().is_empty());
    assert!(with.environment.away_messages().is_empty());
    assert!(with.environment.reply_targets().is_empty());
    let text = logs.text();
    assert!(
        text.contains("agent message dropped (not in notify.imessage) pane_id=%1"),
        "{text}"
    );
    assert!(!text.contains("All pods are green"), "{text}");
}

#[tokio::test]
async fn chief_off_the_web_app_list_goes_by_imessage_and_not_to_the_app() {
    let directory = TestDirectory::new();
    let with = with_app_settings(
        &directory,
        chief_and_pod(),
        FakeApp::new(AppDelivery::Pushed),
        "[notify]\nchief = \"/work/chief-of-pods\"\nweb_app = []\nimessage = [\"/work/chief-of-pods\"]",
    )
    .await;

    let outcome = with.dispatcher.dispatch(&away_delivery(json!(message_from(
        "%1",
        "All pods are green"
    ))));

    assert!(matches!(outcome, Ok(DeliveryOutcome::Delivered)));
    assert!(with.app.delivered().is_empty());
    assert_eq!(with.environment.away_messages(), ["All pods are green"]);
}

#[tokio::test]
async fn spoken_lines_from_unlisted_panes_are_dropped_on_each_channel_naming_its_key() {
    let directory = TestDirectory::new();
    let (store, snapshots, environment, dispatcher) =
        only_from_chief(&directory, chief_and_pod()).await;
    drain(&store, &snapshots, &dispatcher).await;
    let line = |pane: &str| {
        spoken_delivery(json!(SpokenLineRequested {
            pane_id: pane.into(),
            text: "All green.".into(),
        }))
    };
    let logs = CapturedLogs::start();

    // Screen locked: the phone's list.
    assert!(matches!(
        dispatcher.dispatch(&line("%2")),
        Ok(DeliveryOutcome::Skipped)
    ));
    assert!(matches!(
        dispatcher.dispatch(&line("%1")),
        Ok(DeliveryOutcome::Delivered)
    ));
    assert_eq!(environment.voice_notes(), ["All green."]);
    // Screen unlocked: the desk's list.
    environment.set_screen_locked(false);
    assert!(matches!(
        dispatcher.dispatch(&line("%2")),
        Ok(DeliveryOutcome::Skipped)
    ));
    assert!(matches!(
        dispatcher.dispatch(&line("%1")),
        Ok(DeliveryOutcome::Delivered)
    ));
    assert_eq!(environment.spoken_lines(), ["All green."]);
    let text = logs.text();
    assert!(
        text.contains("spoken line dropped (not in notify.imessage) pane_id=%2"),
        "{text}"
    );
    assert!(
        text.contains("spoken line dropped (not in notify.machine_audio_output) pane_id=%2"),
        "{text}"
    );
    assert!(!text.contains("All green."), "{text}");
}

#[tokio::test]
async fn two_panes_matching_chief_are_no_chief_for_the_app_and_still_use_the_lists() {
    let directory = TestDirectory::new();
    let overrides = "[notify]\nchief = \"chief-of-pods\"\nimessage = [\"chief-of-pods\"]\nweb_app = [\"chief-of-pods\"]";
    let events = vec![
        agent_observed("%1", 10, "/Users/x/Dev/p/chief-of-pods"),
        agent_observed("%2", 20, "/tmp/chief-of-pods"),
    ];
    let with = with_app_settings(
        &directory,
        events,
        FakeApp::new(AppDelivery::Pushed),
        overrides,
    )
    .await;
    let logs = CapturedLogs::start();

    for pane in ["%1", "%2"] {
        let outcome = with
            .dispatcher
            .dispatch(&away_delivery(json!(message_from(pane, "hi"))));
        assert!(matches!(outcome, Ok(DeliveryOutcome::Delivered)));
    }

    assert!(with.app.delivered().is_empty());
    assert_eq!(with.environment.away_messages(), ["hi", "hi"]);
    let text = logs.text();
    assert!(text.contains("chief pane changed"), "{text}");
    assert!(text.contains("state=\"ambiguous\""), "{text}");
    assert!(text.contains("count=2"), "{text}");
    assert!(!text.contains("/tmp/chief-of-pods"), "{text}");
}

// ---------------------------------------------------------------------------
// What each agent sent on each channel, and the chief's held-back message
// ---------------------------------------------------------------------------

/// The `harold.notifications` count for an agent, channel and outcome.
fn notified(agent: &str, channel: &str, outcome: &str) -> u64 {
    telemetry::test_support::exact_counter_value(
        "harold.notifications",
        &[("agent", agent), ("channel", channel), ("outcome", outcome)],
    )
    .unwrap_or(0)
}

/// A folder tree no other test uses, so this test's counts are its own.
fn unique_tree() -> String {
    format!("/work/obs-{}", uuid::Uuid::new_v4().simple())
}

/// Settings where only `listed` (a folder) is on the imessage and machine_audio_output
/// lists and `tree/chief-of-pods` is the chief. The other lists let everyone through.
fn lists_naming(tree: &str, listed: &str) -> String {
    format!(
        "[notify]\nchief = \"{tree}/chief-of-pods\"\nimessage = [\"{listed}\"]\nmachine_audio_output = [\"{listed}\"]"
    )
}

/// The daemon's dispatcher over a published snapshot of `events`, queueing alerts into the
/// returned receiver.
async fn published_with_alerts(
    directory: &TestDirectory,
    overrides: &str,
    events: Vec<AgentEvent>,
) -> (
    Arc<FakeEnvironment>,
    ProductionDispatcher,
    tokio::sync::mpsc::Receiver<crate::alerts::Queued>,
) {
    let store = HaroldStore::open(&directory.0).await.unwrap();
    append_agent_events(&store, events).await.unwrap();
    let snapshots = AgentSnapshotHub::new(store.load_agent_snapshot().await.unwrap());
    project_and_publish_agent_snapshot(&store, &snapshots, 500)
        .await
        .unwrap();
    let environment = Arc::new(FakeEnvironment::locked(overrides));
    let (queue, receiver) = crate::alerts::AlertQueue::new();
    let dispatcher = production_dispatcher(environment.clone(), &snapshots, None, Some(queue));
    (environment, dispatcher, receiver)
}

fn queued_lines(receiver: &mut tokio::sync::mpsc::Receiver<crate::alerts::Queued>) -> Vec<String> {
    let mut lines = Vec::new();
    while let Ok((line, _)) = receiver.try_recv() {
        lines.push(line);
    }
    lines
}

fn spoken_from(pane_id: &str) -> PendingDelivery {
    spoken_delivery(json!(SpokenLineRequested {
        pane_id: pane_id.into(),
        text: "SECRET-WORDS".into(),
    }))
}

const CHIEF_ALERT: &str = "harold alert: chief-message-held-back firing";

#[tokio::test]
async fn an_agent_message_a_list_holds_back_counts_kept_and_logs_the_fields() {
    let directory = TestDirectory::new();
    let tree = unique_tree();
    let (environment, dispatcher, _alerts) = published_with_alerts(
        &directory,
        &lists_naming(&tree, &format!("{tree}/chief-of-pods")),
        vec![
            agent_observed("%1", 10, &format!("{tree}/chief-of-pods")),
            agent_observed("%2", 20, &format!("{tree}/harold")),
        ],
    )
    .await;
    let logs = CapturedLogs::start();

    let outcome = dispatcher.dispatch_with_pause(
        &away_delivery(json!(message_from("%2", "SECRET-WORDS"))),
        None,
        false,
    );

    assert!(matches!(outcome, Ok(DeliveryOutcome::Skipped)));
    assert!(environment.away_messages().is_empty());
    let text = logs.text();
    assert!(
        text.contains("agent message dropped (not in notify.imessage) pane_id=%2"),
        "{text}"
    );
    for field in [
        "channel=\"imessage\"",
        "kind=\"agent_message\"",
        "reason=\"not_listed\"",
        &format!("agent=\"{tree}/harold\""),
    ] {
        assert!(text.contains(field), "{field} in {text}");
    }
    assert!(!text.contains("SECRET-WORDS"), "{text}");
    assert_eq!(notified(&format!("{tree}/harold"), "imessage", "kept"), 1);
    assert_eq!(notified(&format!("{tree}/harold"), "imessage", "sent"), 0);
}

#[tokio::test]
async fn a_message_and_a_line_a_channel_took_count_sent() {
    let directory = TestDirectory::new();
    let tree = unique_tree();
    let chief = format!("{tree}/chief-of-pods");
    let (environment, dispatcher, mut alerts) = published_with_alerts(
        &directory,
        &lists_naming(&tree, &chief),
        vec![agent_observed("%1", 10, &chief)],
    )
    .await;

    // Locked: the message and the line go to the away device (imessage).
    for delivery in [
        away_delivery(json!(message_from("%1", "hi"))),
        spoken_from("%1"),
    ] {
        let outcome = dispatcher.dispatch_with_pause(&delivery, None, false);
        assert!(matches!(outcome, Ok(DeliveryOutcome::Delivered)));
    }
    assert_eq!(notified(&chief, "imessage", "sent"), 2);

    // Unlocked: the line is said at the desk.
    environment.set_screen_locked(false);
    let outcome = dispatcher.dispatch_with_pause(&spoken_from("%1"), None, false);
    assert!(matches!(outcome, Ok(DeliveryOutcome::Delivered)));
    assert_eq!(notified(&chief, "machine_audio_output", "sent"), 1);
    assert_eq!(notified(&chief, "imessage", "kept"), 0);
    assert!(queued_lines(&mut alerts).is_empty());
}

#[tokio::test]
async fn a_spoken_line_a_list_holds_back_counts_kept_on_the_phone_and_at_the_desk() {
    let directory = TestDirectory::new();
    let tree = unique_tree();
    let agent = format!("{tree}/harold");
    let (environment, dispatcher, _alerts) = published_with_alerts(
        &directory,
        &lists_naming(&tree, &format!("{tree}/chief-of-pods")),
        vec![agent_observed("%2", 20, &agent)],
    )
    .await;
    let logs = CapturedLogs::start();

    let locked = dispatcher.dispatch_with_pause(&spoken_from("%2"), None, false);
    environment.set_screen_locked(false);
    let desk = dispatcher.dispatch_with_pause(&spoken_from("%2"), None, false);

    assert!(matches!(locked, Ok(DeliveryOutcome::Skipped)));
    assert!(matches!(desk, Ok(DeliveryOutcome::Skipped)));
    let text = logs.text();
    assert!(text.contains("spoken line dropped (not in notify.imessage) pane_id=%2"));
    assert!(text.contains("spoken line dropped (not in notify.machine_audio_output) pane_id=%2"));
    assert!(text.contains("kind=\"spoken_line\""), "{text}");
    assert!(!text.contains("SECRET-WORDS"), "{text}");
    assert_eq!(notified(&agent, "imessage", "kept"), 1);
    assert_eq!(notified(&agent, "machine_audio_output", "kept"), 1);
}

#[tokio::test]
async fn nothing_is_counted_under_the_mute_the_pause_or_on_a_channel_not_set_up() {
    let directory = TestDirectory::new();
    let tree = unique_tree();
    let agent = format!("{tree}/harold");
    let (environment, dispatcher, mut alerts) = published_with_alerts(
        &directory,
        &lists_naming(&tree, &format!("{tree}/someone-else")),
        vec![
            agent_observed("%1", 10, &format!("{tree}/chief-of-pods")),
            agent_observed("%2", 20, &agent),
        ],
    )
    .await;

    // The pause is looked at before the list.
    let paused =
        dispatcher.dispatch_with_pause(&away_delivery(json!(message_from("%1", "x"))), None, true);
    assert!(matches!(paused, Ok(DeliveryOutcome::Skipped)));

    // The voice mute is looked at before the desk list.
    environment.set_screen_locked(false);
    crate::outbound::set_tts_muted(true);
    let muted = dispatcher.dispatch_with_pause(&spoken_from("%1"), None, false);
    crate::outbound::set_tts_muted(false);
    assert!(matches!(muted, Ok(DeliveryOutcome::Skipped)));

    // A channel that is not set up counts nothing, though its line is still written.
    environment.set_screen_locked(true);
    environment.set_not_set_up(Channel::Imessage);
    let logs = CapturedLogs::start();
    let unset =
        dispatcher.dispatch_with_pause(&away_delivery(json!(message_from("%1", "x"))), None, false);
    assert!(matches!(unset, Ok(DeliveryOutcome::Skipped)));
    assert!(
        logs.text()
            .contains("agent message dropped (not in notify.imessage)")
    );

    assert_eq!(notified(&agent, "imessage", "kept"), 0);
    assert_eq!(
        notified(&format!("{tree}/chief-of-pods"), "imessage", "kept"),
        0
    );
    assert_eq!(
        notified(
            &format!("{tree}/chief-of-pods"),
            "machine_audio_output",
            "kept"
        ),
        0
    );
    assert!(
        queued_lines(&mut alerts).is_empty(),
        "no alert for choices or unset channels"
    );
}

#[tokio::test]
async fn a_chief_message_or_line_a_list_holds_back_queues_one_alert() {
    let directory = TestDirectory::new();
    let tree = unique_tree();
    let (environment, dispatcher, mut alerts) = published_with_alerts(
        &directory,
        &lists_naming(&tree, &format!("{tree}/someone-else")),
        vec![
            agent_observed("%1", 10, &format!("{tree}/chief-of-pods")),
            agent_observed("%2", 20, &format!("{tree}/harold")),
        ],
    )
    .await;

    // Phone: a message and a line. Desk: a line. Each queues its own alert line.
    for delivery in [
        away_delivery(json!(message_from("%1", "hi"))),
        spoken_from("%1"),
    ] {
        dispatcher
            .dispatch_with_pause(&delivery, None, false)
            .unwrap();
    }
    environment.set_screen_locked(false);
    dispatcher
        .dispatch_with_pause(&spoken_from("%1"), None, false)
        .unwrap();
    assert_eq!(
        queued_lines(&mut alerts),
        [CHIEF_ALERT, CHIEF_ALERT, CHIEF_ALERT]
    );

    // Another agent's message, and a turn-end update of the chief, queue none.
    environment.set_screen_locked(true);
    dispatcher
        .dispatch_with_pause(&away_delivery(json!(message_from("%2", "hi"))), None, false)
        .unwrap();
    dispatcher
        .dispatch_with_pause(&turn_delivery("%1", "build it"), None, false)
        .unwrap();
    assert!(queued_lines(&mut alerts).is_empty());
}

#[tokio::test]
async fn the_alert_is_queued_while_the_chief_pane_is_ambiguous() {
    let directory = TestDirectory::new();
    let tree = unique_tree();
    let (_environment, dispatcher, mut alerts) = published_with_alerts(
        &directory,
        &lists_naming(&tree, &format!("{tree}/someone-else")),
        vec![
            agent_observed("%1", 10, &format!("{tree}/chief-of-pods")),
            agent_observed("%3", 30, &format!("{tree}/chief-of-pods/")),
        ],
    )
    .await;

    dispatcher
        .dispatch_with_pause(&away_delivery(json!(message_from("%1", "hi"))), None, false)
        .unwrap();

    assert_eq!(queued_lines(&mut alerts), [CHIEF_ALERT]);
}

#[tokio::test]
async fn the_delivery_span_of_a_held_back_update_names_the_holding_channel_and_outcome() {
    let directory = TestDirectory::new();
    let tree = unique_tree();
    let (_environment, dispatcher, _alerts) = published_with_alerts(
        &directory,
        &lists_naming(&tree, &format!("{tree}/chief-of-pods")),
        vec![agent_observed("%2", 20, &format!("{tree}/harold"))],
    )
    .await;
    let event_id = uuid::Uuid::new_v4();
    let (_trace, parent) = keep_a_turn_for(event_id);

    let outcome = dispatch_turn_end(&Unpaused(dispatcher), &event_id.to_string(), "%2");

    assert!(matches!(
        outcome,
        Ok(DeliveryOutcome::HeldBack {
            reason: "not_listed"
        })
    ));
    let deliveries = deliveries_under(parent);
    assert_eq!(deliveries.len(), 1);
    assert_eq!(attr(&deliveries[0], "outcome"), "held_back");
    assert_eq!(attr(&deliveries[0], "reason"), "not_listed");
    let channels: Vec<_> = deliveries[0]
        .attributes
        .iter()
        .filter(|kv| kv.key.as_str() == "channel")
        .collect();
    assert_eq!(channels.len(), 1, "recorded once: {channels:?}");
    assert_eq!(attr(&deliveries[0], "channel"), "imessage");
}
