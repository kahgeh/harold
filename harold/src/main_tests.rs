use std::io::Write;
use std::sync::{Arc, Mutex};

use events::EventStreamVersion;
use prost::Message;
use tokio::sync::watch;
use tokio_stream::StreamExt;

use super::agent::domain::{
    AgentEvent, AgentIncarnation, AgentLifecycleObserved, AgentPaneObservation, AgentPaneObserved,
    AgentPaneProjection, AgentSnapshot, CompletionSummaryUpdate, EffectiveAgentState,
    MonitorHealthProjection, ObservedAgentState, WorkSummaryUpdate,
};
use super::agent::inventory::{AgentInventoryPort, InventoryError};
use super::agent::runtime::spawn_agent_monitor_for_test;
use super::agent::screen::TmuxVisibleScreen;
use super::agent::snapshot::AgentSnapshotHub;
use super::harold::harold_server::Harold;
use super::harold::{
    AgentState, MonitorHealthState, ReportAgentStateRequest, WatchAgentStatesRequest,
};
use super::{
    HaroldService, Request, TurnCompleteRequest, load_startup_agent_snapshot, pane_id_for_log,
    store,
};

struct EmptyInventory;

struct CapturedLogWriter(Arc<Mutex<Vec<u8>>>);

impl Write for CapturedLogWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("captured log lock").extend(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn turn_complete_log_identifier_is_bounded_and_shape_checked() {
    assert_eq!(pane_id_for_log("%88"), "%88");
    assert_eq!(pane_id_for_log("not-a-pane"), "<invalid-pane-id>");
    assert_eq!(
        pane_id_for_log(&format!("%{}", "8".repeat(64))),
        "<invalid-pane-id>"
    );
}

impl AgentInventoryPort for EmptyInventory {
    fn scan(&self) -> Result<Vec<AgentPaneObservation>, InventoryError> {
        Ok(Vec::new())
    }

    fn resolve(&self, _pane_id: &str) -> Result<Option<AgentPaneObservation>, InventoryError> {
        Ok(None)
    }

    fn is_current(&self, _incarnation: &AgentIncarnation) -> Result<bool, InventoryError> {
        Ok(false)
    }
}

fn test_service(
    store: Arc<store::HaroldStore>,
    snapshot: AgentSnapshot,
) -> (
    HaroldService,
    watch::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    test_service_with_inventory(store, snapshot, Arc::new(EmptyInventory))
}

fn test_service_with_inventory<I>(
    store: Arc<store::HaroldStore>,
    snapshot: AgentSnapshot,
    inventory: Arc<I>,
) -> (
    HaroldService,
    watch::Sender<()>,
    tokio::task::JoinHandle<()>,
)
where
    I: AgentInventoryPort + 'static,
{
    let (shutdown, shutdown_rx) = watch::channel(());
    let (monitor, task) = spawn_agent_monitor_for_test(
        store,
        inventory,
        Arc::new(TmuxVisibleScreen::new()),
        Vec::new(),
        2_000,
        shutdown_rx.clone(),
    );
    (
        HaroldService {
            monitor,
            snapshots: AgentSnapshotHub::new(snapshot),
            shutdown: shutdown_rx,
        },
        shutdown,
        task,
    )
}

#[derive(Clone)]
struct ResolvingInventory {
    pane: Option<AgentPaneObservation>,
}

impl AgentInventoryPort for ResolvingInventory {
    fn scan(&self) -> Result<Vec<AgentPaneObservation>, InventoryError> {
        Ok(self.pane.clone().into_iter().collect())
    }

    fn resolve(&self, pane_id: &str) -> Result<Option<AgentPaneObservation>, InventoryError> {
        Ok(self
            .pane
            .as_ref()
            .filter(|pane| pane.incarnation.pane_id == pane_id)
            .cloned())
    }

    fn is_current(&self, incarnation: &AgentIncarnation) -> Result<bool, InventoryError> {
        Ok(self
            .pane
            .as_ref()
            .is_some_and(|pane| pane.incarnation == *incarnation))
    }
}

fn resolved_pane() -> AgentPaneObservation {
    AgentPaneObservation {
        incarnation: AgentIncarnation {
            pane_id: "%8".into(),
            pane_pid: 81,
            agent_pid: 82,
            agent_started_at_ms: 8_000,
            provider_id: "codex".into(),
        },
        tmux_target: "harold:2.3".into(),
        session_name: "harold".into(),
        window_index: 2,
        pane_index: 3,
        working_directory: "/work/harold".into(),
        provider_display_name: "Codex".into(),
        observed_at_ms: 8_500,
    }
}

fn empty_snapshot() -> AgentSnapshot {
    AgentSnapshot {
        through_event_version: EventStreamVersion::start(),
        server_time_ms: 1_000,
        monitor_health: Vec::new(),
        panes: Vec::new(),
    }
}

fn populated_snapshot(revision: i64, summary: Option<&str>) -> AgentSnapshot {
    AgentSnapshot {
        through_event_version: EventStreamVersion::new(revision).expect("valid revision"),
        server_time_ms: 10_000 + revision,
        monitor_health: vec![MonitorHealthProjection {
            component: "screen".into(),
            healthy: false,
            reason_code: "capture_failed".into(),
            observed_at_ms: 9_800,
            last_event_version: EventStreamVersion::new(revision).expect("valid revision"),
        }],
        panes: vec![AgentPaneProjection {
            pane: AgentPaneObservation {
                incarnation: AgentIncarnation {
                    pane_id: "%8".into(),
                    pane_pid: 81,
                    agent_pid: 82,
                    agent_started_at_ms: 8_000,
                    provider_id: "codex".into(),
                },
                tmux_target: "harold:2.3".into(),
                session_name: "harold".into(),
                window_index: 2,
                pane_index: 3,
                working_directory: "/work/harold".into(),
                provider_display_name: "Codex".into(),
                observed_at_ms: 8_500,
            },
            hook_state: None,
            hook_observed_at_ms: None,
            screen_state: None,
            screen_classifier_id: None,
            screen_observed_at_ms: None,
            effective_state: EffectiveAgentState::Busy,
            explicit_work_summary: summary.map(str::to_string),
            explicit_work_summary_updated_at_ms: summary.map(|_| 8_600),
            screen_work_summary: None,
            screen_work_summary_updated_at_ms: None,
            work_summary: summary.map(str::to_string),
            last_transition_at_ms: 8_700,
            last_event_version: EventStreamVersion::new(revision).expect("valid revision"),
        }],
    }
}

struct TestDirectory(std::path::PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("harold-service-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("create test directory");
        Self(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn turn_complete_accepts_only_after_appending_every_request_field() {
    let directory = TestDirectory::new();
    let store = Arc::new(store::HaroldStore::open(&directory.0).await.unwrap());
    let (service, _shutdown, task) = test_service(Arc::clone(&store), empty_snapshot());
    let request = TurnCompleteRequest {
        pane_id: "%8".into(),
        pane_label: "harold:0.8".into(),
        last_user_prompt: "refresh events".into(),
        assistant_message: "events refreshed".into(),
        main_context: "harold".into(),
    };

    let response = service
        .turn_complete(Request::new(request))
        .await
        .unwrap()
        .into_inner();
    assert!(response.accepted);

    let events = store
        .stream()
        .load_after_version(EventStreamVersion::start(), 10)
        .await
        .unwrap();
    assert_eq!(events.len(), 1);
    let event: store::TurnCompleted = serde_json::from_value(events[0].payload.clone()).unwrap();
    assert_eq!(event.pane_id, "%8");
    assert_eq!(event.pane_label, "harold:0.8");
    assert_eq!(event.last_user_prompt, "refresh events");
    assert_eq!(event.assistant_message, "events refreshed");
    assert_eq!(event.main_context, "harold");
    assert_eq!(event.agent_incarnation, None);
    assert_eq!(
        event.work_summary,
        super::agent::domain::CompletionSummaryUpdate::Set("refresh events".into())
    );
    task.abort();
}

#[tokio::test]
async fn report_agent_state_preserves_summary_presence_and_normalizes_before_durable_append() {
    let directory = TestDirectory::new();
    let store = Arc::new(store::HaroldStore::open(&directory.0).await.unwrap());
    let expected_incarnation = resolved_pane().incarnation.clone();
    let inventory = Arc::new(ResolvingInventory {
        pane: Some(resolved_pane()),
    });
    let (service, _shutdown, task) =
        test_service_with_inventory(Arc::clone(&store), empty_snapshot(), inventory);
    let oversized = "🦀".repeat(161);
    let cases = [
        (AgentState::Busy, None, WorkSummaryUpdate::Unchanged),
        (
            AgentState::Idle,
            Some(" \u{1b}[31m \t"),
            WorkSummaryUpdate::Clear,
        ),
        (
            AgentState::Busy,
            Some("  refresh\n\u{2003}events  "),
            WorkSummaryUpdate::Set("refresh events".into()),
        ),
        (
            AgentState::Idle,
            Some(oversized.as_str()),
            WorkSummaryUpdate::Set("🦀".repeat(160)),
        ),
    ];

    for (state, work_summary, _) in &cases {
        let response = service
            .report_agent_state(Request::new(ReportAgentStateRequest {
                pane_id: "%8".into(),
                state: i32::from(*state),
                adapter_id: "codex-hook".into(),
                work_summary: work_summary.map(str::to_string),
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(response.accepted);
    }

    task.abort();
    task.await.expect_err("monitor task should be cancelled");
    drop(service);
    drop(store);

    let reopened = store::HaroldStore::open(&directory.0).await.unwrap();
    let events = reopened
        .stream()
        .load_after_version(EventStreamVersion::start(), 20)
        .await
        .unwrap();
    assert_eq!(events.len(), cases.len() * 2);
    for (index, (state, _, expected_summary)) in cases.into_iter().enumerate() {
        let pane: AgentPaneObserved =
            serde_json::from_value(events[index * 2].payload.clone()).unwrap();
        let lifecycle: AgentLifecycleObserved =
            serde_json::from_value(events[index * 2 + 1].payload.clone()).unwrap();
        assert_eq!(pane.pane.incarnation, expected_incarnation);
        assert_eq!(lifecycle.incarnation, expected_incarnation);
        assert_eq!(
            lifecycle.state,
            match state {
                AgentState::Busy => ObservedAgentState::Busy,
                AgentState::Idle => ObservedAgentState::Idle,
                _ => unreachable!("test table contains only actionable states"),
            }
        );
        assert_eq!(lifecycle.work_summary, expected_summary);
    }
}

#[tokio::test]
async fn report_agent_state_rejects_malformed_inputs_without_durable_events() {
    let directory = TestDirectory::new();
    let store = Arc::new(store::HaroldStore::open(&directory.0).await.unwrap());
    let inventory = Arc::new(ResolvingInventory {
        pane: Some(resolved_pane()),
    });
    let (service, _shutdown, task) =
        test_service_with_inventory(Arc::clone(&store), empty_snapshot(), inventory);
    let cases = [
        ("%8", i32::from(AgentState::Unspecified), "codex-hook"),
        ("%8", i32::from(AgentState::Unknown), "codex-hook"),
        ("%8", 99, "codex-hook"),
        ("8", i32::from(AgentState::Busy), "codex-hook"),
        ("%8", i32::from(AgentState::Busy), "Codex hook"),
    ];

    for (pane_id, state, adapter_id) in cases {
        let error = service
            .report_agent_state(Request::new(ReportAgentStateRequest {
                pane_id: pane_id.into(),
                state,
                adapter_id: adapter_id.into(),
                work_summary: Some("sensitive request content".into()),
            }))
            .await
            .unwrap_err();
        assert_eq!(error.code(), tonic::Code::InvalidArgument);
        assert!(!error.message().contains("sensitive request content"));
    }

    let events = store
        .stream()
        .load_after_version(EventStreamVersion::start(), 20)
        .await
        .unwrap();
    assert!(events.is_empty());
    task.abort();
}

#[tokio::test]
async fn report_agent_state_maps_unresolved_append_and_runtime_failures_to_bounded_statuses() {
    let directory = TestDirectory::new();
    let store = Arc::new(store::HaroldStore::open(&directory.0).await.unwrap());
    let (unresolved_service, _shutdown, unresolved_task) =
        test_service(Arc::clone(&store), empty_snapshot());
    let request = || ReportAgentStateRequest {
        pane_id: "%8".into(),
        state: AgentState::Busy.into(),
        adapter_id: "codex-hook".into(),
        work_summary: Some("sensitive request content".into()),
    };

    let unresolved = unresolved_service
        .report_agent_state(Request::new(request()))
        .await
        .unwrap_err();
    assert_eq!(unresolved.code(), tonic::Code::FailedPrecondition);
    assert!(!unresolved.message().contains("sensitive request content"));
    unresolved_task.abort();

    let inventory = Arc::new(ResolvingInventory {
        pane: Some(resolved_pane()),
    });
    let (append_service, _shutdown, append_task) =
        test_service_with_inventory(Arc::clone(&store), empty_snapshot(), Arc::clone(&inventory));
    store.fail_next_monitor_append_for_test();
    let append = append_service
        .report_agent_state(Request::new(request()))
        .await
        .unwrap_err();
    assert_eq!(append.code(), tonic::Code::Unavailable);
    assert!(!append.message().contains("sensitive request content"));
    append_task.abort();

    let (stopped_service, shutdown, stopped_task) =
        test_service_with_inventory(Arc::clone(&store), empty_snapshot(), inventory);
    drop(shutdown);
    stopped_task.await.unwrap();
    let stopped = stopped_service
        .report_agent_state(Request::new(request()))
        .await
        .unwrap_err();
    assert_eq!(stopped.code(), tonic::Code::Unavailable);
    assert!(!stopped.message().contains("sensitive request content"));

    let events = store
        .stream()
        .load_after_version(EventStreamVersion::start(), 20)
        .await
        .unwrap();
    assert!(events.is_empty());
}

#[tokio::test]
async fn report_agent_state_keeps_normalized_empty_legacy_completion_summary_unchanged() {
    let directory = TestDirectory::new();
    let store = Arc::new(store::HaroldStore::open(&directory.0).await.unwrap());
    let inventory = Arc::new(ResolvingInventory {
        pane: Some(resolved_pane()),
    });
    let (service, _shutdown, task) =
        test_service_with_inventory(Arc::clone(&store), empty_snapshot(), inventory);
    let response = service
        .turn_complete(Request::new(TurnCompleteRequest {
            pane_id: "%8".into(),
            pane_label: "harold:2.3".into(),
            last_user_prompt: " \u{1b}[31m \t".into(),
            assistant_message: "done".into(),
            main_context: "harold".into(),
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(response.accepted);

    let events = store
        .stream()
        .load_after_version(EventStreamVersion::start(), 10)
        .await
        .unwrap();
    assert_eq!(events.len(), 2);
    let event: store::TurnCompleted = serde_json::from_value(events[1].payload.clone()).unwrap();
    assert_eq!(event.agent_incarnation, Some(resolved_pane().incarnation));
    assert_eq!(event.work_summary, CompletionSummaryUpdate::Unchanged);
    task.abort();
}

#[tokio::test]
async fn explicit_summary_survives_event_projection_watch_and_restart_with_full_incarnation() {
    let directory = TestDirectory::new();
    let store = Arc::new(store::HaroldStore::open(&directory.0).await.unwrap());
    let expected_pane = resolved_pane();
    let inventory = Arc::new(ResolvingInventory {
        pane: Some(expected_pane.clone()),
    });
    let (service, _shutdown, task) =
        test_service_with_inventory(Arc::clone(&store), empty_snapshot(), inventory);

    let response = service
        .report_agent_state(Request::new(ReportAgentStateRequest {
            pane_id: expected_pane.incarnation.pane_id.clone(),
            state: AgentState::Busy.into(),
            adapter_id: "codex-hook".into(),
            work_summary: Some("Implement projector".into()),
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(response.accepted);

    let events = store
        .stream()
        .load_after_version(EventStreamVersion::start(), 10)
        .await
        .unwrap();
    assert_eq!(events.len(), 2);
    let observed: AgentPaneObserved = serde_json::from_value(events[0].payload.clone()).unwrap();
    let lifecycle: AgentLifecycleObserved =
        serde_json::from_value(events[1].payload.clone()).unwrap();
    assert_eq!(observed.pane.incarnation, expected_pane.incarnation);
    assert_eq!(lifecycle.incarnation, expected_pane.incarnation);
    assert_eq!(lifecycle.state, ObservedAgentState::Busy);
    assert_eq!(
        lifecycle.work_summary,
        WorkSummaryUpdate::Set("Implement projector".into())
    );

    super::projector::project_and_publish_agent_snapshot(&store, &service.snapshots, 500)
        .await
        .unwrap();
    let mut stream = service
        .watch_agent_states(Request::new(WatchAgentStatesRequest {}))
        .await
        .unwrap()
        .into_inner();
    let projected = stream.next().await.expect("projected snapshot").unwrap();
    assert_watched_busy_incarnation(&projected, &expected_pane, "Implement projector");

    task.abort();
    task.await.expect_err("monitor task should be cancelled");
    drop(stream);
    drop(service);
    drop(store);

    let reopened = Arc::new(store::HaroldStore::open(&directory.0).await.unwrap());
    let recovered = load_startup_agent_snapshot(&reopened).await.unwrap();
    assert_eq!(
        recovered.panes[0].work_summary.as_deref(),
        Some("Implement projector")
    );
    assert_eq!(
        recovered.panes[0].pane.incarnation,
        expected_pane.incarnation
    );
    let (restarted_service, _shutdown, restarted_task) =
        test_service(Arc::clone(&reopened), recovered);
    let mut restarted_stream = restarted_service
        .watch_agent_states(Request::new(WatchAgentStatesRequest {}))
        .await
        .unwrap()
        .into_inner();
    let restarted = restarted_stream
        .next()
        .await
        .expect("restart snapshot")
        .unwrap();
    assert_watched_busy_incarnation(&restarted, &expected_pane, "Implement projector");
    restarted_task.abort();
}

fn assert_watched_busy_incarnation(
    snapshot: &super::harold::AgentStateSnapshot,
    expected: &AgentPaneObservation,
    summary: &str,
) {
    assert_eq!(snapshot.panes.len(), 1);
    let pane = &snapshot.panes[0];
    assert_eq!(pane.pane_id, expected.incarnation.pane_id);
    assert_eq!(pane.pane_pid, expected.incarnation.pane_pid);
    assert_eq!(pane.agent_pid, expected.incarnation.agent_pid);
    assert_eq!(
        pane.agent_started_at_ms,
        expected.incarnation.agent_started_at_ms
    );
    assert_eq!(pane.provider_id, expected.incarnation.provider_id);
    assert_eq!(pane.state, i32::from(AgentState::Busy));
    assert_eq!(pane.work_summary.as_deref(), Some(summary));
}

#[tokio::test]
async fn raw_summary_sentinel_is_absent_from_status_and_protobuf_diagnostics() {
    const RAW_SCREEN_SENTINEL: &str = "RAW_SCREEN_SENTINEL_TASK_11";

    let directory = TestDirectory::new();
    let store = Arc::new(store::HaroldStore::open(&directory.0).await.unwrap());
    let inventory = Arc::new(ResolvingInventory {
        pane: Some(resolved_pane()),
    });
    let (service, _shutdown, task) =
        test_service_with_inventory(Arc::clone(&store), empty_snapshot(), inventory);
    let captured_logs = Arc::new(Mutex::new(Vec::new()));
    let log_writer = Arc::clone(&captured_logs);
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_writer(move || CapturedLogWriter(Arc::clone(&log_writer)))
        .finish();
    let _subscriber_guard = tracing::subscriber::set_default(subscriber);
    let error = service
        .report_agent_state(Request::new(ReportAgentStateRequest {
            pane_id: "not-a-pane".into(),
            state: AgentState::Busy.into(),
            adapter_id: "codex-hook".into(),
            work_summary: Some(RAW_SCREEN_SENTINEL.into()),
        }))
        .await
        .unwrap_err();
    assert!(!format!("{error:?}").contains(RAW_SCREEN_SENTINEL));

    store::append_agent_events(
        &store,
        vec![
            AgentEvent::PaneObserved(AgentPaneObserved {
                pane: resolved_pane(),
            }),
            AgentEvent::ScreenObserved(super::agent::domain::AgentScreenObserved {
                incarnation: resolved_pane().incarnation,
                state: Some(ObservedAgentState::Busy),
                classifier_id: "tmux-visible-v1".into(),
                fallback_summary: Some(format!(
                    "\u{1b}P{RAW_SCREEN_SENTINEL}\u{1b}\\Review\t tests"
                )),
                observed_at_ms: 8_600,
            }),
        ],
    )
    .await
    .unwrap();

    let mut watch = service
        .watch_agent_states(Request::new(WatchAgentStatesRequest {}))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        watch
            .next()
            .await
            .expect("initial snapshot")
            .unwrap()
            .through_event_version,
        0
    );
    let (handler_shutdown, handler_shutdown_rx) = tokio::sync::watch::channel(());
    let handler = tokio::spawn(super::projector::run_event_handler(
        Arc::clone(&store),
        service.snapshots.clone(),
        handler_shutdown_rx,
    ));
    let snapshot = tokio::time::timeout(std::time::Duration::from_secs(1), watch.next())
        .await
        .expect("projector did not publish")
        .expect("watch closed before projection")
        .unwrap();
    assert_eq!(
        snapshot.panes[0].work_summary.as_deref(),
        Some("Review tests")
    );
    assert!(!format!("{snapshot:?}").contains(RAW_SCREEN_SENTINEL));
    assert!(
        !snapshot
            .encode_to_vec()
            .windows(RAW_SCREEN_SENTINEL.len())
            .any(|window| window == RAW_SCREEN_SENTINEL.as_bytes())
    );
    drop(handler_shutdown);
    tokio::time::timeout(std::time::Duration::from_secs(1), handler)
        .await
        .expect("event handler exceeded shutdown deadline")
        .expect("event handler task panicked");
    let logs = String::from_utf8(captured_logs.lock().expect("captured log lock").clone())
        .expect("logs are utf-8");
    assert!(logs.contains("event handler starting"));
    assert!(!logs.contains(RAW_SCREEN_SENTINEL));
    task.abort();
}

#[test]
fn protobuf_keeps_summary_optional_and_search_provenance_and_timestamp_local() {
    let schema = include_str!("../../harold-api/proto/harold.proto");
    assert!(schema.contains("optional string work_summary = 14;"));
    for forbidden in [
        "work_summary_updated_at_ms",
        "rpc Search",
        "rpc Query",
        "search_term",
        "query",
        "provenance",
    ] {
        assert!(
            !schema.contains(forbidden),
            "public protobuf unexpectedly contains {forbidden}"
        );
    }
}

#[tokio::test]
async fn watch_sends_complete_current_snapshot_then_coalesced_newer_truth() {
    let directory = TestDirectory::new();
    let store = Arc::new(store::HaroldStore::open(&directory.0).await.unwrap());
    let (service, _shutdown, task) =
        test_service(store, populated_snapshot(4, Some("index events")));

    let mut stream = service
        .watch_agent_states(Request::new(WatchAgentStatesRequest {}))
        .await
        .unwrap()
        .into_inner();
    let initial = stream.next().await.expect("initial message").unwrap();
    assert_eq!(initial.through_event_version, 4);
    assert_eq!(initial.server_time_ms, 10_004);
    assert_eq!(initial.monitor_health.len(), 1);
    assert_eq!(initial.monitor_health[0].component, "screen");
    assert_eq!(
        initial.monitor_health[0].state,
        i32::from(MonitorHealthState::Degraded)
    );
    assert_eq!(initial.monitor_health[0].reason_code, "capture_failed");
    assert_eq!(initial.monitor_health[0].observed_at_ms, 9_800);
    assert_eq!(initial.panes.len(), 1);
    let pane = &initial.panes[0];
    assert_eq!(pane.pane_id, "%8");
    assert_eq!(pane.tmux_target, "harold:2.3");
    assert_eq!(pane.session_name, "harold");
    assert_eq!(pane.window_index, 2);
    assert_eq!(pane.pane_index, 3);
    assert_eq!(pane.pane_pid, 81);
    assert_eq!(pane.agent_pid, 82);
    assert_eq!(pane.agent_started_at_ms, 8_000);
    assert_eq!(pane.provider_id, "codex");
    assert_eq!(pane.provider_display_name, "Codex");
    assert_eq!(pane.working_directory, "/work/harold");
    assert_eq!(pane.state, i32::from(AgentState::Busy));
    assert_eq!(pane.last_transition_at_ms, 8_700);
    assert_eq!(pane.work_summary.as_deref(), Some("index events"));

    service
        .snapshots
        .publish_committed(populated_snapshot(5, Some("intermediate")));
    service
        .snapshots
        .publish_committed(populated_snapshot(6, None));
    let current = stream.next().await.expect("changed message").unwrap();
    assert_eq!(current.through_event_version, 6);
    assert_eq!(current.panes[0].work_summary, None);

    let mut reconnected = service
        .watch_agent_states(Request::new(WatchAgentStatesRequest {}))
        .await
        .unwrap()
        .into_inner();
    let reconnected_current = reconnected.next().await.expect("current message").unwrap();
    assert_eq!(reconnected_current.through_event_version, 6);
    assert_eq!(reconnected_current.panes[0].work_summary, None);
    task.abort();
}

#[tokio::test]
async fn watch_stream_closes_within_shutdown_deadline() {
    let directory = TestDirectory::new();
    let store = Arc::new(store::HaroldStore::open(&directory.0).await.unwrap());
    let (service, shutdown, task) = test_service(store, empty_snapshot());
    let mut stream = service
        .watch_agent_states(Request::new(WatchAgentStatesRequest {}))
        .await
        .unwrap()
        .into_inner();
    stream.next().await.expect("initial message").unwrap();
    let mut second_stream = service
        .watch_agent_states(Request::new(WatchAgentStatesRequest {}))
        .await
        .unwrap()
        .into_inner();
    second_stream
        .next()
        .await
        .expect("second initial message")
        .unwrap();

    drop(shutdown);

    let (closed, second_closed) = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        tokio::join!(stream.next(), second_stream.next())
    })
    .await
    .expect("watch forwarders exceeded shutdown deadline");
    assert!(closed.is_none());
    assert!(second_closed.is_none());
    tokio::time::timeout(std::time::Duration::from_secs(1), task)
        .await
        .expect("monitor exceeded shutdown deadline")
        .expect("monitor task panicked");
}

#[tokio::test]
async fn startup_catches_up_and_recovers_stored_agent_truth_before_seeding() {
    let directory = TestDirectory::new();
    let store = store::HaroldStore::open(&directory.0).await.unwrap();
    store::append_agent_events(
        &store,
        vec![AgentEvent::PaneObserved(AgentPaneObserved {
            pane: AgentPaneObservation {
                incarnation: AgentIncarnation {
                    pane_id: "%14".into(),
                    pane_pid: 141,
                    agent_pid: 142,
                    agent_started_at_ms: 14_000,
                    provider_id: "claude".into(),
                },
                tmux_target: "work:4.1".into(),
                session_name: "work".into(),
                window_index: 4,
                pane_index: 1,
                working_directory: "/work/app".into(),
                provider_display_name: "Claude".into(),
                observed_at_ms: 14_100,
            },
        })],
    )
    .await
    .unwrap();

    let first = load_startup_agent_snapshot(&store).await.unwrap();
    assert_eq!(first.through_event_version.get(), 1);
    assert_eq!(first.panes[0].pane.incarnation.pane_id, "%14");
    drop(store);

    let reopened = store::HaroldStore::open(&directory.0).await.unwrap();
    let recovered = load_startup_agent_snapshot(&reopened).await.unwrap();
    assert_eq!(recovered.through_event_version.get(), 1);
    assert_eq!(recovered.panes[0].pane.incarnation.agent_pid, 142);
}

#[tokio::test]
async fn watch_reconnect_observes_a_non_agent_checkpoint_revision() {
    let directory = TestDirectory::new();
    let store = Arc::new(store::HaroldStore::open(&directory.0).await.unwrap());
    let (service, _shutdown, task) = test_service(Arc::clone(&store), empty_snapshot());
    store::append_inbound_message(
        &store,
        &store::InboundMessage {
            text: "continue".into(),
        },
    )
    .await
    .unwrap();
    super::projector::project_and_publish_agent_snapshot(&store, &service.snapshots, 500)
        .await
        .unwrap();

    let mut reconnected = service
        .watch_agent_states(Request::new(WatchAgentStatesRequest {}))
        .await
        .unwrap()
        .into_inner();
    let current = reconnected.next().await.expect("current snapshot").unwrap();
    assert_eq!(current.through_event_version, 1);
    assert!(current.panes.is_empty());
    task.abort();
}
