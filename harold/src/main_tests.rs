use std::sync::Arc;

use crate::store::EventVersion;
use tokio::sync::watch;
use tokio_stream::StreamExt;

use super::agent::domain::{
    AgentEvent, AgentIncarnation, AgentLifecycleObserved, AgentPaneObservation, AgentPaneObserved,
    AgentPaneProjection, AgentSnapshot, CompletionSummaryUpdate, EffectiveAgentState,
    MonitorHealthProjection, ObservedAgentState, WorkSummaryUpdate,
};
use super::agent::focus::FocusPort;
use super::agent::inventory::{AgentInventoryPort, InventoryError, WindowNames};
use super::agent::recap::Recaps;
use super::agent::rex::CurrentSteps;
use super::agent::rex_host_runner::{HostAgentRow, HostAgents, HostReport};
use super::agent::rex_hosts::{HostAgentState, RexHostAgent};
use super::agent::runtime::spawn_agent_monitor_for_test;
use super::agent::screen::TmuxVisibleScreen;
use super::agent::snapshot::AgentSnapshotHub;
use super::agent::waiting::{TrackedRow, WaitingView};
use super::harold::harold_server::Harold;
use super::harold::{
    AgentSeenRequest, AgentSeenResponse, AgentState, GetHaroldStateRequest, GetRemoteOrderRequest,
    MonitorHealthState, ReportAgentStateRequest, SendMessageToAwayDeviceRequest,
    SetMessagingPausedRequest, SetRemoteOrderRequest, SetTtsMutedRequest, SpeakLineRequest,
    WatchAgentStatesRequest,
};
use super::{
    HAROLD_VERSION, HaroldService, HaroldStateView, Request, RoleSources, TurnCompleteRequest,
    apply_setting, first_local_pane_in_rex, load_startup_agent_snapshot, map_agent_snapshot,
    pane_id_for_log, restore_startup_state, spawn_waiting_task, store, version_or_source,
    waiting_counts, with_waiting,
};

struct EmptyInventory;

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
    let service_store = Arc::clone(&store);
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
            store: service_store,
            snapshots: AgentSnapshotHub::new(snapshot),
            messaging_switch: tokio::sync::Mutex::new(None),
            tts_switch: tokio::sync::Mutex::new(None),
            shutdown: shutdown_rx,
            restart_reminder: None,
            current_steps: crate::agent::rex::CurrentSteps::default(),
            recaps: crate::agent::recap::Recaps::default(),
            host_agents: HostAgents::default(),
            window_names: WindowNames::default(),
            local_icon: String::new(),
            state_view: HaroldStateView::default(),
            watch_counts: watch::channel(crate::tools::WatchCounts::default()).1,
            waiting: watch::channel(WaitingView::default()).1,
            seen_calls: tokio::sync::mpsc::channel(1).0,
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
        tts_muted: false,
        messaging_paused: false,
        through_event_version: EventVersion::start(),
        server_time_ms: 1_000,
        monitor_health: Vec::new(),
        panes: Vec::new(),
    }
}

fn populated_snapshot(revision: i64, summary: Option<&str>) -> AgentSnapshot {
    AgentSnapshot {
        tts_muted: false,
        messaging_paused: false,
        through_event_version: EventVersion::from_column(revision).expect("valid revision"),
        server_time_ms: 10_000 + revision,
        monitor_health: vec![MonitorHealthProjection {
            component: "screen".into(),
            healthy: false,
            reason_code: "capture_failed".into(),
            observed_at_ms: 9_800,
            last_event_version: EventVersion::from_column(revision).expect("valid revision"),
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
            lifecycle_adapter_id: None,
            screen_state: None,
            screen_classifier_id: None,
            screen_observed_at_ms: None,
            effective_state: EffectiveAgentState::Working,
            explicit_work_summary: summary.map(str::to_string),
            explicit_work_summary_updated_at_ms: summary.map(|_| 8_600),
            screen_work_summary: None,
            screen_work_summary_updated_at_ms: None,
            summary_basis_version: EventVersion::start(),
            generated_work_summary: None,
            generated_summary_basis_version: None,
            work_summary: summary.map(str::to_string),
            last_transition_at_ms: 8_700,
            last_event_version: EventVersion::from_column(revision).expect("valid revision"),
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

#[derive(Default)]
struct RecordedTurnEnds(std::sync::Mutex<Vec<(String, String, String)>>);

impl crate::restart_reminder::TurnEnds for RecordedTurnEnds {
    fn turn_ended(&self, pane_id: &str, session_id: &str, transcript_path: &str) {
        self.0
            .lock()
            .unwrap()
            .push((pane_id.into(), session_id.into(), transcript_path.into()));
    }
}

#[tokio::test]
async fn turn_complete_tells_the_restart_reminder_the_session_and_transcript_after_storing() {
    let directory = TestDirectory::new();
    let store = Arc::new(store::HaroldStore::open(&directory.0).await.unwrap());
    let (mut service, _shutdown, task) = test_service(Arc::clone(&store), empty_snapshot());
    let recorded = Arc::new(RecordedTurnEnds::default());
    service.restart_reminder =
        Some(Arc::clone(&recorded) as Arc<dyn crate::restart_reminder::TurnEnds>);

    service
        .turn_complete(Request::new(TurnCompleteRequest {
            pane_id: "%8".into(),
            session_id: "s-1".into(),
            transcript_path: "/t/s-1.jsonl".into(),
            ..Default::default()
        }))
        .await
        .unwrap();

    assert_eq!(
        *recorded.0.lock().unwrap(),
        [(
            "%8".to_string(),
            "s-1".to_string(),
            "/t/s-1.jsonl".to_string()
        )]
    );
    task.abort();
}

#[derive(Clone, Default)]
struct WarnLogs(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for WarnLogs {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl WarnLogs {
    fn count(&self, message: &str) -> usize {
        String::from_utf8(self.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .filter(|line| line.contains(message))
            .count()
    }
}

/// A snapshot with Claude panes `ids[0]` and `ids[1]` and a Codex pane `ids[2]`.
fn claude_and_codex_snapshot(ids: &[String; 3]) -> AgentSnapshot {
    let mut snapshot = populated_snapshot(1, None);
    let template = snapshot.panes[0].clone();
    snapshot.panes = [(&ids[0], "claude"), (&ids[1], "claude"), (&ids[2], "codex")]
        .into_iter()
        .map(|(pane_id, provider_id)| {
            let mut pane = template.clone();
            pane.pane.incarnation.pane_id = pane_id.clone();
            pane.pane.incarnation.provider_id = provider_id.into();
            pane
        })
        .collect();
    snapshot
}

#[tokio::test]
async fn a_claude_turn_end_without_a_session_id_warns_once_for_its_pane() {
    crate::test_logs::keep_second_dispatcher();
    let logs = WarnLogs::default();
    let writer = logs.clone();
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .without_time()
            .with_max_level(tracing::Level::INFO)
            .finish(),
    );
    // The warned panes are kept for the whole process, so no other test may share these.
    let ids: [String; 3] =
        std::array::from_fn(|_| format!("%{}", uuid::Uuid::new_v4().as_u128() % 10u128.pow(18)));
    let directory = TestDirectory::new();
    let store = Arc::new(store::HaroldStore::open(&directory.0).await.unwrap());
    let (service, _shutdown, task) =
        test_service(Arc::clone(&store), claude_and_codex_snapshot(&ids));
    let warning = "turn hook sends no session id";
    let end = |pane_id: &str, session_id: &str| {
        let request = TurnCompleteRequest {
            pane_id: pane_id.into(),
            session_id: session_id.into(),
            ..Default::default()
        };
        service.turn_complete(Request::new(request))
    };

    end(&ids[2], "").await.unwrap();
    end(&ids[0], "s-1").await.unwrap();
    assert_eq!(logs.count(warning), 0, "a Codex pane, or a session id");

    end(&ids[0], "").await.unwrap();
    end(&ids[0], "").await.unwrap();
    assert_eq!(logs.count(warning), 1, "once for the pane");

    end(&ids[1], "").await.unwrap();
    assert_eq!(logs.count(warning), 2, "again for another pane");
    task.abort();
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
        ..Default::default()
    };

    let response = service
        .turn_complete(Request::new(request))
        .await
        .unwrap()
        .into_inner();
    assert!(response.accepted);

    let events = store
        .stream()
        .load_after_version(EventVersion::start(), 10)
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
        (AgentState::Working, None, WorkSummaryUpdate::Unchanged),
        (
            AgentState::Idle,
            Some(" \u{1b}[31m \t"),
            WorkSummaryUpdate::Clear,
        ),
        (
            AgentState::Working,
            Some("  refresh\n\u{2003}events  "),
            WorkSummaryUpdate::Set("refresh events".into()),
        ),
        (
            AgentState::Idle,
            Some(oversized.as_str()),
            WorkSummaryUpdate::Set("🦀".repeat(160)),
        ),
        (AgentState::Blocked, None, WorkSummaryUpdate::Unchanged),
        (AgentState::Done, None, WorkSummaryUpdate::Unchanged),
        (AgentState::Error, None, WorkSummaryUpdate::Unchanged),
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
        .load_after_version(EventVersion::start(), 20)
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
                AgentState::Working => Some(ObservedAgentState::Working),
                AgentState::Idle => Some(ObservedAgentState::Idle),
                AgentState::Blocked => Some(ObservedAgentState::Blocked),
                AgentState::Done => Some(ObservedAgentState::Done),
                AgentState::Error => Some(ObservedAgentState::Error),
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
        ("8", i32::from(AgentState::Working), "codex-hook"),
        ("%8", i32::from(AgentState::Working), "Codex hook"),
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
        .load_after_version(EventVersion::start(), 20)
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
        state: AgentState::Working.into(),
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
        .load_after_version(EventVersion::start(), 20)
        .await
        .unwrap();
    assert!(events.is_empty());
}

#[tokio::test]
async fn report_agent_state_keeps_normalized_empty_completion_summary_unchanged() {
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
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(response.accepted);

    let events = store
        .stream()
        .load_after_version(EventVersion::start(), 10)
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
            state: AgentState::Working.into(),
            adapter_id: "codex-hook".into(),
            work_summary: Some("Implement projector".into()),
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(response.accepted);

    let events = store
        .stream()
        .load_after_version(EventVersion::start(), 10)
        .await
        .unwrap();
    assert_eq!(events.len(), 2);
    let observed: AgentPaneObserved = serde_json::from_value(events[0].payload.clone()).unwrap();
    let lifecycle: AgentLifecycleObserved =
        serde_json::from_value(events[1].payload.clone()).unwrap();
    assert_eq!(observed.pane.incarnation, expected_pane.incarnation);
    assert_eq!(lifecycle.incarnation, expected_pane.incarnation);
    assert_eq!(lifecycle.state, Some(ObservedAgentState::Working));
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
    assert_watched_working_incarnation(&projected, &expected_pane, "Implement projector");

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
    assert_watched_working_incarnation(&restarted, &expected_pane, "Implement projector");
    restarted_task.abort();
}

fn assert_watched_working_incarnation(
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
    assert_eq!(pane.state, i32::from(AgentState::Working));
    assert_eq!(pane.work_summary.as_deref(), Some(summary));
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
    assert_eq!(pane.state, i32::from(AgentState::Working));
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
#[cfg_attr(
    not(feature = "timing-tests"),
    ignore = "timing: run with --features timing-tests"
)]
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
async fn startup_projects_every_page_before_the_first_watch_snapshot_without_appending_events() {
    let directory = TestDirectory::new();
    let store = Arc::new(store::HaroldStore::open(&directory.0).await.unwrap());
    let pane = resolved_pane();
    let mut events = vec![AgentEvent::PaneObserved(AgentPaneObserved {
        pane: pane.clone(),
    })];
    events.extend((1..=501).map(|index| {
        AgentEvent::LifecycleObserved(AgentLifecycleObserved {
            incarnation: pane.incarnation.clone(),
            state: Some(ObservedAgentState::Working),
            adapter_id: "codex-hook".into(),
            work_summary: WorkSummaryUpdate::Set(format!("Task {index}")),
            observed_at_ms: pane.observed_at_ms + index,
        })
    }));
    let appended = store::append_agent_events(&store, events).await.unwrap();
    assert_eq!(appended.events.len(), 502);
    assert_eq!(store.last_processed_version().await.unwrap().get(), 0);

    let initial = load_startup_agent_snapshot(&store).await.unwrap();
    assert_eq!(initial.through_event_version.get(), 502);
    assert_eq!(initial.panes[0].work_summary.as_deref(), Some("Task 501"));
    let persisted = store
        .stream()
        .load_after_version(EventVersion::start(), 1_000)
        .await
        .unwrap();
    assert_eq!(persisted.len(), 502);
    assert_eq!(persisted.last().unwrap().r#type, "AgentLifecycleObserved");

    let (service, shutdown, task) = test_service(Arc::clone(&store), initial);
    let mut stream = service
        .watch_agent_states(Request::new(WatchAgentStatesRequest {}))
        .await
        .unwrap()
        .into_inner();
    let first = stream.next().await.expect("first snapshot").unwrap();
    assert_eq!(first.panes.len(), 1);
    assert_eq!(first.panes[0].pane_id, pane.incarnation.pane_id);
    assert_eq!(first.panes[0].work_summary.as_deref(), Some("Task 501"));

    drop(shutdown);
    task.await.unwrap();
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

#[tokio::test]
async fn set_tts_muted_appends_once_and_is_idempotent() {
    let directory = std::env::temp_dir().join(format!("harold-mute-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let store = Arc::new(store::HaroldStore::open(&directory).await.unwrap());
    let (service, shutdown, task) = test_service(Arc::clone(&store), empty_snapshot());

    // The switch is flipped by the request itself, before anything is projected.
    assert!(!super::outbound::is_tts_muted());
    let response = service
        .set_tts_muted(Request::new(SetTtsMutedRequest { muted: true }))
        .await
        .unwrap()
        .into_inner();
    assert!(response.muted);
    assert!(super::outbound::is_tts_muted());
    let batch = store.project_unhandled_events(10).await.unwrap();
    assert_eq!(batch.applied, 1);
    assert!(store.load_agent_snapshot().await.unwrap().tts_muted);

    // Already-applied value (as seen by the hub) must not append again.
    service
        .snapshots
        .publish_committed(store.load_agent_snapshot().await.unwrap());
    service
        .set_tts_muted(Request::new(SetTtsMutedRequest { muted: true }))
        .await
        .unwrap();
    assert_eq!(store.project_unhandled_events(10).await.unwrap().applied, 0);

    drop(shutdown);
    let _ = task.await;
    let _ = std::fs::remove_dir_all(&directory);
}

#[tokio::test]
async fn mute_sent_while_the_snapshot_lags_an_unmute_is_stored() {
    let directory = std::env::temp_dir().join(format!("harold-mute-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let store = Arc::new(store::HaroldStore::open(&directory).await.unwrap());
    let (service, shutdown, task) = test_service(Arc::clone(&store), empty_snapshot());

    let mut switched = Vec::new();
    service
        .apply_tts_muted(true, || false, |muted| switched.push(muted))
        .await
        .unwrap();
    assert_eq!(switched, [true]);
    assert_eq!(store.project_unhandled_events(10).await.unwrap().applied, 1);
    service
        .snapshots
        .publish_committed(store.load_agent_snapshot().await.unwrap());

    // A true repeat: the switch and the hub both say "muted" already.
    service
        .apply_tts_muted(true, || true, |muted| switched.push(muted))
        .await
        .unwrap();
    assert_eq!(switched, [true, true]);
    assert_eq!(store.project_unhandled_events(10).await.unwrap().applied, 0);

    // An unmute that is not projected yet leaves the hub saying "muted". A mute sent
    // in that window must still be stored and applied, not taken for a repeat.
    service
        .apply_tts_muted(false, || true, |muted| switched.push(muted))
        .await
        .unwrap();
    service
        .apply_tts_muted(true, || false, |muted| switched.push(muted))
        .await
        .unwrap();
    assert_eq!(switched, [true, true, false, true]);
    assert_eq!(store.project_unhandled_events(10).await.unwrap().applied, 2);
    assert!(store.load_agent_snapshot().await.unwrap().tts_muted);

    drop(shutdown);
    let _ = task.await;
    let _ = std::fs::remove_dir_all(&directory);
}

#[tokio::test]
async fn overlapping_mute_requests_wait_for_the_one_in_progress() {
    let directory = std::env::temp_dir().join(format!("harold-mute-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let store = Arc::new(store::HaroldStore::open(&directory).await.unwrap());
    let (service, shutdown, task) = test_service(Arc::clone(&store), empty_snapshot());

    // Stand in for a request that is between its append and its switch.
    let in_progress = service.tts_switch.lock().await;
    let mut switched = Vec::new();
    {
        let mut request =
            Box::pin(service.apply_tts_muted(true, || false, |muted| switched.push(muted)));
        let waited =
            tokio::time::timeout(std::time::Duration::from_millis(200), &mut request).await;
        assert!(waited.is_err(), "second request ran inside the first");
        assert_eq!(store.project_unhandled_events(10).await.unwrap().applied, 0);

        drop(in_progress);
        request.await.unwrap();
    }
    assert_eq!(switched, [true]);
    assert_eq!(store.project_unhandled_events(10).await.unwrap().applied, 1);

    drop(shutdown);
    let _ = task.await;
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn snapshot_mapping_carries_the_harold_version() {
    assert_eq!(
        map_agent_snapshot(
            empty_snapshot(),
            &CurrentSteps::default(),
            &Recaps::default(),
            &HostAgents::default(),
            &WindowNames::default(),
            "",
            &RoleSources::default(),
        )
        .harold_version,
        HAROLD_VERSION
    );
    // Tests build without HAROLD_RELEASE_VERSION.
    assert_eq!(HAROLD_VERSION, "source");
}

#[test]
fn snapshot_mapping_gives_each_effective_state_its_own_number() {
    for (effective, expected) in [
        (EffectiveAgentState::Unknown, 3),
        (EffectiveAgentState::Working, 1),
        (EffectiveAgentState::Blocked, 4),
        (EffectiveAgentState::Done, 5),
        (EffectiveAgentState::Idle, 2),
        (EffectiveAgentState::Error, 6),
    ] {
        let mut snapshot = populated_snapshot(3, None);
        snapshot.panes[0].effective_state = effective;
        let mapped = map_agent_snapshot(
            snapshot,
            &CurrentSteps::default(),
            &Recaps::default(),
            &HostAgents::default(),
            &WindowNames::default(),
            "",
            &RoleSources::default(),
        );
        assert_eq!(mapped.panes[0].state, expected, "{effective:?}");
    }
}

#[test]
fn an_unset_or_empty_release_version_is_source() {
    assert_eq!(version_or_source(None), "source");
    assert_eq!(version_or_source(Some("")), "source");
    assert_eq!(version_or_source(Some("v0.9.0")), "v0.9.0");
}

#[test]
fn snapshot_mapping_carries_tts_muted() {
    let mut snapshot = empty_snapshot();
    snapshot.tts_muted = true;
    assert!(
        map_agent_snapshot(
            snapshot,
            &CurrentSteps::default(),
            &Recaps::default(),
            &HostAgents::default(),
            &WindowNames::default(),
            "",
            &RoleSources::default(),
        )
        .tts_muted
    );
}

#[tokio::test]
async fn set_messaging_paused_appends_once_and_switches_immediately() {
    let directory = std::env::temp_dir().join(format!("harold-pause-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let store = Arc::new(store::HaroldStore::open(&directory).await.unwrap());
    let (service, shutdown, task) = test_service(Arc::clone(&store), empty_snapshot());

    // The switch is flipped by the request itself, before anything is projected.
    let mut switched = Vec::new();
    service
        .apply_messaging_paused(true, || false, |paused| switched.push(paused))
        .await
        .unwrap();
    assert_eq!(switched, [true]);
    let batch = store.project_unhandled_events(10).await.unwrap();
    assert_eq!(batch.applied, 1);
    assert!(store.load_agent_snapshot().await.unwrap().messaging_paused);

    // Already-applied value (as seen by the hub) must not append again.
    service
        .snapshots
        .publish_committed(store.load_agent_snapshot().await.unwrap());
    service
        .apply_messaging_paused(true, || true, |paused| switched.push(paused))
        .await
        .unwrap();
    assert_eq!(store.project_unhandled_events(10).await.unwrap().applied, 0);
    assert_eq!(switched, [true, true]);

    // A resume that is not projected yet leaves the hub saying "paused". A pause sent
    // in that window must still be stored and applied, not taken for a repeat.
    service
        .apply_messaging_paused(false, || true, |paused| switched.push(paused))
        .await
        .unwrap();
    service
        .apply_messaging_paused(true, || false, |paused| switched.push(paused))
        .await
        .unwrap();
    assert_eq!(switched, [true, true, false, true]);
    assert_eq!(store.project_unhandled_events(10).await.unwrap().applied, 2);
    assert!(store.load_agent_snapshot().await.unwrap().messaging_paused);

    drop(shutdown);
    let _ = task.await;
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn snapshot_mapping_carries_messaging_paused() {
    let mut snapshot = empty_snapshot();
    snapshot.messaging_paused = true;
    assert!(
        map_agent_snapshot(
            snapshot,
            &CurrentSteps::default(),
            &Recaps::default(),
            &HostAgents::default(),
            &WindowNames::default(),
            "",
            &RoleSources::default(),
        )
        .messaging_paused
    );
}

#[tokio::test]
async fn startup_restores_the_messaging_switch_from_a_reopened_store() {
    let directory = std::env::temp_dir().join(format!("harold-startup-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    {
        let store = store::HaroldStore::open(&directory).await.unwrap();
        store::append_messaging_paused_changed(&store, true)
            .await
            .unwrap();
    }
    assert!(!super::outbound::is_messaging_paused());

    let reopened = store::HaroldStore::open(&directory).await.unwrap();
    let snapshot = restore_startup_state(&reopened).await.unwrap();

    assert!(snapshot.messaging_paused);
    assert!(super::outbound::is_messaging_paused());
    let _ = std::fs::remove_dir_all(&directory);
}

#[tokio::test]
async fn overlapping_pause_requests_wait_for_the_one_in_progress() {
    let directory = std::env::temp_dir().join(format!("harold-serial-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let store = Arc::new(store::HaroldStore::open(&directory).await.unwrap());
    let (service, shutdown, task) = test_service(Arc::clone(&store), empty_snapshot());

    // Stand in for a request that is between its append and its switch.
    let in_progress = service.messaging_switch.lock().await;
    let mut switched = Vec::new();
    {
        let mut request = Box::pin(service.apply_messaging_paused(
            true,
            || false,
            |paused| switched.push(paused),
        ));
        let waited =
            tokio::time::timeout(std::time::Duration::from_millis(200), &mut request).await;
        assert!(waited.is_err(), "second request ran inside the first");
        assert_eq!(store.project_unhandled_events(10).await.unwrap().applied, 0);

        drop(in_progress);
        request.await.unwrap();
    }
    assert_eq!(switched, [true]);
    assert_eq!(store.project_unhandled_events(10).await.unwrap().applied, 1);

    drop(shutdown);
    let _ = task.await;
    let _ = std::fs::remove_dir_all(&directory);
}

/// Opens a store whose snapshot hub has caught up with a stored `on` value for both
/// settings, as after startup.
async fn service_started_with_both_settings(
    directory: &std::path::Path,
    on: bool,
) -> (
    Arc<store::HaroldStore>,
    HaroldService,
    watch::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    std::fs::create_dir_all(directory).unwrap();
    let store = Arc::new(store::HaroldStore::open(directory).await.unwrap());
    store::append_tts_mute_changed(&store, on).await.unwrap();
    store::append_messaging_paused_changed(&store, on)
        .await
        .unwrap();
    store.project_unhandled_events(10).await.unwrap();
    let snapshot = store.load_agent_snapshot().await.unwrap();
    super::outbound::set_tts_muted(snapshot.tts_muted);
    super::outbound::set_messaging_paused(snapshot.messaging_paused);
    let (service, shutdown, task) = test_service(Arc::clone(&store), snapshot);
    (store, service, shutdown, task)
}

#[tokio::test]
async fn mute_after_the_projector_rewrites_the_switch_from_an_older_snapshot_is_stored() {
    let directory = std::env::temp_dir().join(format!("harold-stale-{}", uuid::Uuid::new_v4()));
    let (store, service, shutdown, task) =
        service_started_with_both_settings(&directory, true).await;

    service
        .set_tts_muted(Request::new(SetTtsMutedRequest { muted: false }))
        .await
        .unwrap();
    assert!(!super::outbound::is_tts_muted());
    // The projector finishes a pass it began before the unmute was stored: it writes the
    // switch from its older snapshot, and the hub still says "muted" too.
    super::outbound::set_tts_muted(true);
    assert!(service.snapshots.tts_muted());

    service
        .set_tts_muted(Request::new(SetTtsMutedRequest { muted: true }))
        .await
        .unwrap();

    assert_eq!(store.project_unhandled_events(10).await.unwrap().applied, 2);
    assert!(store.load_agent_snapshot().await.unwrap().tts_muted);
    assert!(super::outbound::is_tts_muted());

    drop(shutdown);
    let _ = task.await;
    let _ = std::fs::remove_dir_all(&directory);
}

#[tokio::test]
async fn pause_after_the_projector_rewrites_the_switch_from_an_older_snapshot_is_stored() {
    let directory = std::env::temp_dir().join(format!("harold-stale-{}", uuid::Uuid::new_v4()));
    let (store, service, shutdown, task) =
        service_started_with_both_settings(&directory, true).await;

    service
        .set_messaging_paused(Request::new(SetMessagingPausedRequest { paused: false }))
        .await
        .unwrap();
    assert!(!super::outbound::is_messaging_paused());
    // As above: the switch is rewritten from a snapshot older than the resume.
    super::outbound::set_messaging_paused(true);
    assert!(service.snapshots.messaging_paused());

    service
        .set_messaging_paused(Request::new(SetMessagingPausedRequest { paused: true }))
        .await
        .unwrap();

    assert_eq!(store.project_unhandled_events(10).await.unwrap().applied, 2);
    assert!(store.load_agent_snapshot().await.unwrap().messaging_paused);
    assert!(super::outbound::is_messaging_paused());

    drop(shutdown);
    let _ = task.await;
    let _ = std::fs::remove_dir_all(&directory);
}

#[tokio::test]
async fn first_request_after_startup_is_judged_against_the_loaded_state() {
    let directory = std::env::temp_dir().join(format!("harold-first-{}", uuid::Uuid::new_v4()));
    let (store, service, shutdown, task) =
        service_started_with_both_settings(&directory, true).await;

    // Nothing has been requested yet, and both requests match what startup loaded.
    service
        .set_tts_muted(Request::new(SetTtsMutedRequest { muted: true }))
        .await
        .unwrap();
    service
        .set_messaging_paused(Request::new(SetMessagingPausedRequest { paused: true }))
        .await
        .unwrap();
    assert_eq!(store.project_unhandled_events(10).await.unwrap().applied, 0);
    assert_eq!(*service.tts_switch.lock().await, None);
    assert_eq!(*service.messaging_switch.lock().await, None);

    service
        .set_tts_muted(Request::new(SetTtsMutedRequest { muted: false }))
        .await
        .unwrap();
    service
        .set_messaging_paused(Request::new(SetMessagingPausedRequest { paused: false }))
        .await
        .unwrap();
    assert_eq!(store.project_unhandled_events(10).await.unwrap().applied, 2);
    let snapshot = store.load_agent_snapshot().await.unwrap();
    assert!(!snapshot.tts_muted);
    assert!(!snapshot.messaging_paused);
    assert_eq!(*service.tts_switch.lock().await, Some(false));
    assert_eq!(*service.messaging_switch.lock().await, Some(false));

    drop(shutdown);
    let _ = task.await;
    let _ = std::fs::remove_dir_all(&directory);
}

#[tokio::test]
async fn failed_append_forgets_the_last_requested_value_and_leaves_the_switch_alone() {
    for remembered in [None, Some(false)] {
        let last_requested = tokio::sync::Mutex::new(remembered);
        let mut switched = Vec::new();

        let failed = apply_setting(
            &last_requested,
            true,
            || false,
            || async { Err("store unavailable") },
            |on| switched.push(on),
        )
        .await;
        assert_eq!(failed, Err("store unavailable"));
        assert_eq!(*last_requested.lock().await, None);
        assert!(switched.is_empty());

        // The request is not taken for a repeat when it is sent again.
        let stored = apply_setting(
            &last_requested,
            true,
            || false,
            || async { Ok::<(), &str>(()) },
            |on| switched.push(on),
        )
        .await;
        assert_eq!(stored, Ok(true));
        assert_eq!(*last_requested.lock().await, Some(true));
        assert_eq!(switched, [true]);
    }
}

#[tokio::test]
async fn request_after_a_failed_append_is_judged_against_the_loaded_state() {
    let last_requested = tokio::sync::Mutex::new(Some(false));
    let mut switched = Vec::new();
    let failed = apply_setting(
        &last_requested,
        true,
        || false,
        || async { Err("store unavailable") },
        |on| switched.push(on),
    )
    .await;
    assert_eq!(failed, Err("store unavailable"));

    // The event may have been written all the same. If the loaded state says so, the
    // remembered "false" must not make this a change to store again.
    let stored = apply_setting(
        &last_requested,
        true,
        || true,
        || async { Err("a repeat appends nothing") },
        |on| switched.push(on),
    )
    .await;
    assert_eq!(stored, Ok(false));
    assert_eq!(switched, [true]);
}

#[tokio::test]
async fn request_dropped_during_its_append_forgets_the_last_requested_value() {
    let last_requested = tokio::sync::Mutex::new(Some(false));

    let dropped = tokio::time::timeout(
        std::time::Duration::from_millis(50),
        apply_setting(
            &last_requested,
            true,
            || false,
            std::future::pending::<Result<(), &str>>,
            |_| panic!("switched without a stored value"),
        ),
    )
    .await;

    assert!(dropped.is_err(), "append finished");
    assert_eq!(*last_requested.lock().await, None);
}

#[tokio::test]
async fn repeated_request_sets_the_switch_again_and_appends_nothing() {
    let directory = std::env::temp_dir().join(format!("harold-repeat-{}", uuid::Uuid::new_v4()));
    let (store, service, shutdown, task) =
        service_started_with_both_settings(&directory, false).await;

    service
        .set_tts_muted(Request::new(SetTtsMutedRequest { muted: true }))
        .await
        .unwrap();
    service
        .set_messaging_paused(Request::new(SetMessagingPausedRequest { paused: true }))
        .await
        .unwrap();
    assert_eq!(store.project_unhandled_events(10).await.unwrap().applied, 2);
    // The projector finishes a pass it began before the requests were stored and writes
    // both switches from its older snapshot.
    super::outbound::set_tts_muted(false);
    super::outbound::set_messaging_paused(false);

    service
        .set_tts_muted(Request::new(SetTtsMutedRequest { muted: true }))
        .await
        .unwrap();
    service
        .set_messaging_paused(Request::new(SetMessagingPausedRequest { paused: true }))
        .await
        .unwrap();

    assert_eq!(store.project_unhandled_events(10).await.unwrap().applied, 0);
    assert!(super::outbound::is_tts_muted());
    assert!(super::outbound::is_messaging_paused());

    drop(shutdown);
    let _ = task.await;
    let _ = std::fs::remove_dir_all(&directory);
}

/// Every agent message stored in `store`, in order, and the count of all stored events.
async fn away_messages(store: &store::HaroldStore) -> (Vec<store::AwayMessageRequested>, usize) {
    let events = store
        .stream()
        .load_after_version(EventVersion::start(), 100)
        .await
        .unwrap();
    let total = events.len();
    let messages = events
        .into_iter()
        .filter(|event| event.r#type == "AwayMessageRequested")
        .map(|event| serde_json::from_value(event.payload).unwrap())
        .collect();
    (messages, total)
}

fn away_request(src_id: &str, text: &str) -> Request<SendMessageToAwayDeviceRequest> {
    Request::new(SendMessageToAwayDeviceRequest {
        src_id: src_id.into(),
        text: text.into(),
    })
}

/// A service whose monitor tracks the one agent pane `%8`, labelled `harold:2.3`.
fn service_tracking_a_pane(
    store: &Arc<store::HaroldStore>,
) -> (
    HaroldService,
    watch::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let inventory = Arc::new(ResolvingInventory {
        pane: Some(resolved_pane()),
    });
    test_service_with_inventory(Arc::clone(store), empty_snapshot(), inventory)
}

#[tokio::test]
async fn accepted_away_message_stores_one_event_with_the_monitors_label_and_one_line_text() {
    let directory = TestDirectory::new();
    let store = Arc::new(store::HaroldStore::open(&directory.0).await.unwrap());
    let (service, _shutdown, task) = service_tracking_a_pane(&store);

    let response = service
        .send_message_to_away_device(away_request(
            "%8",
            "  approval:\n\thttps://example.com\u{7}  ",
        ))
        .await
        .unwrap();

    assert!(response.into_inner().accepted);
    let (messages, total) = away_messages(&store).await;
    assert_eq!(total, 1, "exactly one event is stored");
    assert_eq!(
        messages,
        [store::AwayMessageRequested {
            pane_id: "%8".into(),
            pane_label: "harold:2.3".into(),
            text: "approval: https://example.com".into(),
        }]
    );
    task.abort();
}

#[tokio::test]
async fn away_message_of_one_and_of_a_thousand_characters_is_accepted_and_longer_is_refused() {
    let directory = TestDirectory::new();
    let store = Arc::new(store::HaroldStore::open(&directory.0).await.unwrap());
    let (service, _shutdown, task) = service_tracking_a_pane(&store);

    for text in ["x".to_string(), "x".repeat(1_000), "🤖".repeat(1_000)] {
        let response = service
            .send_message_to_away_device(away_request("%8", &text))
            .await
            .unwrap();
        assert!(response.into_inner().accepted);
    }
    for text in ["x".repeat(1_001), String::new(), " \n\t ".to_string()] {
        let error = service
            .send_message_to_away_device(away_request("%8", &text))
            .await
            .unwrap_err();
        assert_eq!(error.code(), tonic::Code::InvalidArgument);
        assert_eq!(error.message(), "invalid message length");
    }

    let (messages, total) = away_messages(&store).await;
    assert_eq!(total, 3);
    assert_eq!(messages[1].text.chars().count(), 1_000);
    task.abort();
}

#[tokio::test]
async fn each_away_message_refusal_returns_its_status_and_stores_nothing() {
    let directory = TestDirectory::new();
    let store = Arc::new(store::HaroldStore::open(&directory.0).await.unwrap());
    let (service, _shutdown, task) = service_tracking_a_pane(&store);

    for src_id in ["8", "%", "%8a", "", "pane"] {
        let error = service
            .send_message_to_away_device(away_request(src_id, "sensitive text"))
            .await
            .unwrap_err();
        assert_eq!(error.code(), tonic::Code::InvalidArgument, "{src_id:?}");
        assert_eq!(error.message(), "invalid source id");
    }

    let too_long = service
        .send_message_to_away_device(away_request("%8", &"x".repeat(1_001)))
        .await
        .unwrap_err();
    assert_eq!(too_long.code(), tonic::Code::InvalidArgument);
    assert_eq!(too_long.message(), "invalid message length");

    // The switch is per thread under test, and this test runs on one thread.
    crate::outbound::set_messaging_paused(true);
    let paused = service
        .send_message_to_away_device(away_request("%8", "sensitive text"))
        .await
        .unwrap_err();
    crate::outbound::set_messaging_paused(false);
    assert_eq!(paused.code(), tonic::Code::FailedPrecondition);
    assert_eq!(paused.message(), "messaging paused");

    let unknown = service
        .send_message_to_away_device(away_request("%9", "sensitive text"))
        .await
        .unwrap_err();
    assert_eq!(unknown.code(), tonic::Code::FailedPrecondition);
    assert_eq!(unknown.message(), "agent not found");

    let (messages, total) = away_messages(&store).await;
    assert!(messages.is_empty());
    assert_eq!(total, 0, "a refusal stores no event");
    task.abort();
}

#[tokio::test]
async fn away_message_checks_run_in_the_order_the_spec_gives() {
    let directory = TestDirectory::new();
    let store = Arc::new(store::HaroldStore::open(&directory.0).await.unwrap());
    let (service, _shutdown, task) = service_tracking_a_pane(&store);

    // A bad source id wins over an empty text, and an empty text over the pause.
    crate::outbound::set_messaging_paused(true);
    let bad_source = service
        .send_message_to_away_device(away_request("8", ""))
        .await
        .unwrap_err();
    let empty = service
        .send_message_to_away_device(away_request("%9", ""))
        .await
        .unwrap_err();
    // The pause wins over a pane that hosts no agent.
    let paused = service
        .send_message_to_away_device(away_request("%9", "text"))
        .await
        .unwrap_err();
    crate::outbound::set_messaging_paused(false);

    assert_eq!(bad_source.message(), "invalid source id");
    assert_eq!(empty.message(), "invalid message length");
    assert_eq!(paused.message(), "messaging paused");
    task.abort();
}

#[tokio::test]
async fn away_message_is_unavailable_when_the_monitor_has_stopped() {
    let directory = TestDirectory::new();
    let store = Arc::new(store::HaroldStore::open(&directory.0).await.unwrap());
    let (service, shutdown, task) = service_tracking_a_pane(&store);
    drop(shutdown);
    task.await.unwrap();

    let error = service
        .send_message_to_away_device(away_request("%8", "sensitive text"))
        .await
        .unwrap_err();

    assert_eq!(error.code(), tonic::Code::Unavailable);
    assert!(!error.message().contains("sensitive text"));
    let (messages, _) = away_messages(&store).await;
    assert!(messages.is_empty());
}

/// Every spoken line stored in `store`, in order, and the count of all stored events.
async fn spoken_lines(store: &store::HaroldStore) -> (Vec<store::SpokenLineRequested>, usize) {
    let events = store
        .stream()
        .load_after_version(EventVersion::start(), 100)
        .await
        .unwrap();
    let total = events.len();
    let lines = events
        .into_iter()
        .filter(|event| event.r#type == "SpokenLineRequested")
        .map(|event| serde_json::from_value(event.payload).unwrap())
        .collect();
    (lines, total)
}

fn speak_request(src_id: &str, text: &str) -> Request<SpeakLineRequest> {
    Request::new(SpeakLineRequest {
        src_id: src_id.into(),
        text: text.into(),
    })
}

#[tokio::test]
async fn accepted_spoken_line_stores_one_event_with_one_line_text() {
    let directory = TestDirectory::new();
    let store = Arc::new(store::HaroldStore::open(&directory.0).await.unwrap());
    let (service, _shutdown, task) = service_tracking_a_pane(&store);

    service
        .speak_line(speak_request("%8", "  Release install\n\tis done.  "))
        .await
        .unwrap();

    let (lines, total) = spoken_lines(&store).await;
    assert_eq!(total, 1);
    assert_eq!(
        lines,
        [store::SpokenLineRequested {
            pane_id: "%8".into(),
            text: "Release install is done.".into(),
        }]
    );
    task.abort();
}

#[tokio::test]
async fn spoken_line_of_300_characters_is_accepted_and_0_or_301_is_refused() {
    let directory = TestDirectory::new();
    let store = Arc::new(store::HaroldStore::open(&directory.0).await.unwrap());
    let (service, _shutdown, task) = service_tracking_a_pane(&store);

    for text in ["", " \n\t", &"x".repeat(301)] {
        let status = service
            .speak_line(speak_request("%8", text))
            .await
            .unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert_eq!(
            status.message(),
            crate::away_message::INVALID_MESSAGE_LENGTH
        );
    }
    let (lines, total) = spoken_lines(&store).await;
    assert_eq!(
        (lines.len(), total),
        (0, 0),
        "a refused line stores nothing"
    );

    service
        .speak_line(speak_request("%8", &"x".repeat(300)))
        .await
        .unwrap();
    assert_eq!(spoken_lines(&store).await.0.len(), 1);
    task.abort();
}

#[tokio::test]
async fn spoken_line_from_a_bad_or_unknown_pane_is_refused_and_stores_nothing() {
    let directory = TestDirectory::new();
    let store = Arc::new(store::HaroldStore::open(&directory.0).await.unwrap());
    let (service, _shutdown, task) = service_tracking_a_pane(&store);

    let bad = service
        .speak_line(speak_request("8", "hi"))
        .await
        .unwrap_err();
    assert_eq!(bad.code(), tonic::Code::InvalidArgument);
    assert_eq!(bad.message(), crate::away_message::INVALID_SOURCE_ID);
    let unknown = service
        .speak_line(speak_request("%9", "hi"))
        .await
        .unwrap_err();
    assert_eq!(unknown.code(), tonic::Code::FailedPrecondition);
    assert_eq!(unknown.message(), crate::away_message::AGENT_NOT_FOUND);

    assert_eq!(spoken_lines(&store).await.1, 0);
    task.abort();
}

#[tokio::test]
async fn spoken_line_is_accepted_while_messaging_is_paused() {
    // The pause covers the phone only; at the desk the line is still said.
    let directory = TestDirectory::new();
    let store = Arc::new(store::HaroldStore::open(&directory.0).await.unwrap());
    let (service, _shutdown, task) = service_tracking_a_pane(&store);
    crate::outbound::set_messaging_paused(true);

    let result = service.speak_line(speak_request("%8", "hi")).await;

    crate::outbound::set_messaging_paused(false);
    assert!(result.is_ok());
    task.abort();
}

#[tokio::test]
async fn turn_complete_with_a_host_pane_id_stores_nothing() {
    crate::test_logs::keep_second_dispatcher();
    let logs = WarnLogs::default();
    let writer = logs.clone();
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .without_time()
            .with_max_level(tracing::Level::INFO)
            .finish(),
    );
    let directory = TestDirectory::new();
    let store = Arc::new(store::HaroldStore::open(&directory.0).await.unwrap());
    let (service, _shutdown, task) = test_service(Arc::clone(&store), empty_snapshot());
    // The refusal log is once for each pane id, so each id here is new to the process.
    let unique = uuid::Uuid::new_v4().simple().to_string();
    let host_id = format!("rex:omarchy/block:{unique}");
    let session_id = format!("main:0.{unique}");

    for pane_id in [host_id.as_str(), session_id.as_str()] {
        let response = service
            .turn_complete(Request::new(TurnCompleteRequest {
                pane_id: pane_id.into(),
                assistant_message: "report text".into(),
                ..Default::default()
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(!response.accepted, "{pane_id}");
    }

    let events = store
        .stream()
        .load_after_version(EventVersion::start(), 10)
        .await
        .unwrap();
    assert!(events.is_empty(), "a refused report stores nothing");
    // The host id logs once, by its text. The id with no known form logs the fixed text:
    // once for the process, so an earlier test may have used that line.
    assert_eq!(logs.count(&host_id), 1);
    assert_eq!(logs.count(&session_id), 0);
    assert!(logs.count("pane_id=<invalid-pane-id>") <= 1);
    assert!(logs.count("writer=\"turn_complete\"") >= 1);
    assert_eq!(logs.count("turn complete received"), 0);
    task.abort();
}

#[tokio::test]
async fn turn_complete_with_a_hostile_pane_id_logs_and_keeps_none_of_it() {
    crate::test_logs::keep_second_dispatcher();
    let logs = WarnLogs::default();
    let writer = logs.clone();
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .without_time()
            .with_max_level(tracing::Level::INFO)
            .finish(),
    );
    let directory = TestDirectory::new();
    let store = Arc::new(store::HaroldStore::open(&directory.0).await.unwrap());
    let (service, _shutdown, task) = test_service(Arc::clone(&store), empty_snapshot());
    let hostile = format!("rex:\nHOSTILE\u{1b}[31m{}", "x".repeat(10_000));
    let response = service
        .turn_complete(Request::new(TurnCompleteRequest {
            pane_id: hostile,
            assistant_message: "report text".into(),
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(!response.accepted);
    assert_eq!(logs.count("HOSTILE"), 0);
    assert_eq!(logs.count("xxxx"), 0);
    assert!(
        crate::agent::pane_route::kept_ids()
            .iter()
            .all(|id| id.len() <= 128 && !id.contains("HOSTILE"))
    );
    task.abort();
}

#[tokio::test]
async fn away_message_and_spoken_line_refuse_a_host_src_id_as_on_rex_host() {
    let directory = TestDirectory::new();
    let store = Arc::new(store::HaroldStore::open(&directory.0).await.unwrap());
    let (service, _shutdown, task) = service_tracking_a_pane(&store);
    let host_id = "rex:omarchy/block:abc";

    let away = service
        .send_message_to_away_device(away_request(host_id, "sensitive text"))
        .await
        .unwrap_err();
    let spoken = service
        .speak_line(speak_request(host_id, "hi"))
        .await
        .unwrap_err();

    for error in [away, spoken] {
        assert_ne!(error.message(), "invalid source id");
        assert!(
            error.message().contains("on_rex_host"),
            "{}",
            error.message()
        );
    }
    let (messages, total) = away_messages(&store).await;
    assert!(messages.is_empty());
    assert_eq!(total, 0, "a refusal stores no event");
    task.abort();
}

// ---------------------------------------------------------------------------
// One trace per turn
// ---------------------------------------------------------------------------

mod one_trace_per_turn {
    use std::sync::Arc;

    use opentelemetry::trace::TraceId;
    use opentelemetry_sdk::trace::SpanData;
    use time::OffsetDateTime;
    #[cfg_attr(not(feature = "imessage"), allow(unused_imports))]
    use time::format_description::well_known::Rfc3339;

    use super::{
        Harold, Request, TestDirectory, TurnCompleteRequest, empty_snapshot, store, test_service,
    };
    use crate::inbound::RouteReport;
    use crate::outbound::fake::FakeEnvironment;
    #[cfg_attr(not(feature = "imessage"), allow(unused_imports))]
    use crate::outbound::{DeliveryOutcome, OutboundChannel};
    use crate::projector::tests::fake_dispatcher;
    use crate::projector::{Router, handle_next_delivery};
    use crate::telemetry::test_support::{self, span_attr};
    use crate::telemetry::{ResolvedBy, Routing};

    /// A valid pane id (`%` and digits, so the log lines name it) no other test uses.
    fn unique_pane() -> String {
        format!("%{}", uuid::Uuid::new_v4().as_u128() % 10u128.pow(18))
    }

    /// A router that types every reply into `pane`.
    fn typing_router(pane: &str) -> Router {
        let pane = pane.to_string();
        Arc::new(move |_| {
            Ok(RouteReport {
                outcome: DeliveryOutcome::Delivered,
                routing: Routing::Typed {
                    pane_id: pane.clone(),
                    resolved_by: ResolvedBy::LatestNotifier,
                    tagged: false,
                    typed_at: OffsetDateTime::now_utc(),
                },
            })
        })
    }

    /// Stores a reply as the listener does, sent at `reply_at`.
    async fn store_reply(store: &store::HaroldStore, text: &str, reply_at: OffsetDateTime) {
        let message = store::InboundMessage { text: text.into() };
        let stored = crate::telemetry::store_phone_reply("imessage", reply_at, async {
            store::append_inbound_message(store, &message).await.is_ok()
        })
        .await;
        assert!(stored);
    }

    /// The one exported span called `name` whose `pane_id` is `pane`.
    fn span_on(name: &str, pane: &str) -> SpanData {
        let mut found: Vec<_> = test_support::spans()
            .into_iter()
            .filter(|span| span.name == name && span_attr(span, "pane_id").as_deref() == Some(pane))
            .collect();
        assert_eq!(found.len(), 1, "{name} spans on {pane}");
        found.remove(0)
    }

    /// The exported spans called `name` in the trace `trace_id`.
    fn in_trace(name: &str, trace_id: TraceId) -> Vec<SpanData> {
        test_support::spans()
            .into_iter()
            .filter(|span| span.name == name && span.span_context.trace_id() == trace_id)
            .collect()
    }

    #[cfg(feature = "imessage")]
    #[tokio::test]
    async fn a_reply_a_turn_end_and_its_delivery_form_one_trace() {
        test_support::install();
        let pane = unique_pane();
        let session = format!("session-{}", uuid::Uuid::new_v4());
        let directory = TestDirectory::new();
        let store = Arc::new(store::HaroldStore::open(&directory.0).await.unwrap());
        let (service, _shutdown, task) = test_service(Arc::clone(&store), empty_snapshot());
        let environment = Arc::new(FakeEnvironment::locked(""));
        let dispatcher = fake_dispatcher(&environment, typing_router(&pane));

        // 1. A reply, stored as the listener stores it and typed into the pane.
        let now = OffsetDateTime::now_utc();
        store_reply(&store, "please build it", now - time::Duration::seconds(5)).await;
        assert!(
            handle_next_delivery(&store, Arc::clone(&dispatcher))
                .await
                .unwrap()
        );

        // 2. The stop hook reports the turn's end, with the transcript time of its prompt.
        let started = now.replace_nanosecond(0).unwrap() - time::Duration::seconds(30);
        service
            .turn_complete(Request::new(TurnCompleteRequest {
                pane_id: pane.clone(),
                pane_label: "harold:0.9".into(),
                last_user_prompt: "please build it".into(),
                assistant_message: "built".into(),
                main_context: "harold".into(),
                session_id: session.clone(),
                turn_started_at: started.format(&Rfc3339).unwrap(),
                ..Default::default()
            }))
            .await
            .unwrap();

        // 3. The turn-end update is delivered.
        assert!(handle_next_delivery(&store, dispatcher).await.unwrap());
        assert_eq!(
            environment.delivered(),
            [(OutboundChannel::Away, pane.clone())]
        );

        let request = span_on("harold.phone_request", &pane);
        let trace_id = request.span_context.trace_id();
        let routes = in_trace("harold.route", trace_id);
        let turns = in_trace("harold.turn", trace_id);
        let completes = in_trace("harold.turn_complete", trace_id);
        let deliveries = in_trace("harold.delivery", trace_id);
        assert_eq!(
            (routes.len(), turns.len(), completes.len(), deliveries.len()),
            (1, 1, 1, 1)
        );
        let turn = &turns[0];
        assert_eq!(turn.parent_span_id, request.span_context.span_id());
        assert_eq!(completes[0].parent_span_id, turn.span_context.span_id());
        assert_eq!(
            deliveries[0].parent_span_id,
            completes[0].span_context.span_id()
        );
        assert_eq!(turn.start_time, std::time::SystemTime::from(started));
        assert_eq!(span_attr(turn, "pane_id").as_deref(), Some(pane.as_str()));
        assert_eq!(
            span_attr(turn, "session.id").as_deref(),
            Some(session.as_str())
        );
        assert_eq!(span_attr(turn, "started_by").as_deref(), Some("phone"));
        assert_eq!(span_attr(turn, "session.source"), None);
        assert_eq!(span_attr(turn, "turn.start"), None);
        assert_eq!(
            span_attr(&completes[0], "pane_id").as_deref(),
            Some(pane.as_str())
        );
        assert_eq!(
            span_attr(&deliveries[0], "outcome").as_deref(),
            Some("delivered")
        );

        let naming_pane: Vec<_> = test_support::logs()
            .into_iter()
            .filter(|log| {
                log.record
                    .attributes_iter()
                    .any(|(_, value)| format!("{value:?}").contains(&pane))
            })
            .collect();
        assert!(
            !naming_pane.is_empty(),
            "the turn's log lines name the pane"
        );
        for log in &naming_pane {
            assert_eq!(
                log.record.trace_context().map(|trace| trace.trace_id),
                Some(trace_id),
                "{:?}",
                log.record
            );
        }

        assert_eq!(
            test_support::counter_value(
                "harold.turns",
                &[
                    ("pane_id", pane.as_str()),
                    ("session.id", session.as_str()),
                    ("started_by", "phone"),
                ],
            ),
            Some(1)
        );
        let durations =
            test_support::histogram_sum("harold.turn.duration", &[("started_by", "phone")])
                .expect("the turn's duration is recorded");
        assert!(durations >= 30.0, "{durations}");
        task.abort();
    }

    #[tokio::test]
    async fn a_turn_end_with_no_start_information_is_marked_unknown() {
        test_support::install();
        let pane = unique_pane();
        let directory = TestDirectory::new();
        let store = Arc::new(store::HaroldStore::open(&directory.0).await.unwrap());
        let (service, _shutdown, task) = test_service(Arc::clone(&store), empty_snapshot());

        service
            .turn_complete(Request::new(TurnCompleteRequest {
                pane_id: pane.clone(),
                ..Default::default()
            }))
            .await
            .unwrap();

        let turn = span_on("harold.turn", &pane);
        assert_eq!(span_attr(&turn, "turn.start").as_deref(), Some("unknown"));
        assert_eq!(span_attr(&turn, "started_by").as_deref(), Some("unknown"));
        assert_eq!(
            span_attr(&turn, "session.id").as_deref(),
            Some(pane.as_str())
        );
        assert_eq!(span_attr(&turn, "session.source").as_deref(), Some("pane"));
        assert_eq!(
            turn.start_time, turn.end_time,
            "it starts at the turn's end"
        );
        assert_eq!(turn.parent_span_id, opentelemetry::trace::SpanId::INVALID);
        assert_eq!(
            test_support::counter_value(
                "harold.turns",
                &[
                    ("pane_id", pane.as_str()),
                    ("session.id", pane.as_str()),
                    ("started_by", "unknown"),
                ],
            ),
            Some(1)
        );
        assert_eq!(
            test_support::histogram_sum("harold.turn.duration", &[("started_by", "unknown")]),
            None
        );
        task.abort();
    }

    // --- tokens ---

    /// Puts `pane` in the last poll's snapshot with the given identity.
    fn set_pane_identity(
        pane: &str,
        role: crate::pod_stats::Role,
        product: Option<&str>,
        spec_id: Option<&str>,
    ) {
        let snapshot = crate::pod_stats::gauge_snapshot();
        snapshot.lock().unwrap().panes.insert(
            pane.to_string(),
            crate::pod_stats::PaneIdentity {
                role,
                product: product.map(str::to_owned),
                spec_id: spec_id.map(str::to_owned),
            },
        );
    }

    /// Reports a turn end with tokens from `pane`.
    async fn complete_with_tokens(pane: &str, model: &str, tokens: [u64; 4]) {
        test_support::install();
        let directory = TestDirectory::new();
        let store = Arc::new(store::HaroldStore::open(&directory.0).await.unwrap());
        let (service, _shutdown, task) = test_service(Arc::clone(&store), empty_snapshot());
        service
            .turn_complete(Request::new(TurnCompleteRequest {
                pane_id: pane.to_string(),
                model: model.to_string(),
                input_tokens: tokens[0],
                output_tokens: tokens[1],
                cache_read_tokens: tokens[2],
                cache_write_tokens: tokens[3],
                ..Default::default()
            }))
            .await
            .unwrap();
        task.abort();
    }

    fn token_count(kind: &str, model: &str, role: &str, product: &str) -> Option<u64> {
        test_support::exact_counter_value(
            "harold.turn.tokens",
            &[
                ("kind", kind),
                ("model", model),
                ("role", role),
                ("pod.product", product),
            ],
        )
    }

    #[tokio::test]
    async fn turn_complete_with_tokens_adds_four_counts() {
        let _snapshot = crate::pod_stats::GAUGE_SNAPSHOT_TESTS.lock().await;
        let pane = unique_pane();
        let product = format!("prod-{}", uuid::Uuid::new_v4().simple());
        set_pane_identity(&pane, crate::pod_stats::Role::Pnem, Some(&product), None);

        complete_with_tokens(&pane, "claude-sonnet-5-5", [1, 2, 3, 4]).await;

        for (kind, want) in [
            ("input", 1),
            ("output", 2),
            ("cache_read", 3),
            ("cache_write", 4),
        ] {
            assert_eq!(
                token_count(kind, "claude-sonnet-5-5", "pnem", &product),
                Some(want),
                "{kind}"
            );
        }
    }

    #[tokio::test]
    async fn turn_complete_from_a_build_worker_adds_four_by_spec_counts() {
        let _snapshot = crate::pod_stats::GAUGE_SNAPSHOT_TESTS.lock().await;
        let pane = unique_pane();
        let product = format!("prod-{}", uuid::Uuid::new_v4().simple());
        let spec = format!("spec-{}", uuid::Uuid::new_v4().simple());
        set_pane_identity(
            &pane,
            crate::pod_stats::Role::BuildWorker,
            Some(&product),
            Some(&spec),
        );

        complete_with_tokens(&pane, "claude-opus-5-5", [10, 7, 100, 1000]).await;

        for (kind, want) in [
            ("input", 10),
            ("output", 7),
            ("cache_read", 100),
            ("cache_write", 1000),
        ] {
            assert_eq!(
                test_support::exact_counter_value(
                    "harold.turn.tokens.by_spec",
                    &[
                        ("spec.id", &spec),
                        ("kind", kind),
                        ("model", "claude-opus-5-5")
                    ]
                ),
                Some(want),
                "{kind}"
            );
        }
    }

    #[tokio::test]
    async fn turn_complete_without_tokens_adds_none() {
        let _snapshot = crate::pod_stats::GAUGE_SNAPSHOT_TESTS.lock().await;
        let pane = unique_pane();
        let product = format!("prod-{}", uuid::Uuid::new_v4().simple());
        set_pane_identity(&pane, crate::pod_stats::Role::Pnem, Some(&product), None);

        complete_with_tokens(&pane, "claude-sonnet-5-5", [0, 0, 0, 0]).await;

        assert_eq!(
            test_support::counter_value("harold.turn.tokens", &[("pod.product", &product)]),
            None
        );
    }

    #[tokio::test]
    async fn turn_complete_with_no_model_counts_the_model_as_none() {
        let _snapshot = crate::pod_stats::GAUGE_SNAPSHOT_TESTS.lock().await;
        let pane = unique_pane();
        let product = format!("prod-{}", uuid::Uuid::new_v4().simple());
        set_pane_identity(&pane, crate::pod_stats::Role::Pnem, Some(&product), None);

        complete_with_tokens(&pane, "", [5, 0, 0, 0]).await;

        assert_eq!(token_count("input", "none", "pnem", &product), Some(5));
    }

    #[tokio::test]
    async fn turn_complete_with_a_strange_model_counts_as_other() {
        let _snapshot = crate::pod_stats::GAUGE_SNAPSHOT_TESTS.lock().await;
        let pane = unique_pane();
        let product = format!("prod-{}", uuid::Uuid::new_v4().simple());
        set_pane_identity(&pane, crate::pod_stats::Role::Pnem, Some(&product), None);

        complete_with_tokens(&pane, "Strange\nModel", [5, 0, 0, 0]).await;

        assert_eq!(token_count("input", "other", "pnem", &product), Some(5));
    }

    #[tokio::test]
    async fn turn_complete_turns_counter_has_role_and_product_and_no_model() {
        let _snapshot = crate::pod_stats::GAUGE_SNAPSHOT_TESTS.lock().await;
        let pane = unique_pane();
        let product = format!("prod-{}", uuid::Uuid::new_v4().simple());
        set_pane_identity(&pane, crate::pod_stats::Role::Pnem, Some(&product), None);

        complete_with_tokens(&pane, "claude-sonnet-5-5", [1, 1, 1, 1]).await;

        assert_eq!(
            test_support::counter_value(
                "harold.turns",
                &[
                    ("pane_id", pane.as_str()),
                    ("role", "pnem"),
                    ("pod.product", &product)
                ],
            ),
            Some(1)
        );
        assert_eq!(
            test_support::counter_value(
                "harold.turns",
                &[("pane_id", pane.as_str()), ("model", "claude-sonnet-5-5")]
            ),
            None
        );
    }

    #[tokio::test]
    async fn turn_complete_from_a_pane_not_in_the_last_poll_counts_as_other() {
        let _snapshot = crate::pod_stats::GAUGE_SNAPSHOT_TESTS.lock().await;
        let pane = unique_pane();
        // A model no other test uses, so the counter point is this test's alone.
        let model = format!("claude-test-{}", uuid::Uuid::new_v4().simple());

        complete_with_tokens(&pane, &model, [1, 0, 0, 0]).await;

        assert_eq!(
            test_support::counter_value(
                "harold.turns",
                &[("pane_id", pane.as_str()), ("role", "other")],
            ),
            Some(1)
        );
        assert_eq!(
            test_support::exact_counter_value(
                "harold.turn.tokens",
                &[("role", "other"), ("model", &model), ("kind", "input")],
            ),
            Some(1)
        );
        // The pane has no spec, so no by-spec series is recorded for it.
        assert_eq!(
            test_support::counter_value("harold.turn.tokens.by_spec", &[("spec.id", "")]),
            None
        );
    }

    #[cfg(feature = "imessage")]
    #[tokio::test]
    async fn no_text_leaves_harold() {
        test_support::install();
        let canary = format!("TELEMETRY-CANARY-7731-{}", uuid::Uuid::new_v4().simple());
        let pane = unique_pane();
        let directory = TestDirectory::new();
        let store = Arc::new(store::HaroldStore::open(&directory.0).await.unwrap());
        let (service, _shutdown, task) = test_service(Arc::clone(&store), empty_snapshot());
        let environment = Arc::new(FakeEnvironment::locked(""));
        let dispatcher = fake_dispatcher(&environment, typing_router(&pane));

        store_reply(&store, &canary, OffsetDateTime::now_utc()).await;
        assert!(
            handle_next_delivery(&store, Arc::clone(&dispatcher))
                .await
                .unwrap()
        );
        service
            .turn_complete(Request::new(TurnCompleteRequest {
                pane_id: pane.clone(),
                pane_label: canary.clone(),
                last_user_prompt: canary.clone(),
                assistant_message: canary.clone(),
                main_context: canary.clone(),
                ..Default::default()
            }))
            .await
            .unwrap();
        environment.fail_next_send_with(&format!("send failed: {canary}"));
        let failed = handle_next_delivery(&store, Arc::clone(&dispatcher)).await;
        assert!(
            failed
                .as_ref()
                .is_err_and(|error| error.to_string().contains(&canary)),
            "the first delivery fails with an error holding the canary"
        );
        assert!(handle_next_delivery(&store, dispatcher).await.unwrap());
        assert_eq!(
            environment.delivered(),
            [(OutboundChannel::Away, pane.clone())]
        );

        let trace_id = span_on("harold.phone_request", &pane)
            .span_context
            .trace_id();
        let outcomes: Vec<_> = in_trace("harold.delivery", trace_id)
            .iter()
            .map(|span| span_attr(span, "outcome").unwrap_or_default())
            .collect();
        assert_eq!(outcomes, ["failed", "delivered"]);
        assert!(
            !test_support::exported_text().contains(&canary),
            "the canary was exported"
        );
        task.abort();
    }

    #[tokio::test]
    async fn a_turn_end_left_out_at_the_desk_is_logged_inside_the_turns_trace() {
        test_support::install();
        let pane = unique_pane();
        let directory = TestDirectory::new();
        let store = Arc::new(store::HaroldStore::open(&directory.0).await.unwrap());
        let (service, _shutdown, task) = test_service(Arc::clone(&store), empty_snapshot());
        let environment = Arc::new(FakeEnvironment::new(""));
        let dispatcher = fake_dispatcher(&environment, typing_router(&pane));

        store_reply(&store, "please build it", OffsetDateTime::now_utc()).await;
        assert!(
            handle_next_delivery(&store, Arc::clone(&dispatcher))
                .await
                .unwrap()
        );
        service
            .turn_complete(Request::new(TurnCompleteRequest {
                pane_id: pane.clone(),
                ..Default::default()
            }))
            .await
            .unwrap();
        let logs = crate::outbound::fake::CapturedLogs::start();
        assert!(handle_next_delivery(&store, dispatcher).await.unwrap());

        assert!(environment.delivered().is_empty());
        let trace_id = span_on("harold.phone_request", &pane)
            .span_context
            .trace_id();
        let deliveries = in_trace("harold.delivery", trace_id);
        assert_eq!(deliveries.len(), 1);
        assert_eq!(
            span_attr(&deliveries[0], "outcome").as_deref(),
            Some("held_back")
        );
        let skipped_in_trace = test_support::logs().iter().any(|log| {
            log.record
                .body()
                .is_some_and(|body| format!("{body:?}").contains("delivery intentionally skipped"))
                && log.record.trace_context().map(|trace| trace.trace_id) == Some(trace_id)
        });
        assert!(
            skipped_in_trace,
            "the skipped line carries the turn's trace"
        );
        // harold.log writes the line as before, with no span prefix.
        assert!(
            logs.text()
                .contains("INFO harold::projector: delivery intentionally skipped event_id="),
            "{}",
            logs.text()
        );
        task.abort();
    }

    #[tokio::test]
    async fn the_turn_exports_the_pane_id_as_harold_log_shows_it() {
        test_support::install();
        let session = format!("session-{}", uuid::Uuid::new_v4());
        // An id with no local form is dropped before the trace, so the id here is a local one.
        let pane_id = format!("%{}", uuid::Uuid::new_v4().as_u128() % 10u128.pow(18));
        let directory = TestDirectory::new();
        let store = Arc::new(store::HaroldStore::open(&directory.0).await.unwrap());
        let (service, _shutdown, task) = test_service(Arc::clone(&store), empty_snapshot());

        service
            .turn_complete(Request::new(TurnCompleteRequest {
                pane_id: pane_id.clone(),
                session_id: session.clone(),
                ..Default::default()
            }))
            .await
            .unwrap();

        let turns: Vec<_> = test_support::spans()
            .into_iter()
            .filter(|span| {
                span.name == "harold.turn"
                    && span_attr(span, "session.id").as_deref() == Some(session.as_str())
            })
            .collect();
        assert_eq!(turns.len(), 1);
        assert_eq!(
            span_attr(&turns[0], "pane_id").as_deref(),
            Some(pane_id.as_str())
        );
        assert_eq!(
            test_support::counter_value(
                "harold.turns",
                &[
                    ("session.id", session.as_str()),
                    ("pane_id", pane_id.as_str())
                ],
            ),
            Some(1)
        );
        task.abort();
    }
}

#[tokio::test]
async fn with_export_off_nothing_is_opened_or_run() {
    let cfg = crate::settings::settings_with("[telemetry]\nexport = false\n");
    assert!(cfg.telemetry.otlp_endpoint.is_some());
    assert!(!cfg.telemetry.export);

    let (guard, build_failed) =
        crate::telemetry::TelemetryGuard::for_settings(&cfg.telemetry, &cfg.machine_name);
    assert!(!guard.exporting(), "no export pipeline is built");
    assert!(build_failed.is_none());

    let (shutdown_tx, shutdown_rx) = watch::channel(());
    let tasks =
        super::start_telemetry_tasks(&cfg, &AgentSnapshotHub::new(empty_snapshot()), &shutdown_rx)
            .await;
    assert!(
        tasks.machine_stats.is_none(),
        "export off, so no machine read"
    );
    assert!(tasks.pod_stats.is_none(), "no pod facts, so no git command");
    assert!(tasks.alerts.is_none(), "no alert address");
    drop(shutdown_tx);
    tasks.stopped().await;
    guard.shutdown().await;
}

#[tokio::test]
async fn a_taken_alert_address_logs_a_warning_and_harold_runs_without_it() {
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = taken.local_addr().unwrap();
    let cfg =
        crate::settings::settings_with(&format!("[telemetry]\nalert_listen = \"{address}\"\n"));
    let logs = crate::outbound::fake::CapturedLogs::start();

    let (shutdown_tx, shutdown_rx) = watch::channel(());
    let tasks =
        super::start_telemetry_tasks(&cfg, &AgentSnapshotHub::new(empty_snapshot()), &shutdown_rx)
            .await;

    assert!(tasks.alerts.is_none(), "no alert address");
    let lines = logs.text();
    let warning = lines
        .lines()
        .find(|line| line.contains("alert address unavailable"))
        .unwrap_or_else(|| panic!("no warning in {lines}"));
    assert!(warning.trim_start().starts_with("WARN"), "{warning}");
    assert!(warning.contains("reason=\"bind_failed\""), "{warning}");
    drop(shutdown_tx);
    tasks.stopped().await;
    drop(taken);
}

#[tokio::test]
async fn each_telemetry_key_starts_its_own_task() {
    let missing = std::env::temp_dir().join(format!("no-registry-{}.toml", uuid::Uuid::new_v4()));
    let cfg = crate::settings::settings_with(&format!(
        "[telemetry]\nalert_listen = \"127.0.0.1:0\"\npod_registry = {:?}\n",
        missing.display().to_string()
    ));

    let (shutdown_tx, shutdown_rx) = watch::channel(());
    let tasks =
        super::start_telemetry_tasks(&cfg, &AgentSnapshotHub::new(empty_snapshot()), &shutdown_rx)
            .await;
    assert!(tasks.pod_stats.is_some());
    assert!(tasks.alerts.is_some());
    drop(shutdown_tx);
    tokio::time::timeout(std::time::Duration::from_secs(5), tasks.stopped())
        .await
        .expect("both stop at shutdown");
}

#[tokio::test]
async fn a_store_in_use_stops_harold_with_the_error_text_not_its_debug_form() {
    let directory = std::env::temp_dir().join(format!("harold-in-use-{}", uuid::Uuid::new_v4()));
    let first = store::HaroldStore::open(&directory).await.unwrap();
    let Err(error) = store::HaroldStore::open(&directory).await else {
        panic!("a second store on the same folder must be refused");
    };
    let logs = crate::outbound::fake::CapturedLogs::start();

    let line = super::report_store_open_failure(&directory.display().to_string(), &error);

    assert_eq!(line, "store is in use by another Harold");
    let lines = logs.text();
    let logged = lines
        .lines()
        .find(|line| line.contains("store could not be opened"))
        .unwrap_or_else(|| panic!("no error in {lines}"));
    assert!(logged.trim_start().starts_with("ERROR"), "{logged}");
    assert!(
        logged.contains("error=store is in use by another Harold"),
        "{logged}"
    );
    assert!(!lines.contains("InUse"), "{lines}");
    drop(first);
    let _ = std::fs::remove_dir_all(&directory);
}

// ---------------------------------------------------------------------------
// Calls with the chief
// ---------------------------------------------------------------------------

fn call_request(action: super::harold::CallAction) -> Request<super::harold::CallRequest> {
    Request::new(super::harold::CallRequest {
        action: action as i32,
    })
}

#[tokio::test]
async fn call_answers_not_set_up_without_daemon_calls() {
    use super::harold::{CallAction, CallResult, CallState};
    let directory = TestDirectory::new();
    let store = Arc::new(store::HaroldStore::open(&directory.0).await.unwrap());
    let (service, _shutdown, task) = test_service(store, empty_snapshot());

    for action in [
        CallAction::Start,
        CallAction::Stop,
        CallAction::TalkNow,
        CallAction::Status,
    ] {
        let answer = service
            .call(call_request(action))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(answer.state, CallState::NotSetUp as i32, "{action:?}");
        assert_eq!(answer.last_result, CallResult::Unspecified as i32);
        assert_eq!(answer.chief_pane_id, "");
        assert_eq!(answer.last_transcript, "");
    }
    task.abort();
}

#[tokio::test]
async fn call_without_an_action_is_invalid() {
    let directory = TestDirectory::new();
    let store = Arc::new(store::HaroldStore::open(&directory.0).await.unwrap());
    let (service, _shutdown, task) = test_service(store, empty_snapshot());

    for action in [super::harold::CallAction::Unspecified as i32, 99] {
        let status = service
            .call(Request::new(super::harold::CallRequest { action }))
            .await
            .unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert_eq!(status.message(), "call action is required");
    }
    task.abort();
}

#[test]
fn call_response_maps_every_state_and_result() {
    use super::harold::{CallResult, CallState};
    use super::voice_input::{self, CallStatus, TurnResult};
    let states = [
        (voice_input::CallState::NotSetUp, CallState::NotSetUp),
        (voice_input::CallState::Off, CallState::Off),
        (voice_input::CallState::Connecting, CallState::Listening),
        (voice_input::CallState::Listening, CallState::Listening),
        (
            voice_input::CallState::Transcribing,
            CallState::Transcribing,
        ),
        (voice_input::CallState::Waiting, CallState::Waiting),
        (voice_input::CallState::Speaking, CallState::Speaking),
    ];
    let results = [
        (None, CallResult::Unspecified),
        (Some(TurnResult::Typed), CallResult::Typed),
        (Some(TurnResult::NothingHeard), CallResult::NothingHeard),
        (Some(TurnResult::NoChief), CallResult::NoChief),
        (Some(TurnResult::ChiefBusy), CallResult::ChiefBusy),
        (Some(TurnResult::Failed), CallResult::Failed),
        (Some(TurnResult::VoiceMuted), CallResult::VoiceMuted),
    ];
    for (state, wire_state) in states {
        for (result, wire_result) in results {
            let answer = super::call_response(&CallStatus {
                state,
                chief_pane_id: Some("%3".into()),
                last_transcript: Some("check the build".into()),
                last_result: result,
                model_percent: None,
                opened_at_ms: None,
            });
            assert_eq!(answer.state, wire_state as i32);
            assert_eq!(answer.last_result, wire_result as i32);
            assert_eq!(answer.chief_pane_id, "%3");
            assert_eq!(answer.last_transcript, "check the build");
        }
    }
    let empty = super::call_response(&CallStatus {
        state: voice_input::CallState::Off,
        chief_pane_id: None,
        last_transcript: None,
        last_result: None,
        model_percent: None,
        opened_at_ms: None,
    });
    assert_eq!(empty.chief_pane_id, "");
    assert_eq!(empty.last_transcript, "");
}

#[tokio::test]
async fn the_log_trim_starts_only_when_launchd_started_harold() {
    let (shutdown_tx, shutdown_rx) = watch::channel(());
    // The signal is already sent, so a started trim ends without touching any file.
    drop(shutdown_tx);

    assert!(super::start_log_trim(false, &shutdown_rx).is_none());
    let started = super::start_log_trim(true, &shutdown_rx).expect("trim starts under launchd");
    started.await.expect("trim stops with the shutdown signal");
}

/// The pane and agent pid of `populated_snapshot`.
const STEP_PANE: &str = "%8";
const STEP_PID: u32 = 82;

fn step_of(snapshot: &super::harold::AgentStateSnapshot) -> Option<String> {
    snapshot.panes[0].current_step.clone()
}

/// Runs `forward_agent_snapshots` over a hub and the steps, and gives the stream end.
fn step_stream(
    steps: &CurrentSteps,
) -> (
    tokio::sync::mpsc::Receiver<Result<super::harold::AgentStateSnapshot, tonic::Status>>,
    watch::Sender<()>,
    AgentSnapshotHub,
) {
    recap_stream(steps, &Recaps::default())
}

/// Like `step_stream`, with the recaps the stream joins.
fn recap_stream(
    steps: &CurrentSteps,
    recaps: &Recaps,
) -> (
    tokio::sync::mpsc::Receiver<Result<super::harold::AgentStateSnapshot, tonic::Status>>,
    watch::Sender<()>,
    AgentSnapshotHub,
) {
    named_stream(steps, recaps, &WindowNames::default())
}

/// Like `recap_stream`, with the window names the stream joins.
fn named_stream(
    steps: &CurrentSteps,
    recaps: &Recaps,
    names: &WindowNames,
) -> (
    tokio::sync::mpsc::Receiver<Result<super::harold::AgentStateSnapshot, tonic::Status>>,
    watch::Sender<()>,
    AgentSnapshotHub,
) {
    let hub = AgentSnapshotHub::new(populated_snapshot(1, None));
    let (sender, receiver) = tokio::sync::mpsc::channel(8);
    let (shutdown, shutdown_rx) = watch::channel(());
    tokio::spawn(super::forward_agent_snapshots(
        hub.subscribe(),
        steps.clone(),
        recaps.clone(),
        HostAgents::default(),
        names.clone(),
        String::new(),
        RoleSources::default(),
        watch::channel(crate::tools::WatchCounts::default()).1,
        watch::channel(WaitingView::default()).1,
        sender,
        shutdown_rx,
    ));
    (receiver, shutdown, hub)
}

/// Runs `forward_agent_snapshots` with a count channel the test sends on.
fn counts_stream() -> (
    tokio::sync::mpsc::Receiver<Result<super::harold::AgentStateSnapshot, tonic::Status>>,
    watch::Sender<()>,
    watch::Sender<crate::tools::WatchCounts>,
) {
    counts_stream_from(crate::tools::WatchCounts::default())
}

fn counts_stream_from(
    first: crate::tools::WatchCounts,
) -> (
    tokio::sync::mpsc::Receiver<Result<super::harold::AgentStateSnapshot, tonic::Status>>,
    watch::Sender<()>,
    watch::Sender<crate::tools::WatchCounts>,
) {
    let hub = AgentSnapshotHub::new(populated_snapshot(1, None));
    let (counts, counts_rx) = watch::channel(first);
    let (sender, receiver) = tokio::sync::mpsc::channel(8);
    let (shutdown, shutdown_rx) = watch::channel(());
    tokio::spawn(async move {
        super::forward_agent_snapshots(
            hub.subscribe(),
            CurrentSteps::default(),
            Recaps::default(),
            HostAgents::default(),
            WindowNames::default(),
            String::new(),
            RoleSources::default(),
            counts_rx,
            watch::channel(WaitingView::default()).1,
            sender,
            shutdown_rx,
        )
        .await;
        drop(hub);
    });
    (receiver, shutdown, counts)
}

fn watch_counts_of(snapshot: &super::harold::AgentStateSnapshot) -> (u32, u32, u32) {
    (
        snapshot.watches_running,
        snapshot.watches_paused,
        snapshot.watches_capped,
    )
}

fn counts(running: u32, paused: u32, capped: u32) -> crate::tools::WatchCounts {
    crate::tools::WatchCounts {
        running,
        paused,
        capped,
    }
}

#[tokio::test]
async fn the_first_snapshot_holds_the_counts_the_watch_tool_sent_before_the_stream() {
    let (mut receiver, _shutdown, _counts) = counts_stream_from(counts(2, 1, 1));
    let item = next_item(&mut receiver).await;
    assert_eq!(watch_counts_of(&item), (2, 1, 1));
}

#[tokio::test]
async fn the_snapshot_counts_follow_start_pause_resume_cap_and_stop() {
    let (mut receiver, _shutdown, sender) = counts_stream();
    let first = next_item(&mut receiver).await;
    assert_eq!(watch_counts_of(&first), (0, 0, 0));
    let version = first.through_event_version;

    // start, pause, resume, cap, stop of the paused one, stop of the capped one
    let steps = [
        (counts(1, 0, 0), (1, 0, 0)),
        (counts(0, 1, 0), (0, 1, 0)),
        (counts(1, 0, 0), (1, 0, 0)),
        (counts(1, 0, 1), (1, 0, 1)),
        (counts(0, 0, 1), (0, 0, 1)),
        (counts(0, 0, 0), (0, 0, 0)),
    ];
    for (sent, expected) in steps {
        sender.send_replace(sent);
        let item = next_item(&mut receiver).await;
        assert_eq!(watch_counts_of(&item), expected);
        assert_eq!(
            item.through_event_version, version,
            "a count change does not raise the event version"
        );
    }
    no_item(&mut receiver).await;
}

#[tokio::test]
async fn a_count_change_sends_one_snapshot_with_the_agent_state_unchanged() {
    let (mut receiver, _shutdown, sender) = counts_stream();
    let first = next_item(&mut receiver).await;

    sender.send_replace(counts(3, 0, 0));
    let second = next_item(&mut receiver).await;

    assert_eq!(second.panes, first.panes);
    assert_eq!(second.through_event_version, first.through_event_version);
    no_item(&mut receiver).await;
}

#[tokio::test]
async fn the_stream_goes_on_after_the_watch_tool_is_gone() {
    let (mut receiver, _shutdown, sender) = counts_stream();
    next_item(&mut receiver).await;
    sender.send_replace(counts(1, 0, 0));
    next_item(&mut receiver).await;

    drop(sender);

    no_item(&mut receiver).await;
    assert!(
        !receiver.is_closed(),
        "the stream stays open with the last counts"
    );
}

async fn next_item(
    receiver: &mut tokio::sync::mpsc::Receiver<
        Result<super::harold::AgentStateSnapshot, tonic::Status>,
    >,
) -> super::harold::AgentStateSnapshot {
    tokio::time::timeout(std::time::Duration::from_secs(5), receiver.recv())
        .await
        .expect("a stream item in time")
        .expect("the stream is open")
        .expect("an item, not an error")
}

async fn no_item(
    receiver: &mut tokio::sync::mpsc::Receiver<
        Result<super::harold::AgentStateSnapshot, tonic::Status>,
    >,
) {
    let waited = tokio::time::timeout(std::time::Duration::from_millis(300), receiver.recv()).await;
    assert!(waited.is_err(), "no stream item is expected");
}

#[tokio::test]
async fn a_working_record_gives_current_step_on_the_stream() {
    let steps = CurrentSteps::default();
    steps.set(STEP_PANE, STEP_PID, Some("Run the tests".into()));
    let (mut receiver, _shutdown, _hub) = step_stream(&steps);
    let item = next_item(&mut receiver).await;
    assert_eq!(step_of(&item).as_deref(), Some("Run the tests"));
    // A step of another agent pid in the same pane does not show.
    steps.set(STEP_PANE, STEP_PID + 1, Some("Other agent".into()));
    let item = next_item(&mut receiver).await;
    assert_eq!(step_of(&item), None);
}

#[tokio::test]
async fn a_blocked_record_gives_its_step() {
    let steps = CurrentSteps::default();
    steps.set(STEP_PANE, STEP_PID, Some("Allow Bash?".into()));
    let mapped = map_agent_snapshot(
        populated_snapshot(1, None),
        &steps,
        &Recaps::default(),
        &HostAgents::default(),
        &WindowNames::default(),
        "",
        &RoleSources::default(),
    );
    assert_eq!(step_of(&mapped).as_deref(), Some("Allow Bash?"));
}

/// The writer drops the step of a done, idle or error record, and of a record with no
/// `msg`. The tests `a_done_idle_and_error_record_write_no_step` and
/// `done_idle_and_error_records_give_no_step` in `agent/rex_tests.rs` prove that.
#[test]
fn a_done_idle_or_error_record_and_a_record_with_no_msg_give_none() {
    let steps = CurrentSteps::default();
    steps.set(STEP_PANE, STEP_PID, None);
    let mapped = map_agent_snapshot(
        populated_snapshot(1, None),
        &steps,
        &Recaps::default(),
        &HostAgents::default(),
        &WindowNames::default(),
        "",
        &RoleSources::default(),
    );
    assert_eq!(step_of(&mapped), None);
    // A step that is removed goes from the next snapshot too.
    steps.set(STEP_PANE, STEP_PID, Some("Run".into()));
    steps.set(STEP_PANE, STEP_PID, None);
    let mapped = map_agent_snapshot(
        populated_snapshot(1, None),
        &steps,
        &Recaps::default(),
        &HostAgents::default(),
        &WindowNames::default(),
        "",
        &RoleSources::default(),
    );
    assert_eq!(step_of(&mapped), None);
}

#[tokio::test]
async fn a_new_msg_sends_a_new_snapshot() {
    let steps = CurrentSteps::default();
    let (mut receiver, _shutdown, _hub) = step_stream(&steps);
    assert_eq!(step_of(&next_item(&mut receiver).await), None);
    steps.set(STEP_PANE, STEP_PID, Some("First".into()));
    assert_eq!(
        step_of(&next_item(&mut receiver).await).as_deref(),
        Some("First")
    );
    // The same step again changes nothing.
    steps.set(STEP_PANE, STEP_PID, Some("First".into()));
    no_item(&mut receiver).await;
    steps.set(STEP_PANE, STEP_PID, Some("Second".into()));
    assert_eq!(
        step_of(&next_item(&mut receiver).await).as_deref(),
        Some("Second")
    );
}

const LEAKCHECK: &str = "LEAKCHECK-7f3a";

/// Opens a store and a service whose steps hold `LEAKCHECK`, and reads one stream item.
/// The item holds the marker: that proves the marker is in play where the tests look.
async fn watch_with_the_marker(
    directory: &std::path::Path,
) -> (
    Arc<store::HaroldStore>,
    HaroldService,
    <HaroldService as Harold>::WatchAgentStatesStream,
    watch::Sender<()>,
) {
    let store = Arc::new(store::HaroldStore::open(directory).await.unwrap());
    let (service, shutdown, _task) = test_service(
        Arc::clone(&store),
        populated_snapshot(1, Some("Plain summary")),
    );
    service
        .current_steps
        .set(STEP_PANE, STEP_PID, Some(LEAKCHECK.into()));
    let mut stream = service
        .watch_agent_states(Request::new(WatchAgentStatesRequest {}))
        .await
        .unwrap()
        .into_inner();
    let item = stream.next().await.expect("an item").unwrap();
    assert_eq!(step_of(&item).as_deref(), Some(LEAKCHECK));
    assert!(!format!("{:?}", item.panes[0].work_summary).contains(LEAKCHECK));
    (store, service, stream, shutdown)
}

#[tokio::test]
async fn the_current_step_is_not_in_the_captured_logs() {
    let logs = crate::outbound::fake::CapturedLogs::start();
    let directory = std::env::temp_dir().join(format!("harold-leak-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let (_store, service, mut stream, _shutdown) = watch_with_the_marker(&directory).await;
    service
        .current_steps
        .set(STEP_PANE, STEP_PID, Some(format!("{LEAKCHECK} again")));
    // The step change reaches the live stream; the logs stay clean.
    let second = stream.next().await.expect("a second item").unwrap();
    assert_eq!(step_of(&second), Some(format!("{LEAKCHECK} again")));
    assert!(!logs.text().contains(LEAKCHECK), "{}", logs.text());
    let _ = std::fs::remove_dir_all(&directory);
}

#[tokio::test]
async fn the_current_step_is_not_in_the_exported_spans_or_log_records() {
    crate::telemetry::test_support::install();
    let directory = std::env::temp_dir().join(format!("harold-leak-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let (_store, _service, _stream, _shutdown) = watch_with_the_marker(&directory).await;
    let spans = format!("{:?}", crate::telemetry::test_support::spans());
    let records = format!("{:?}", crate::telemetry::test_support::logs());
    assert!(!spans.contains(LEAKCHECK));
    assert!(!records.contains(LEAKCHECK));
    let _ = std::fs::remove_dir_all(&directory);
}

#[tokio::test]
async fn the_current_step_is_not_in_the_stored_rows() {
    let directory = std::env::temp_dir().join(format!("harold-leak-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let (store, service, _stream, _shutdown) = watch_with_the_marker(&directory).await;
    super::projector::project_and_publish_agent_snapshot(&store, &service.snapshots, 500)
        .await
        .unwrap();
    let stored = format!("{:?}", store.load_agent_snapshot().await.unwrap());
    assert!(!stored.contains(LEAKCHECK));
    let mut files = vec![directory.clone()];
    while let Some(path) = files.pop() {
        for entry in std::fs::read_dir(&path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                files.push(path);
                continue;
            }
            let bytes = std::fs::read(&path).unwrap();
            assert!(
                !bytes
                    .windows(LEAKCHECK.len())
                    .any(|w| w == LEAKCHECK.as_bytes()),
                "{path:?}"
            );
        }
    }
    let _ = std::fs::remove_dir_all(&directory);
}

#[tokio::test]
async fn a_turn_end_of_a_pane_in_the_snapshot_keeps_its_agent_pid_and_provider() {
    let directory = TestDirectory::new();
    let store = Arc::new(store::HaroldStore::open(&directory.0).await.unwrap());
    let (service, _shutdown, task) = test_service(store, populated_snapshot(1, None));

    service
        .turn_path()
        .note_recap_turn_end("%8", "s-1", "/t/s-1.jsonl");

    let targets = service.recaps.targets();
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].pane_id, "%8");
    assert_eq!(targets[0].agent_pid, 82);
    assert_eq!(targets[0].provider_id, "codex");
    assert_eq!(targets[0].session_id, "s-1");
    assert_eq!(targets[0].transcript_path, "/t/s-1.jsonl");
    task.abort();
}

#[tokio::test]
async fn a_turn_end_with_no_session_id_or_no_transcript_path_keeps_nothing() {
    let directory = TestDirectory::new();
    let store = Arc::new(store::HaroldStore::open(&directory.0).await.unwrap());
    let (service, _shutdown, task) = test_service(store, populated_snapshot(1, None));

    service
        .turn_path()
        .note_recap_turn_end("%8", "", "/t/s-1.jsonl");
    service.turn_path().note_recap_turn_end("%8", "s-1", "");

    assert!(service.recaps.targets().is_empty());
    task.abort();
}

#[tokio::test]
async fn a_turn_end_of_a_pane_absent_from_the_snapshot_keeps_nothing() {
    let directory = TestDirectory::new();
    let store = Arc::new(store::HaroldStore::open(&directory.0).await.unwrap());
    let (service, _shutdown, task) = test_service(store, populated_snapshot(1, None));

    service
        .turn_path()
        .note_recap_turn_end("%99", "s-1", "/t/s-1.jsonl");

    assert!(service.recaps.targets().is_empty());
    task.abort();
}

#[tokio::test]
async fn a_turn_end_with_a_new_session_id_replaces_the_entry_and_its_recap() {
    let directory = TestDirectory::new();
    let store = Arc::new(store::HaroldStore::open(&directory.0).await.unwrap());
    let (service, _shutdown, task) = test_service(store, populated_snapshot(1, None));
    service
        .turn_path()
        .note_recap_turn_end("%8", "s-1", "/t/s-1.jsonl");
    let target = service.recaps.targets().remove(0);
    service.recaps.update_read(
        "%8",
        82,
        "s-1",
        "/t/s-1.jsonl",
        target.tail,
        Some(crate::agent::recap::RecapLine {
            text: "Old recap.".into(),
            at_ms: 1,
        }),
    );
    assert!(service.recaps.get("%8", 82).is_some());

    service
        .turn_path()
        .note_recap_turn_end("%8", "s-2", "/t/s-2.jsonl");

    let targets = service.recaps.targets();
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].session_id, "s-2");
    assert!(service.recaps.get("%8", 82).is_none());
    task.abort();
}

#[tokio::test]
async fn turn_complete_keeps_the_session_and_transcript_for_the_recap_reader() {
    let directory = TestDirectory::new();
    let store = Arc::new(store::HaroldStore::open(&directory.0).await.unwrap());
    let (service, _shutdown, task) = test_service(store, populated_snapshot(1, None));

    service
        .turn_complete(Request::new(TurnCompleteRequest {
            pane_id: "%8".into(),
            session_id: "s-1".into(),
            transcript_path: "/t/s-1.jsonl".into(),
            ..Default::default()
        }))
        .await
        .unwrap();

    let targets = service.recaps.targets();
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].agent_pid, 82);
    assert_eq!(targets[0].transcript_path, "/t/s-1.jsonl");
    task.abort();
}

const RECAP_MARKER: &str = "RECAPCHECK-91bd";

fn recap_of(snapshot: &super::harold::AgentStateSnapshot) -> (Option<String>, i64) {
    let pane = &snapshot.panes[0];
    (pane.recap.clone(), pane.recap_at_ms)
}

#[tokio::test]
async fn a_recap_reaches_the_stream_with_its_time() {
    let recaps = Recaps::default();
    recaps.put_for_test(STEP_PANE, STEP_PID, "Plan done", 1_234);
    let (mut receiver, _shutdown, _hub) = recap_stream(&CurrentSteps::default(), &recaps);
    let item = next_item(&mut receiver).await;
    assert_eq!(recap_of(&item), (Some("Plan done".to_string()), 1_234));
    // A recap of another agent pid in the same pane does not show.
    let other = Recaps::default();
    other.put_for_test(STEP_PANE, STEP_PID + 1, "Other agent", 9);
    let mapped = map_agent_snapshot(
        populated_snapshot(1, None),
        &CurrentSteps::default(),
        &other,
        &HostAgents::default(),
        &WindowNames::default(),
        "",
        &RoleSources::default(),
    );
    assert_eq!(recap_of(&mapped), (None, 0));
}

#[test]
fn a_pane_without_a_recap_has_none_and_zero() {
    let mapped = map_agent_snapshot(
        populated_snapshot(1, None),
        &CurrentSteps::default(),
        &Recaps::default(),
        &HostAgents::default(),
        &WindowNames::default(),
        "",
        &RoleSources::default(),
    );
    assert_eq!(recap_of(&mapped), (None, 0));
}

#[tokio::test]
async fn a_new_recap_sends_a_new_snapshot_at_the_same_event_version() {
    let recaps = Recaps::default();
    let (mut receiver, _shutdown, _hub) = recap_stream(&CurrentSteps::default(), &recaps);
    let first = next_item(&mut receiver).await;
    assert_eq!(recap_of(&first), (None, 0));
    recaps.put_for_test(STEP_PANE, STEP_PID, "First", 10);
    let second = next_item(&mut receiver).await;
    assert_eq!(recap_of(&second), (Some("First".to_string()), 10));
    assert_eq!(second.through_event_version, first.through_event_version);
    // The same recap again changes nothing.
    recaps.put_for_test(STEP_PANE, STEP_PID, "First", 10);
    no_item(&mut receiver).await;
    recaps.put_for_test(STEP_PANE, STEP_PID, "Second", 20);
    let third = next_item(&mut receiver).await;
    assert_eq!(recap_of(&third), (Some("Second".to_string()), 20));
    assert_eq!(third.through_event_version, first.through_event_version);
}

#[tokio::test]
async fn recap_and_current_step_reach_the_stream_together() {
    let steps = CurrentSteps::default();
    let recaps = Recaps::default();
    recaps.put_for_test(STEP_PANE, STEP_PID, "Recap text", 77);
    // A blocked record gives its msg.
    steps.set(STEP_PANE, STEP_PID, Some("Allow Bash?".into()));
    let (mut receiver, _shutdown, _hub) = recap_stream(&steps, &recaps);
    let item = next_item(&mut receiver).await;
    assert_eq!(step_of(&item).as_deref(), Some("Allow Bash?"));
    assert_eq!(recap_of(&item), (Some("Recap text".to_string()), 77));
    // A done, idle or error record gives no step; the recap stays.
    steps.set(STEP_PANE, STEP_PID, None);
    let item = next_item(&mut receiver).await;
    assert_eq!(step_of(&item), None);
    assert_eq!(recap_of(&item), (Some("Recap text".to_string()), 77));
}

#[tokio::test]
async fn the_recap_is_not_in_the_captured_logs_spans_or_stored_rows() {
    let logs = crate::outbound::fake::CapturedLogs::start();
    crate::telemetry::test_support::install();
    let directory = std::env::temp_dir().join(format!("harold-leak-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let store = Arc::new(store::HaroldStore::open(&directory).await.unwrap());
    let (service, _shutdown, _task) = test_service(
        Arc::clone(&store),
        populated_snapshot(1, Some("Plain summary")),
    );
    service
        .recaps
        .put_for_test(STEP_PANE, STEP_PID, RECAP_MARKER, 5);
    let mut stream = service
        .watch_agent_states(Request::new(WatchAgentStatesRequest {}))
        .await
        .unwrap()
        .into_inner();
    // The stream holds the marker: that proves the marker is in play where the tests look.
    let item = stream.next().await.expect("an item").unwrap();
    assert_eq!(recap_of(&item).0.as_deref(), Some(RECAP_MARKER));
    assert!(!format!("{:?}", item.panes[0].work_summary).contains(RECAP_MARKER));
    service
        .recaps
        .put_for_test(STEP_PANE, STEP_PID, &format!("{RECAP_MARKER} again"), 6);
    let second = stream.next().await.expect("a second item").unwrap();
    assert_eq!(recap_of(&second).0, Some(format!("{RECAP_MARKER} again")));

    assert!(!logs.text().contains(RECAP_MARKER), "{}", logs.text());
    let spans = format!("{:?}", crate::telemetry::test_support::spans());
    let records = format!("{:?}", crate::telemetry::test_support::logs());
    assert!(!spans.contains(RECAP_MARKER));
    assert!(!records.contains(RECAP_MARKER));

    // The projection and the store never receive the Recaps, so the next checks only prove that
    // this path stays free of it.
    super::projector::project_and_publish_agent_snapshot(&store, &service.snapshots, 500)
        .await
        .unwrap();
    let stored = format!("{:?}", store.load_agent_snapshot().await.unwrap());
    assert!(!stored.contains(RECAP_MARKER));
    let mut files = vec![directory.clone()];
    while let Some(path) = files.pop() {
        let Ok(entries) = std::fs::read_dir(&path) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                files.push(path);
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            assert!(
                !bytes
                    .windows(RECAP_MARKER.len())
                    .any(|w| w == RECAP_MARKER.as_bytes()),
                "{path:?}"
            );
        }
    }
    let _ = std::fs::remove_dir_all(&directory);
}

fn host_agent(state: ObservedAgentState) -> RexHostAgent {
    RexHostAgent {
        host: "omarchy".into(),
        host_pane_id: "rex:omarchy/block:b1".into(),
        pid: 4_200,
        first_seen_ms: 7_000,
        state: HostAgentState::Observed(state),
        current_step: Some("Edit files".into()),
        provider_id: "claude".into(),
        provider_display_name: "Claude".into(),
        session_name: "harold".into(),
        window_index: 2,
        pane_index: 3,
        window_name: String::new(),
        working_directory: "/home/u/harold".into(),
    }
}

fn reached(state: ObservedAgentState) -> HostReport {
    HostReport::Reached {
        agents: vec![host_agent(state)],
        host_id: Some("host-o".into()),
        icon: "server.rack".into(),
    }
}

#[tokio::test]
async fn a_host_agent_is_in_the_watch_stream_and_not_in_the_snapshot_events_or_panes() {
    let directory = std::env::temp_dir().join(format!("harold-host-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let store = Arc::new(store::HaroldStore::open(&directory).await.unwrap());
    let (mut service, _shutdown, _task) =
        test_service(Arc::clone(&store), populated_snapshot(1, None));
    service.host_agents = HostAgents::new(vec!["omarchy".into()]);
    service
        .host_agents
        .record("omarchy", reached(ObservedAgentState::Working), 9_000);
    let mut stream = service
        .watch_agent_states(Request::new(WatchAgentStatesRequest {}))
        .await
        .unwrap()
        .into_inner();
    let item = stream.next().await.expect("an item").unwrap();
    assert_eq!(item.panes.len(), 2);
    assert_eq!(item.panes[0].pane_id, STEP_PANE);
    assert_eq!(item.panes[0].host, "");
    let host = &item.panes[1];
    assert_eq!(host.pane_id, "rex:omarchy/block:b1");
    assert_eq!(host.host, "omarchy");
    assert_eq!(host.host_icon, "server.rack");
    assert_eq!(host.tmux_target, "harold");
    assert_eq!(host.session_name, "harold");
    assert_eq!(host.working_directory, "");
    assert_eq!((host.window_index, host.pane_index), (2, 3));
    assert_eq!((host.pane_pid, host.agent_pid), (4_200, 4_200));
    assert_eq!(host.agent_started_at_ms, 7_000);
    assert_eq!(host.last_transition_at_ms, 7_000);
    assert_eq!(host.provider_id, "claude");
    assert_eq!(host.provider_display_name, "Claude");
    assert_eq!(host.state, i32::from(AgentState::Working));
    assert_eq!(host.current_step.as_deref(), Some("Edit files"));
    assert_eq!(host.work_summary, None);
    assert_eq!((host.recap.as_deref(), host.recap_at_ms), (None, 0));

    // A change on the host sends a new item.
    service
        .host_agents
        .record("omarchy", reached(ObservedAgentState::Idle), 9_500);
    let item = stream.next().await.expect("a second item").unwrap();
    assert_eq!(item.panes[1].state, i32::from(AgentState::Idle));
    assert_eq!(item.panes[1].last_transition_at_ms, 9_500);

    // Memory only: not in AgentSnapshot, the event log or agent_panes.
    let is_host = |pane_id: &str| pane_id.starts_with("rex:");
    assert!(
        !service
            .snapshots
            .current()
            .panes
            .iter()
            .any(|pane| is_host(&pane.pane.incarnation.pane_id))
    );
    let stored = store.load_agent_snapshot().await.unwrap();
    assert!(
        !stored
            .panes
            .iter()
            .any(|pane| is_host(&pane.pane.incarnation.pane_id))
    );
    let events = store
        .stream()
        .load_after_version(EventVersion::start(), 1_000)
        .await
        .unwrap();
    assert!(!format!("{events:?}").contains("rex:omarchy"));
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn an_unreachable_host_shows_its_last_agents_as_unknown() {
    let hosts = HostAgents::new(vec!["omarchy".into()]);
    hosts.record("omarchy", reached(ObservedAgentState::Working), 9_000);
    hosts.record(
        "omarchy",
        HostReport::Unreached {
            reason: super::agent::rex_host_runner::Unreached::Timeout,
            host_id: None,
        },
        9_900,
    );
    let mapped = map_agent_snapshot(
        populated_snapshot(1, None),
        &CurrentSteps::default(),
        &Recaps::default(),
        &hosts,
        &WindowNames::default(),
        "",
        &RoleSources::default(),
    );
    assert_eq!(mapped.panes[0].state, i32::from(AgentState::Working));
    assert_eq!(mapped.panes[1].state, i32::from(AgentState::Unknown));
    assert_eq!(mapped.panes[1].last_transition_at_ms, 9_900);
    assert_eq!(mapped.panes[1].current_step, None);
}

#[test]
fn own_rows_carry_the_mark_of_machine_name() {
    // Without a Rex host row.
    let mapped = map_agent_snapshot(
        populated_snapshot(1, None),
        &CurrentSteps::default(),
        &Recaps::default(),
        &HostAgents::default(),
        &WindowNames::default(),
        "mac",
        &RoleSources::default(),
    );
    assert_eq!(mapped.panes[0].host_icon, "mac");
    assert_eq!(mapped.panes[0].host, "");
    // With a Rex host row: the own row keeps the mark of the machine.
    let hosts = HostAgents::new(vec!["omarchy".into()]);
    hosts.record("omarchy", reached(ObservedAgentState::Working), 9_000);
    let mapped = map_agent_snapshot(
        populated_snapshot(1, None),
        &CurrentSteps::default(),
        &Recaps::default(),
        &hosts,
        &WindowNames::default(),
        "mac",
        &RoleSources::default(),
    );
    assert_eq!(mapped.panes.len(), 2);
    assert_eq!(mapped.panes[0].host_icon, "mac");
    assert_eq!(mapped.panes[0].host, "");
}

#[test]
fn own_rows_carry_no_mark_without_an_entry() {
    let mapped = map_agent_snapshot(
        populated_snapshot(1, None),
        &CurrentSteps::default(),
        &Recaps::default(),
        &HostAgents::default(),
        &WindowNames::default(),
        "",
        &RoleSources::default(),
    );
    assert_eq!(mapped.panes[0].host_icon, "");
    assert_eq!(mapped.panes[0].host, "");
}

/// The row that the daemon builds for a fixed host agent, with no clock.
fn fixed_host_agent_row() -> HostAgentRow {
    HostAgentRow {
        agent: host_agent(ObservedAgentState::Working),
        last_transition_at_ms: 7_500,
        icon: "server.rack".into(),
    }
}

#[test]
fn a_host_agent_row_has_the_agent_pid_as_its_pane_pid() {
    let row = super::map_host_agent(fixed_host_agent_row()).expect("a row for a nonzero pid");
    assert_eq!(row.agent_pid, 4_200);
    assert_ne!(row.pane_pid, 0);
    assert_eq!(row.pane_pid, row.agent_pid);
    assert!(row.agent_started_at_ms >= 0 && row.last_transition_at_ms >= 0);
    assert_eq!(row.host, "omarchy");
    assert_eq!(row.host_icon, "server.rack");
    assert_eq!(row.working_directory, "");
    assert_eq!(row.tmux_target, "harold");
    assert_eq!(row.session_name, "harold");
    assert_eq!(row.pane_id, "rex:omarchy/block:b1");
}

#[test]
fn a_host_agent_row_maps_each_state_as_a_pane_does() {
    for (observed, expected) in [
        (ObservedAgentState::Working, AgentState::Working),
        (ObservedAgentState::Blocked, AgentState::Blocked),
        (ObservedAgentState::Done, AgentState::Done),
        (ObservedAgentState::Idle, AgentState::Idle),
        (ObservedAgentState::Error, AgentState::Error),
    ] {
        let mut row = fixed_host_agent_row();
        row.agent = host_agent(observed);
        let mapped = super::map_host_agent(row).expect("a row");
        assert_eq!(mapped.state, i32::from(expected), "{observed:?}");
    }
    let mut row = fixed_host_agent_row();
    row.agent.state = HostAgentState::Unknown;
    let mapped = super::map_host_agent(row).expect("a row");
    assert_eq!(mapped.state, i32::from(AgentState::Unknown));
}

#[test]
fn an_agent_with_a_zero_pid_on_a_host_gets_no_row() {
    let mut row = fixed_host_agent_row();
    row.agent.pid = 0;
    assert!(super::map_host_agent(row).is_none());
}

/// The golden file holds the bytes of the row above. The control-tower tests decode it, so
/// they test the exact row that the daemon builds.
#[test]
fn the_host_agent_row_bytes_match_the_golden_file() {
    use prost::Message;
    let row = super::map_host_agent(fixed_host_agent_row()).expect("a row");
    let actual: String = row
        .encode_to_vec()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../harold-api/tests/fixtures/host_agent_row.hex"
    );
    let golden = std::fs::read_to_string(path).unwrap_or_default();
    assert_eq!(
        golden.trim(),
        actual,
        "The golden file is out of date. Write this one line, with a newline, to \
         harold-api/tests/fixtures/host_agent_row.hex:\n{actual}"
    );
}

// --- GetHaroldState ---

const SECRET_PAIRING_FILE: &str = "/secret/pairing-9f3a.env";
const SECRET_KEY_FILE: &str = "/secret/hyperdx-key-77c1";
const SECRET_RECIPIENT: &str = "recipient-4471@secret.example";
const SECRET_TTS_COMMAND: &str = "secret-tts-command-5d2e";

/// Settings that hold the values the answer reports and some values it must never report.
fn state_settings(with_hosts: bool) -> crate::settings::Settings {
    let dir = format!("{}/config", env!("CARGO_MANIFEST_DIR"));
    let mut loaded = crate::settings::Settings::load_defaults(&dir).expect("default settings");
    let settings = Arc::get_mut(&mut loaded).expect("one owner");
    settings.machine_name = "TestMac".to_owned();
    settings.tts.command = SECRET_TTS_COMMAND.to_owned();
    settings.imessage.recipient = Some(SECRET_RECIPIENT.to_owned());
    // The HyperDX key file path has no setting of its own. A text setting stands in for it.
    settings.telemetry.pod_registry = Some(SECRET_KEY_FILE.to_owned());
    settings.web_app = Some(crate::settings::WebAppSettings {
        listen: "127.0.0.1:4330".to_owned(),
        allowed_login: "someone".to_owned(),
        app_name: None,
        icon: None,
        require_pairing: true,
        pairing_file: Some(SECRET_PAIRING_FILE.to_owned()),
        specs_listen: None,
        collections: Vec::new(),
    });
    if with_hosts {
        settings.agent_monitor.multiplexer = "rex".to_owned();
        settings.agent_monitor.rex_hosts = vec!["omarchy".to_owned(), "plain".to_owned()];
        settings
            .agent_monitor
            .rex_host_marks
            .insert("omarchy".to_owned(), "arch".to_owned());
        settings
            .agent_monitor
            .rex_host_marks
            .insert("testmac".to_owned(), "mac".to_owned());
        settings.notify.chief_session = Some("chief".to_owned());
        settings.tts.engine = crate::settings::TtsEngine::Command;
    }
    Arc::try_unwrap(loaded).expect("one owner")
}

async fn harold_state_for(
    settings: &crate::settings::Settings,
    muted: bool,
    paused: bool,
) -> super::harold::GetHaroldStateResponse {
    let directory = TestDirectory::new();
    let store = Arc::new(store::HaroldStore::open(&directory.0).await.unwrap());
    let mut snapshot = empty_snapshot();
    snapshot.tts_muted = muted;
    snapshot.messaging_paused = paused;
    let (mut service, shutdown, task) = test_service(store, snapshot);
    service.state_view = HaroldStateView::from_settings(settings);
    let response = service
        .get_harold_state(Request::new(GetHaroldStateRequest {}))
        .await
        .expect("answer")
        .into_inner();
    drop(shutdown);
    task.abort();
    response
}

#[tokio::test]
async fn get_harold_state_returns_each_field_from_settings() {
    let settings = state_settings(true);
    let response = harold_state_for(&settings, true, false).await;

    assert_eq!(response.tts_muted, Some(true));
    assert_eq!(response.messaging_paused, Some(false));
    assert_eq!(response.release_version, HAROLD_VERSION);
    assert_eq!(response.machine_name, "TestMac");
    assert_eq!(response.multiplexer, "rex");
    let hosts: Vec<(&str, &str)> = response
        .rex_hosts
        .iter()
        .map(|host| (host.label.as_str(), host.host_icon.as_str()))
        .collect();
    assert_eq!(hosts, vec![("omarchy", "arch"), ("plain", "")]);
    assert_eq!(response.machine_host_icon, "mac");
    assert_eq!(response.tts_engine, "command");
    assert_eq!(response.chief_session, "chief");

    let snapshot = map_agent_snapshot(
        AgentSnapshotHub::new(empty_snapshot()).current(),
        &CurrentSteps::default(),
        &Recaps::default(),
        &HostAgents::default(),
        &WindowNames::default(),
        "",
        &RoleSources::default(),
    );
    assert_eq!(response.release_version, snapshot.harold_version);
}

#[tokio::test]
async fn get_harold_state_holds_no_secret() {
    use prost::Message as _;
    let settings = state_settings(true);
    let response = harold_state_for(&settings, false, false).await;
    let debug = format!("{response:?}");
    let bytes = response.encode_to_vec();
    for secret in [
        SECRET_PAIRING_FILE,
        SECRET_KEY_FILE,
        SECRET_RECIPIENT,
        SECRET_TTS_COMMAND,
    ] {
        assert!(!debug.contains(secret), "Debug text holds {secret}");
        assert!(
            !bytes.windows(secret.len()).any(|w| w == secret.as_bytes()),
            "wire bytes hold {secret}"
        );
    }
}

#[tokio::test]
async fn get_harold_state_flags_follow_the_snapshot() {
    use prost::Message as _;
    let settings = state_settings(false);
    for (muted, paused) in [(false, false), (true, false), (false, true), (true, true)] {
        let response = harold_state_for(&settings, muted, paused).await;
        assert_eq!(response.tts_muted, Some(muted));
        assert_eq!(response.messaging_paused, Some(paused));
        let bytes = response.encode_to_vec();
        // Tag 1 and tag 2 are varints: 0x08 and 0x10, each with its value.
        assert_eq!(
            bytes[..4],
            [0x08, u8::from(muted), 0x10, u8::from(paused)],
            "muted {muted}, paused {paused}"
        );
    }
}

#[tokio::test]
async fn get_harold_state_answers_closed_with_no_call_and_writes_the_false() {
    use prost::Message as _;
    let response = harold_state_for(&state_settings(false), false, false).await;
    assert_eq!(response.call_open, Some(false));
    assert_eq!(response.call_where, "");
    assert_eq!(response.call_opened_at_ms, 0);
    // Tag 10 is a varint: 0x50, with its value 0.
    assert!(
        response
            .encode_to_vec()
            .windows(2)
            .any(|pair| pair == [0x50, 0x00])
    );
}

#[test]
fn call_fields_name_the_open_call_and_its_start() {
    assert_eq!(super::call_fields(None, None), (false, "", 0));
    assert_eq!(super::call_fields(Some(1_000), None), (true, "desk", 1_000));
    assert_eq!(
        super::call_fields(None, Some(2_000)),
        (true, "phone", 2_000)
    );
    // A phone call ends a desk call, so both open is a moment between: the phone's wins.
    assert_eq!(
        super::call_fields(Some(1_000), Some(2_000)),
        (true, "phone", 2_000)
    );
}

#[tokio::test]
async fn get_harold_state_with_no_hosts_marks_or_chief() {
    let settings = state_settings(false);
    let response = harold_state_for(&settings, false, false).await;
    assert!(response.rex_hosts.is_empty());
    assert_eq!(response.machine_host_icon, "");
    assert_eq!(response.chief_session, "");
    assert_eq!(response.multiplexer, "tmux");
}

// -- the window name of a row ----------------------------------------------------------------

fn names_for_step_pane(name: &str) -> WindowNames {
    let names = WindowNames::default();
    names.replace_agents(std::collections::HashMap::from([(
        STEP_PANE.to_string(),
        name.to_string(),
    )]));
    names
}

#[test]
fn a_local_row_gets_its_window_name_from_the_map() {
    let mapped = map_agent_snapshot(
        populated_snapshot(1, None),
        &CurrentSteps::default(),
        &Recaps::default(),
        &HostAgents::default(),
        &names_for_step_pane("work"),
        "",
        &RoleSources::default(),
    );
    assert_eq!(mapped.panes[0].pane_id, STEP_PANE);
    assert_eq!(mapped.panes[0].window_name, "work");
}

#[test]
fn a_local_row_with_no_entry_in_the_map_has_an_empty_window_name() {
    let mapped = map_agent_snapshot(
        populated_snapshot(1, None),
        &CurrentSteps::default(),
        &Recaps::default(),
        &HostAgents::default(),
        &WindowNames::default(),
        "",
        &RoleSources::default(),
    );
    assert_eq!(mapped.panes[0].window_name, "");
}

#[tokio::test]
async fn a_renamed_window_sends_a_new_snapshot() {
    let names = WindowNames::default();
    let (mut receiver, _shutdown, _hub) =
        named_stream(&CurrentSteps::default(), &Recaps::default(), &names);
    assert_eq!(next_item(&mut receiver).await.panes[0].window_name, "");
    names.replace_agents(std::collections::HashMap::from([(
        STEP_PANE.to_string(),
        "work".to_string(),
    )]));
    assert_eq!(next_item(&mut receiver).await.panes[0].window_name, "work");
    names.replace_agents(std::collections::HashMap::from([(
        STEP_PANE.to_string(),
        "review".to_string(),
    )]));
    assert_eq!(
        next_item(&mut receiver).await.panes[0].window_name,
        "review"
    );
}

#[test]
fn a_host_agent_row_gets_its_window_name() {
    let mut row = fixed_host_agent_row();
    row.agent.window_name = "linux-satellite-2b".into();
    let mapped = super::map_host_agent(row).expect("a row");
    assert_eq!(mapped.window_name, "linux-satellite-2b");
}

// --- roles ---

/// A temporary product repo with an `agent-panes` file and a registry that names it.
/// The folder is removed when the value drops.
struct RoleFixture {
    root: std::path::PathBuf,
    sources: RoleSources,
}

impl RoleFixture {
    fn new(agent_panes: &str, with_registry: bool) -> Self {
        let root = std::env::temp_dir().join(format!("harold-roles-{}", uuid::Uuid::new_v4()));
        let pod_folder = root.join("alpha/.superpowers/pod");
        std::fs::create_dir_all(&pod_folder).expect("create the pod folder");
        std::fs::write(pod_folder.join("agent-panes"), agent_panes).expect("write agent-panes");
        let registry = root.join("products.toml");
        std::fs::write(
            &registry,
            format!(
                "[products.alpha]\nrepo = \"{}\"\nbase = \"main\"\nslots = 1\n",
                root.join("alpha").display()
            ),
        )
        .expect("write the registry");
        let sources = RoleSources {
            chief_folder: Some("/work/chief".into()),
            chief_session: None,
            registry: with_registry.then_some(registry),
        };
        Self { root, sources }
    }
}

impl Drop for RoleFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A snapshot of panes as `(pane id, folder)` pairs.
fn snapshot_of_panes(panes: &[(&str, &str)]) -> AgentSnapshot {
    let mut snapshot = populated_snapshot(1, None);
    let template = snapshot.panes[0].clone();
    snapshot.panes = panes
        .iter()
        .map(|(pane_id, folder)| {
            let mut projection = template.clone();
            projection.pane.incarnation.pane_id = (*pane_id).into();
            projection.pane.working_directory = (*folder).into();
            projection
        })
        .collect();
    snapshot
}

/// The `(role, pod_product)` of each pane, by pane id.
fn roles_of(
    snapshot: AgentSnapshot,
    sources: &RoleSources,
) -> std::collections::BTreeMap<String, (String, String)> {
    map_agent_snapshot(
        snapshot,
        &CurrentSteps::default(),
        &Recaps::default(),
        &HostAgents::default(),
        &WindowNames::default(),
        "",
        sources,
    )
    .panes
    .into_iter()
    .map(|pane| (pane.pane_id, (pane.role, pane.pod_product)))
    .collect()
}

fn role(role: &str, product: &str) -> (String, String) {
    (role.to_owned(), product.to_owned())
}

#[test]
fn map_agent_snapshot_marks_chief_coordinator_and_pnem() {
    let fixture = RoleFixture::new("%1 x\n%2\n", true);
    let snapshot = snapshot_of_panes(&[
        ("%1", "/work/alpha"),
        ("%2", "/work/alpha"),
        ("%9", "/work/chief"),
    ]);

    let roles = roles_of(snapshot, &fixture.sources);

    assert_eq!(roles["%9"], role("chief", ""));
    assert_eq!(roles["%1"], role("coordinator", "alpha"));
    assert_eq!(roles["%2"], role("pnem", "alpha"));
}

#[test]
fn an_ordinary_agent_pane_has_no_role() {
    let fixture = RoleFixture::new("%1\n%2\n", true);
    let snapshot = snapshot_of_panes(&[("%5", "/work/other"), ("%9", "/work/chief")]);

    let roles = roles_of(snapshot, &fixture.sources);

    assert_eq!(roles["%5"], role("", ""));
}

#[test]
fn no_registry_gives_the_chief_role_only() {
    let fixture = RoleFixture::new("%1\n%2\n", false);
    let snapshot = snapshot_of_panes(&[
        ("%1", "/work/alpha"),
        ("%2", "/work/alpha"),
        ("%9", "/work/chief"),
    ]);

    let roles = roles_of(snapshot, &fixture.sources);

    assert_eq!(roles["%9"], role("chief", ""));
    assert_eq!(roles["%1"], role("", ""));
    assert_eq!(roles["%2"], role("", ""));
}

#[test]
fn a_registry_that_cannot_be_read_gives_no_pod_roles() {
    let mut fixture = RoleFixture::new("%1\n%2\n", true);
    fixture.sources.registry = Some(fixture.root.join("missing.toml"));
    let snapshot = snapshot_of_panes(&[("%1", "/work/alpha"), ("%9", "/work/chief")]);

    let roles = roles_of(snapshot, &fixture.sources);

    assert_eq!(roles["%1"], role("", ""));
    assert_eq!(roles["%9"], role("chief", ""));
}

#[test]
fn a_row_of_a_rex_host_has_no_role() {
    let mapped = super::map_host_agent(fixed_host_agent_row()).expect("a row");
    assert_eq!(mapped.role, "");
    assert_eq!(mapped.pod_product, "");
}

#[test]
fn a_dead_pod_pane_has_no_row_and_the_live_one_keeps_its_role() {
    let fixture = RoleFixture::new("%1\n%2\n", true);
    // `%1` is named in the file but not in the snapshot.
    let snapshot = snapshot_of_panes(&[("%2", "/work/alpha")]);

    let roles = roles_of(snapshot, &fixture.sources);

    assert_eq!(roles.len(), 1);
    assert_eq!(roles["%2"], role("pnem", "alpha"));
}

#[test]
fn a_pane_named_in_two_products_takes_the_role_of_the_first_product() {
    let fixture = RoleFixture::new("%1\n%2\n", true);
    let beta = fixture.root.join("beta/.superpowers/pod");
    std::fs::create_dir_all(&beta).expect("create the second pod folder");
    std::fs::write(beta.join("agent-panes"), "%2\n%1\n").expect("write agent-panes");
    let registry = fixture.sources.registry.clone().expect("a registry");
    let mut text = std::fs::read_to_string(&registry).expect("read the registry");
    text.push_str(&format!(
        "\n[products.beta]\nrepo = \"{}\"\nbase = \"main\"\nslots = 1\n",
        fixture.root.join("beta").display()
    ));
    std::fs::write(&registry, text).expect("write the registry");
    let snapshot = snapshot_of_panes(&[("%1", "/work/a"), ("%2", "/work/b")]);

    let roles = roles_of(snapshot, &fixture.sources);

    assert_eq!(roles["%1"], role("coordinator", "alpha"));
    assert_eq!(roles["%2"], role("pnem", "alpha"));
}

async fn get(service: &HaroldService) -> Vec<String> {
    service
        .get_remote_order(Request::new(GetRemoteOrderRequest {}))
        .await
        .unwrap()
        .into_inner()
        .products
}

#[tokio::test]
async fn the_service_calls_round_trip() {
    let directory = TestDirectory::new();
    let registry = directory.0.join("products.toml");
    let mut text = String::new();
    for name in ["alpha", "beta", "gamma"] {
        text.push_str(&format!(
            "[products.{name}]\nrepo = \"/work/{name}\"\nbase = \"main\"\nslots = 1\n\n"
        ));
    }
    std::fs::write(&registry, text).unwrap();
    let store = Arc::new(store::HaroldStore::open(&directory.0).await.unwrap());
    let (mut service, _shutdown, _task) = test_service(store, populated_snapshot(1, None));
    service.state_view.remote_order = Some(crate::remote_order::RemoteOrder::new(&directory.0));
    service.state_view.roles.registry = Some(registry);
    assert_eq!(get(&service).await, ["alpha", "beta", "gamma"]);
    let products = ["gamma", "gone", "alpha"].map(String::from).to_vec();
    service
        .set_remote_order(Request::new(SetRemoteOrderRequest { products }))
        .await
        .unwrap();
    assert_eq!(get(&service).await, ["gamma", "alpha", "beta"]);
}

#[tokio::test]
async fn the_remote_order_is_empty_with_no_registry() {
    let directory = TestDirectory::new();
    let store = Arc::new(store::HaroldStore::open(&directory.0).await.unwrap());
    let (mut service, _shutdown, _task) = test_service(store, populated_snapshot(1, None));
    service.state_view.remote_order = Some(crate::remote_order::RemoteOrder::new(&directory.0));
    let reply = service
        .get_remote_order(Request::new(GetRemoteOrderRequest {}))
        .await
        .unwrap();
    assert!(reply.into_inner().products.is_empty());
}

#[tokio::test]
async fn a_failed_write_gives_an_internal_status() {
    let directory = TestDirectory::new();
    let store = Arc::new(store::HaroldStore::open(&directory.0).await.unwrap());
    let (mut service, _shutdown, _task) = test_service(store, populated_snapshot(1, None));
    service.state_view.remote_order = Some(crate::remote_order::RemoteOrder::new(
        &directory.0.join("no-such-folder"),
    ));
    let error = service
        .set_remote_order(Request::new(SetRemoteOrderRequest::default()))
        .await
        .unwrap_err();
    assert_eq!(error.code(), tonic::Code::Internal);
    assert_eq!(error.message(), "remote order write failed");
}

/// Runs `forward_agent_snapshots` with a waiting channel the test sends on.
fn waiting_stream() -> (
    tokio::sync::mpsc::Receiver<Result<super::harold::AgentStateSnapshot, tonic::Status>>,
    watch::Sender<()>,
    watch::Sender<WaitingView>,
) {
    let hub = AgentSnapshotHub::new(populated_snapshot(1, None));
    let (waiting, waiting_rx) = watch::channel(WaitingView::default());
    let (sender, receiver) = tokio::sync::mpsc::channel(8);
    let (shutdown, shutdown_rx) = watch::channel(());
    tokio::spawn(async move {
        super::forward_agent_snapshots(
            hub.subscribe(),
            CurrentSteps::default(),
            Recaps::default(),
            HostAgents::default(),
            WindowNames::default(),
            String::new(),
            RoleSources::default(),
            watch::channel(crate::tools::WatchCounts::default()).1,
            waiting_rx,
            sender,
            shutdown_rx,
        )
        .await;
        drop(hub);
    });
    (receiver, shutdown, waiting)
}

fn waiting_view(panes: &[(&str, i64)], focus_known: bool) -> WaitingView {
    WaitingView {
        since_ms: panes
            .iter()
            .map(|(pane_id, since)| ((*pane_id).to_owned(), *since))
            .collect(),
        focus_known,
    }
}

#[test]
fn snapshot_carries_waiting_since_ms_and_focus_known() {
    let mapped = map_agent_snapshot(
        populated_snapshot(1, None),
        &CurrentSteps::default(),
        &Recaps::default(),
        &HostAgents::default(),
        &WindowNames::default(),
        "",
        &RoleSources::default(),
    );
    assert_eq!(mapped.focus_known, None);

    let served = with_waiting(
        mapped.clone(),
        &waiting_view(&[(STEP_PANE, 12_345), ("%99", 7)], true),
    );

    assert!(served.panes[0].waiting);
    assert_eq!(served.panes[0].waiting_since_ms, 12_345);
    assert_eq!(served.focus_known, Some(true));
    assert_eq!(
        served.panes.len(),
        1,
        "a waiting pane id with no pane adds no row"
    );

    let not_waiting = with_waiting(mapped, &waiting_view(&[], true));
    assert!(!not_waiting.panes[0].waiting);
    assert_eq!(not_waiting.panes[0].waiting_since_ms, 0);
}

#[test]
fn unknown_focus_serves_focus_known_false() {
    let mapped = map_agent_snapshot(
        populated_snapshot(1, None),
        &CurrentSteps::default(),
        &Recaps::default(),
        &HostAgents::default(),
        &WindowNames::default(),
        "",
        &RoleSources::default(),
    );
    let served = with_waiting(mapped, &waiting_view(&[], false));
    assert_eq!(served.focus_known, Some(false));
}

#[tokio::test]
async fn a_waiting_change_sends_a_snapshot_at_the_same_event_version() {
    let (mut receiver, _shutdown, waiting) = waiting_stream();
    let first = next_item(&mut receiver).await;
    assert!(!first.panes[0].waiting);
    assert_eq!(first.focus_known, Some(false));
    let version = first.through_event_version;

    waiting.send_replace(waiting_view(&[(STEP_PANE, 9_000)], true));
    let second = next_item(&mut receiver).await;
    assert!(second.panes[0].waiting);
    assert_eq!(second.panes[0].waiting_since_ms, 9_000);
    assert_eq!(second.focus_known, Some(true));
    assert_eq!(
        second.through_event_version, version,
        "a waiting change does not raise the event version"
    );
    let mut unchanged = second.panes[0].clone();
    unchanged.waiting = false;
    unchanged.waiting_since_ms = 0;
    assert_eq!(unchanged, first.panes[0], "the agent state stays the same");

    waiting.send_replace(waiting_view(&[], true));
    let third = next_item(&mut receiver).await;
    assert!(!third.panes[0].waiting);
    assert_eq!(third.through_event_version, version);
    no_item(&mut receiver).await;
}

/// A focus port that answers `tmux list-clients` with the line the test sets.
struct ScriptedFocus(std::sync::Mutex<Result<String, String>>);

impl ScriptedFocus {
    fn focused_on(pane_id: &str) -> Arc<Self> {
        Arc::new(Self(std::sync::Mutex::new(Ok(Self::line(pane_id)))))
    }

    fn line(pane_id: &str) -> String {
        format!("100\tattached,focused\t{pane_id}\n")
    }

    fn set(&self, answer: Result<String, String>) {
        *self.0.lock().expect("focus lock") = answer;
    }
}

impl FocusPort for ScriptedFocus {
    fn run(&self, program: &str, _args: &[&str]) -> Result<String, String> {
        assert_eq!(program, "tmux", "a tmux pane id makes the reader ask tmux");
        self.0.lock().expect("focus lock").clone()
    }
}

/// The fake clock of the waiting task.
const WAITING_CLOCK_MS: i64 = 42_000;

/// Runs the waiting task and the stream over one hub. The task reads `port` every 20 ms.
fn waiting_task_stream(
    port: Arc<ScriptedFocus>,
) -> (
    tokio::sync::mpsc::Receiver<Result<super::harold::AgentStateSnapshot, tonic::Status>>,
    watch::Sender<()>,
    AgentSnapshotHub,
) {
    waiting_task_stream_with(port, HostAgents::default())
}

/// Like `waiting_task_stream`, with the Rex host agents the task and the stream read.
fn waiting_task_stream_with(
    port: Arc<ScriptedFocus>,
    hosts: HostAgents,
) -> (
    tokio::sync::mpsc::Receiver<Result<super::harold::AgentStateSnapshot, tonic::Status>>,
    watch::Sender<()>,
    AgentSnapshotHub,
) {
    // No caller sends `AgentSeen` here: the closed channel must not stop the task.
    let (receiver, shutdown, hub, _seen) =
        waiting_task_stream_full(port, hosts, RoleSources::default(), || WAITING_CLOCK_MS);
    (receiver, shutdown, hub)
}

/// The stream items that a test reads.
type StreamItems =
    tokio::sync::mpsc::Receiver<Result<super::harold::AgentStateSnapshot, tonic::Status>>;

/// Like `waiting_task_stream_with`, with the roles and the clock the task uses. It also
/// returns the sender of the `AgentSeen` pane ids that the task reads.
fn waiting_task_stream_full(
    port: Arc<ScriptedFocus>,
    hosts: HostAgents,
    roles: RoleSources,
    clock: fn() -> i64,
) -> (
    StreamItems,
    watch::Sender<()>,
    AgentSnapshotHub,
    tokio::sync::mpsc::Sender<String>,
) {
    let hub = AgentSnapshotHub::new(populated_snapshot(1, None));
    let (waiting, waiting_rx) = watch::channel(WaitingView::default());
    let (sender, receiver) = tokio::sync::mpsc::channel(8);
    let (shutdown, shutdown_rx) = watch::channel(());
    let (seen, seen_rx) = tokio::sync::mpsc::channel(8);
    spawn_waiting_task(
        hub.subscribe(),
        hosts.clone(),
        port,
        roles.clone(),
        std::time::Duration::from_millis(20),
        clock,
        waiting,
        seen_rx,
        shutdown_rx.clone(),
    );
    tokio::spawn(super::forward_agent_snapshots(
        hub.subscribe(),
        CurrentSteps::default(),
        Recaps::default(),
        hosts,
        WindowNames::default(),
        String::new(),
        roles,
        watch::channel(crate::tools::WatchCounts::default()).1,
        waiting_rx,
        sender,
        shutdown_rx,
    ));
    (receiver, shutdown, hub, seen)
}

/// The next stream item that `wanted` accepts.
async fn item_where(
    receiver: &mut tokio::sync::mpsc::Receiver<
        Result<super::harold::AgentStateSnapshot, tonic::Status>,
    >,
    wanted: impl Fn(&super::harold::AgentStateSnapshot) -> bool,
) -> super::harold::AgentStateSnapshot {
    loop {
        let item = next_item(receiver).await;
        if wanted(&item) {
            return item;
        }
    }
}

fn snapshot_in_state(revision: i64, state: EffectiveAgentState) -> AgentSnapshot {
    let mut snapshot = populated_snapshot(revision, None);
    snapshot.panes[0].effective_state = state;
    snapshot
}

#[tokio::test]
async fn the_waiting_task_serves_a_pane_that_rests_off_focus_until_the_focus_reaches_it() {
    let port = ScriptedFocus::focused_on("%1");
    let (mut receiver, _shutdown, hub) = waiting_task_stream(Arc::clone(&port));
    item_where(&mut receiver, |item| item.focus_known == Some(true)).await;

    hub.publish_committed(snapshot_in_state(2, EffectiveAgentState::Done));
    let waiting = item_where(&mut receiver, |item| item.panes[0].waiting).await;
    assert_eq!(waiting.panes[0].waiting_since_ms, WAITING_CLOCK_MS);
    assert_eq!(waiting.focus_known, Some(true));
    assert_eq!(waiting.through_event_version, 2);

    port.set(Ok(ScriptedFocus::line(STEP_PANE)));
    let seen = item_where(&mut receiver, |item| !item.panes[0].waiting).await;
    assert_eq!(seen.panes[0].waiting_since_ms, 0);
    assert_eq!(
        seen.through_event_version, 2,
        "a waiting change does not raise the event version"
    );
}

#[tokio::test]
async fn the_waiting_task_serves_focus_known_false_when_the_focus_cannot_be_read() {
    let port = ScriptedFocus::focused_on("%1");
    let (mut receiver, _shutdown, _hub) = waiting_task_stream(Arc::clone(&port));
    item_where(&mut receiver, |item| item.focus_known == Some(true)).await;

    port.set(Err("tmux: no server".into()));
    let unknown = item_where(&mut receiver, |item| item.focus_known == Some(false)).await;
    assert!(!unknown.panes[0].waiting);
}

#[tokio::test]
async fn the_waiting_task_stops_at_shutdown() {
    let hub = AgentSnapshotHub::new(populated_snapshot(1, None));
    let (waiting, _waiting_rx) = watch::channel(WaitingView::default());
    let (shutdown, shutdown_rx) = watch::channel(());
    let task = spawn_waiting_task(
        hub.subscribe(),
        HostAgents::default(),
        ScriptedFocus::focused_on("%1"),
        RoleSources::default(),
        std::time::Duration::from_millis(20),
        || WAITING_CLOCK_MS,
        waiting,
        tokio::sync::mpsc::channel(1).1,
        shutdown_rx,
    );
    drop(shutdown);
    tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .expect("the task stops in time")
        .expect("the task does not panic");
}

#[test]
fn waiting_counts_give_one_row_for_each_role_with_zero_when_none_waits() {
    let rows: Vec<TrackedRow> = ["%8", "%9", "%10"]
        .into_iter()
        .map(|pane_id| TrackedRow {
            pane_id: pane_id.to_owned(),
            agent_pid: STEP_PID,
            agent_started_at_ms: 8_000,
            state: EffectiveAgentState::Done,
        })
        .collect();
    let role_and_product = |pane_id: &str| match pane_id {
        "%8" | "%9" => ("coordinator".to_owned(), "alpha".to_owned()),
        _ => (String::new(), String::new()),
    };
    let row = |role: &str, pod_product: &str, count| crate::telemetry::WaitingCount {
        role: role.to_owned(),
        pod_product: pod_product.to_owned(),
        count,
    };

    let none = waiting_counts(&rows, &waiting_view(&[], true), &role_and_product);
    assert_eq!(none, vec![row("", "", 0), row("coordinator", "alpha", 0)]);

    let view = waiting_view(&[("%8", 1), ("%9", 2), ("%gone", 3)], true);
    let some = waiting_counts(&rows, &view, &role_and_product);
    assert_eq!(some, vec![row("", "", 0), row("coordinator", "alpha", 2)]);
}

#[test]
fn the_first_pane_of_this_machine_decides_rex_or_tmux_for_the_focus() {
    let with_panes = |pane_ids: &[&str]| {
        let mut snapshot = populated_snapshot(1, None);
        let template = snapshot.panes[0].clone();
        snapshot.panes = pane_ids
            .iter()
            .map(|pane_id| {
                let mut pane = template.clone();
                pane.pane.incarnation.pane_id = (*pane_id).to_owned();
                pane
            })
            .collect();
        snapshot
    };
    let in_rex = |pane_ids: &[&str]| first_local_pane_in_rex(&with_panes(pane_ids));
    assert_eq!(in_rex(&["rex:host/block:1", "block:abc", "%1"]), Some(true));
    assert_eq!(
        in_rex(&["rex:host/block:1", "%1", "block:abc"]),
        Some(false)
    );
    assert_eq!(in_rex(&["rex:host/block:1"]), None);
    assert_eq!(in_rex(&[]), None);
}

#[tokio::test]
async fn an_agent_of_a_rex_host_waits_until_it_works_again() {
    const HOST_PANE: &str = "rex:omarchy/block:b1";
    let hosts = HostAgents::new(vec!["omarchy".into()]);
    hosts.record("omarchy", reached(ObservedAgentState::Working), 9_000);
    let port = ScriptedFocus::focused_on("%1");
    let (mut receiver, _shutdown, _hub) =
        waiting_task_stream_with(Arc::clone(&port), hosts.clone());
    let host_row = |item: &super::harold::AgentStateSnapshot| {
        item.panes
            .iter()
            .find(|pane| pane.pane_id == HOST_PANE)
            .cloned()
            .expect("the host row is served")
    };
    item_where(&mut receiver, |item| item.focus_known == Some(true)).await;

    hosts.record("omarchy", reached(ObservedAgentState::Done), 9_500);
    let waiting = item_where(&mut receiver, |item| host_row(item).waiting).await;
    assert_eq!(host_row(&waiting).waiting_since_ms, WAITING_CLOCK_MS);

    // A focus on the host pane id does not clear it: this machine cannot see that pane.
    port.set(Ok(ScriptedFocus::line(HOST_PANE)));
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    while let Ok(item) = receiver.try_recv() {
        let item = item.expect("an item, not an error");
        assert!(
            host_row(&item).waiting,
            "a focus on a host pane does not clear it"
        );
    }

    hosts.record("omarchy", reached(ObservedAgentState::Working), 10_000);
    let working = item_where(&mut receiver, |item| !host_row(item).waiting).await;
    assert_eq!(host_row(&working).waiting_since_ms, 0);
    assert_eq!(host_row(&working).state, i32::from(AgentState::Working));
}

/// The waiting task, the stream, and a service whose `AgentSeen` calls reach the task.
struct SeenRig {
    receiver: tokio::sync::mpsc::Receiver<Result<super::harold::AgentStateSnapshot, tonic::Status>>,
    hub: AgentSnapshotHub,
    service: HaroldService,
    _shutdown: watch::Sender<()>,
    _service_shutdown: watch::Sender<()>,
    _monitor: tokio::task::JoinHandle<()>,
    _directory: TestDirectory,
}

impl SeenRig {
    async fn new(port: Arc<ScriptedFocus>, hosts: HostAgents, clock: fn() -> i64) -> Self {
        // The chief role makes the `role` field of the lines and the sample not empty.
        let roles = RoleSources {
            chief_folder: Some("/work/harold".into()),
            chief_session: Some("harold".into()),
            registry: None,
        };
        let (receiver, shutdown, hub, seen) = waiting_task_stream_full(port, hosts, roles, clock);
        let directory = TestDirectory::new();
        let store = Arc::new(store::HaroldStore::open(&directory.0).await.unwrap());
        let (mut service, service_shutdown, monitor) =
            test_service(store, populated_snapshot(1, None));
        service.seen_calls = seen;
        Self {
            receiver,
            hub,
            service,
            _shutdown: shutdown,
            _service_shutdown: service_shutdown,
            _monitor: monitor,
            _directory: directory,
        }
    }

    /// Calls `AgentSeen` for `pane_id`. The call never fails.
    async fn seen(&self, pane_id: &str) {
        let answer = self
            .service
            .agent_seen(Request::new(AgentSeenRequest {
                pane_id: pane_id.to_owned(),
            }))
            .await
            .expect("AgentSeen never fails the caller");
        assert_eq!(answer.into_inner(), AgentSeenResponse {});
    }

    /// The next stream item whose row `pane_id` is waiting (or not, by `waiting`).
    async fn row_waiting(&mut self, pane_id: &str, waiting: bool) -> super::harold::AgentPaneState {
        let item = item_where(&mut self.receiver, |item| {
            item.panes
                .iter()
                .any(|pane| pane.pane_id == pane_id && pane.waiting == waiting)
        })
        .await;
        item.panes
            .into_iter()
            .find(|pane| pane.pane_id == pane_id)
            .expect("the row")
    }

    /// Makes the agent of `STEP_PANE` finish at `revision`, and waits until it waits.
    async fn finish(&mut self, revision: i64) {
        self.hub
            .publish_committed(snapshot_in_state(revision, EffectiveAgentState::Working));
        // The task reads only the newest snapshot: let it read the working one first.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        self.hub
            .publish_committed(snapshot_in_state(revision + 1, EffectiveAgentState::Done));
        self.row_waiting(STEP_PANE, true).await;
    }
}

/// The captured lines that contain `message`.
fn lines_with(logs: &crate::outbound::fake::CapturedLogs, message: &str) -> Vec<String> {
    logs.text()
        .lines()
        .filter(|line| line.contains(message))
        .map(str::to_owned)
        .collect()
}

#[tokio::test]
async fn agent_seen_clears_a_waiting_pane_and_logs_and_records() {
    static CLOCK: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(42_000);
    let logs = crate::outbound::fake::CapturedLogs::start();
    crate::telemetry::take_noted_waiting_durations();
    let mut rig = SeenRig::new(
        ScriptedFocus::focused_on("%1"),
        HostAgents::default(),
        || CLOCK.load(std::sync::atomic::Ordering::SeqCst),
    )
    .await;
    item_where(&mut rig.receiver, |item| item.focus_known == Some(true)).await;

    rig.hub
        .publish_committed(snapshot_in_state(2, EffectiveAgentState::Done));
    let waiting = rig.row_waiting(STEP_PANE, true).await;
    assert_eq!(waiting.waiting_since_ms, 42_000);
    let began = lines_with(&logs, "agent waiting");
    assert_eq!(began.len(), 1, "{began:?}");
    assert!(began[0].contains("pane_id=%8"), "{}", began[0]);
    assert!(began[0].contains("role=chief"), "{}", began[0]);

    CLOCK.store(45_500, std::sync::atomic::Ordering::SeqCst);
    rig.seen(STEP_PANE).await;
    let seen = rig.row_waiting(STEP_PANE, false).await;
    assert_eq!(seen.waiting_since_ms, 0);
    assert_eq!(seen.state, i32::from(AgentState::Done), "the state stays");

    let lines = lines_with(&logs, "agent seen");
    assert_eq!(lines.len(), 1, "{lines:?}");
    for field in ["pane_id=%8", "role=chief", "waited_s=3.5"] {
        assert!(lines[0].contains(field), "{field}: {}", lines[0]);
    }
    assert_eq!(
        crate::telemetry::take_noted_waiting_durations(),
        vec![("chief".to_owned(), 3.5)]
    );
    // The exported log record carries the same fields.
    let exported = crate::telemetry::test_support::logs()
        .into_iter()
        .any(|log| {
            let record = &log.record;
            let attribute = |key: &str| {
                record
                    .attributes_iter()
                    .find(|(name, _)| name.as_str() == key)
                    .map(|(_, value)| format!("{value:?}"))
            };
            format!("{:?}", record.body()).contains("agent seen")
                && attribute("pane_id").is_some_and(|value| value.contains("%8"))
                && attribute("role").is_some_and(|value| value.contains("chief"))
                && attribute("waited_s").is_some_and(|value| value.contains("3.5"))
        });
    assert!(exported, "an exported `agent seen` record with its fields");
}

#[tokio::test]
async fn agent_seen_for_a_pane_that_does_not_wait_writes_no_line_and_no_sample() {
    static CLOCK: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(42_000);
    let logs = crate::outbound::fake::CapturedLogs::start();
    crate::telemetry::take_noted_waiting_durations();
    let mut rig = SeenRig::new(
        ScriptedFocus::focused_on("%1"),
        HostAgents::default(),
        || CLOCK.load(std::sync::atomic::Ordering::SeqCst),
    )
    .await;
    item_where(&mut rig.receiver, |item| item.focus_known == Some(true)).await;

    // The agent works: it does not wait. Two calls in a row change nothing.
    rig.seen(STEP_PANE).await;
    rig.seen(STEP_PANE).await;
    rig.finish(2).await;
    CLOCK.store(44_000, std::sync::atomic::Ordering::SeqCst);
    // The calls go through one queue in order: when this one clears the mark, the task
    // took the calls before it.
    rig.seen(STEP_PANE).await;
    rig.row_waiting(STEP_PANE, false).await;
    assert_eq!(lines_with(&logs, "agent seen").len(), 1);
    assert_eq!(
        crate::telemetry::take_noted_waiting_durations(),
        vec![("chief".to_owned(), 2.0)]
    );

    // A second call after the mark is gone changes nothing.
    rig.seen(STEP_PANE).await;
    rig.finish(4).await;
    CLOCK.store(45_000, std::sync::atomic::Ordering::SeqCst);
    rig.seen(STEP_PANE).await;
    rig.row_waiting(STEP_PANE, false).await;
    let lines = lines_with(&logs, "agent seen");
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(lines[1].contains("waited_s=1"), "{}", lines[1]);
    assert_eq!(
        crate::telemetry::take_noted_waiting_durations(),
        vec![("chief".to_owned(), 1.0)]
    );
}

#[tokio::test]
async fn agent_seen_for_an_unknown_pane_writes_nothing() {
    static CLOCK: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(42_000);
    let logs = crate::outbound::fake::CapturedLogs::start();
    crate::telemetry::take_noted_waiting_durations();
    let mut rig = SeenRig::new(
        ScriptedFocus::focused_on("%1"),
        HostAgents::default(),
        || CLOCK.load(std::sync::atomic::Ordering::SeqCst),
    )
    .await;
    item_where(&mut rig.receiver, |item| item.focus_known == Some(true)).await;
    rig.finish(2).await;

    for pane_id in ["", "%404", "rex:nohost/block:x"] {
        rig.seen(pane_id).await;
    }
    CLOCK.store(43_000, std::sync::atomic::Ordering::SeqCst);
    rig.seen(STEP_PANE).await;
    rig.row_waiting(STEP_PANE, false).await;
    let lines = lines_with(&logs, "agent seen");
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("pane_id=%8"), "{}", lines[0]);
    assert_eq!(
        crate::telemetry::take_noted_waiting_durations(),
        vec![("chief".to_owned(), 1.0)]
    );
}

#[tokio::test]
async fn agent_seen_works_with_an_unknown_focus() {
    static CLOCK: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(42_000);
    let logs = crate::outbound::fake::CapturedLogs::start();
    crate::telemetry::take_noted_waiting_durations();
    let port = ScriptedFocus::focused_on("%1");
    port.set(Err("tmux: no server".into()));
    let mut rig = SeenRig::new(port, HostAgents::default(), || {
        CLOCK.load(std::sync::atomic::Ordering::SeqCst)
    })
    .await;
    item_where(&mut rig.receiver, |item| item.focus_known == Some(false)).await;
    let started = std::time::Instant::now();
    while lines_with(&logs, "focus unknown").is_empty() {
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "no focus unknown line: {}",
            logs.text()
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let unknown = lines_with(&logs, "focus unknown");
    assert_eq!(unknown.len(), 1, "{unknown:?}");
    assert!(unknown[0].contains("reason="), "{}", unknown[0]);

    // With an unknown focus, an agent that finishes waits, and only a seen call clears it.
    rig.finish(2).await;
    CLOCK.store(52_000, std::sync::atomic::Ordering::SeqCst);
    rig.seen(STEP_PANE).await;
    let seen = rig.row_waiting(STEP_PANE, false).await;
    assert_eq!(seen.waiting_since_ms, 0);
    let lines = lines_with(&logs, "agent seen");
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("waited_s=10"), "{}", lines[0]);
    assert_eq!(
        crate::telemetry::take_noted_waiting_durations(),
        vec![("chief".to_owned(), 10.0)]
    );
    assert_eq!(
        lines_with(&logs, "focus unknown").len(),
        1,
        "one line each time the focus becomes unknown, not one each tick"
    );
}

#[tokio::test]
async fn agent_seen_clears_an_agent_of_a_rex_host() {
    const HOST_PANE: &str = "rex:omarchy/block:b1";
    static CLOCK: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(42_000);
    let logs = crate::outbound::fake::CapturedLogs::start();
    crate::telemetry::take_noted_waiting_durations();
    let hosts = HostAgents::new(vec!["omarchy".into()]);
    hosts.record("omarchy", reached(ObservedAgentState::Working), 9_000);
    let mut rig = SeenRig::new(ScriptedFocus::focused_on("%1"), hosts.clone(), || {
        CLOCK.load(std::sync::atomic::Ordering::SeqCst)
    })
    .await;
    item_where(&mut rig.receiver, |item| item.focus_known == Some(true)).await;

    hosts.record("omarchy", reached(ObservedAgentState::Done), 9_500);
    let waiting = rig.row_waiting(HOST_PANE, true).await;
    assert_eq!(waiting.waiting_since_ms, 42_000);

    CLOCK.store(48_000, std::sync::atomic::Ordering::SeqCst);
    rig.seen(HOST_PANE).await;
    let seen = rig.row_waiting(HOST_PANE, false).await;
    assert_eq!(seen.waiting_since_ms, 0);
    assert_eq!(seen.state, i32::from(AgentState::Done), "the state stays");
    let lines = lines_with(&logs, "agent seen");
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].contains("pane_id=rex:omarchy/block:b1"),
        "{}",
        lines[0]
    );
    assert!(lines[0].contains("waited_s=6"), "{}", lines[0]);
    // An agent of a Rex host has no role.
    assert_eq!(
        crate::telemetry::take_noted_waiting_durations(),
        vec![(String::new(), 6.0)]
    );
}

/// A registry on the real turn-end path of `service`, with the host `omarchy`.
fn slave_registry_on(service: &mut HaroldService) -> Arc<super::slave::registry::SlaveRegistry> {
    service.host_agents = HostAgents::new(vec!["omarchy".into()]);
    Arc::new(super::slave::registry::SlaveRegistry::new(
        service.host_agents.clone(),
        Arc::new(service.turn_path()),
    ))
}

fn slave_turn(session_id: &str, transcript_path: &str) -> harold_api::harold::slave::v1::Turn {
    harold_api::harold::slave::v1::Turn {
        report_no: 1,
        pane_id: "block:a1".into(),
        pane_label: "work:0.1".into(),
        session_id: session_id.into(),
        last_user_prompt: "do it".into(),
        assistant_message: "done".into(),
        main_context: "repo".into(),
        input_tokens: 5,
        transcript_path: transcript_path.into(),
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_slave_turn_is_stored_by_the_turn_end_path_under_its_host_pane_id() {
    use super::slave::master::MasterSink;
    let directory = TestDirectory::new();
    let store = Arc::new(store::HaroldStore::open(&directory.0).await.unwrap());
    let (mut service, _shutdown, task) = test_service(Arc::clone(&store), empty_snapshot());
    let recorded = Arc::new(RecordedTurnEnds::default());
    service.restart_reminder =
        Some(Arc::clone(&recorded) as Arc<dyn crate::restart_reminder::TurnEnds>);
    let registry = slave_registry_on(&mut service);

    let applied = {
        let registry = Arc::clone(&registry);
        tokio::spawn(async move { registry.apply_turn("omarchy", slave_turn("s-1", "/x/t.jsonl")) })
            .await
            .unwrap()
    };

    assert_eq!(applied, super::slave::master::Applied::Applied);
    let events = store
        .stream()
        .load_after_version(EventVersion::start(), 10)
        .await
        .unwrap();
    assert_eq!(events.len(), 1);
    let event: store::TurnCompleted = serde_json::from_value(events[0].payload.clone()).unwrap();
    assert_eq!(event.pane_id, "rex:omarchy/block:a1");
    assert_eq!(event.pane_label, "work:0.1");
    assert_eq!(event.last_user_prompt, "do it");
    assert_eq!(event.assistant_message, "done");
    // The slave's transcript path means nothing here: the master passes it on empty.
    assert_eq!(
        *recorded.0.lock().unwrap(),
        [(
            "rex:omarchy/block:a1".to_string(),
            "s-1".to_string(),
            String::new()
        )]
    );
    task.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn no_recap_for_slave_agents() {
    use super::harold::{AgentPaneState, AgentState as WireState, AgentStateSnapshot};
    use super::slave::master::MasterSink;
    let directory = TestDirectory::new();
    let store = Arc::new(store::HaroldStore::open(&directory.0).await.unwrap());
    let (mut service, _shutdown, task) = test_service(Arc::clone(&store), empty_snapshot());
    let registry = slave_registry_on(&mut service);
    registry.link_changed("omarchy", true);
    registry.apply_snapshot(
        "omarchy",
        AgentStateSnapshot {
            panes: vec![AgentPaneState {
                pane_id: "block:a1".into(),
                agent_pid: 77,
                pane_pid: 77,
                provider_id: "claude".into(),
                state: WireState::Working as i32,
                recap: Some("the slave's recap".into()),
                recap_at_ms: 55,
                ..Default::default()
            }],
            ..Default::default()
        },
    );
    // A slave turn with a session and a transcript starts no recap read either.
    let applied = {
        let registry = Arc::clone(&registry);
        tokio::spawn(async move { registry.apply_turn("omarchy", slave_turn("s-1", "/x/t.jsonl")) })
            .await
            .unwrap()
    };
    assert_eq!(applied, super::slave::master::Applied::Applied);

    let mut stream = service
        .watch_agent_states(Request::new(WatchAgentStatesRequest {}))
        .await
        .unwrap()
        .into_inner();
    let item = stream.next().await.expect("an item").unwrap();
    let row = item
        .panes
        .iter()
        .find(|pane| pane.pane_id == "rex:omarchy/block:a1")
        .expect("the slave's agent has its host pane id");
    assert_eq!(row.host, "omarchy");
    assert_eq!((row.recap.as_deref(), row.recap_at_ms), (None, 0));
    assert!(service.recaps.targets().is_empty());
    task.abort();
}

#[test]
fn a_refused_start_counts_its_problems_and_a_load_error_counts_one() {
    let problems = vec!["one".to_string(), "two".to_string()];
    assert_eq!(super::refused_errors_count(&problems), 2);
    assert_eq!(super::refused_errors_count(&[]), 1);
}

/// Starts twice on a temporary folder with no clean exit between: the second start is the
/// one unclean start.
fn guard_after_unclean_starts(starts: usize) -> crate::crash_guard::CrashGuard {
    // The crash file sits beside the store path, so the store path is one level down.
    let directory = TestDirectory::new();
    let file = crate::crash_guard::crash_file(&directory.0.join("store"));
    let now = std::time::SystemTime::now();
    let mut last = None;
    for _ in 0..starts {
        match crate::crash_guard::begin(&file, now).unwrap() {
            crate::crash_guard::Start::Run(started) => last = Some(started),
            crate::crash_guard::Start::Refuse => panic!("the start was refused"),
        }
    }
    last.expect("at least one start")
}

#[test]
fn a_start_after_an_unclean_exit_logs_the_count() {
    let logs = crate::outbound::fake::CapturedLogs::start();
    let started = guard_after_unclean_starts(2);
    assert_eq!(started.unclean_starts(), 1);

    super::log_unclean_start(&started);

    let lines = lines_with(&logs, "start after an unclean exit");
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("count=1"), "{lines:?}");
}

#[test]
fn a_start_after_a_clean_exit_logs_no_unclean_line() {
    let logs = crate::outbound::fake::CapturedLogs::start();
    let started = guard_after_unclean_starts(1);
    assert_eq!(started.unclean_starts(), 0);

    super::log_unclean_start(&started);

    assert!(lines_with(&logs, "start after an unclean exit").is_empty());
}

#[tokio::test]
async fn a_refused_config_outside_launchd_logs_and_exports_the_refusal_and_returns_its_error() {
    let logs = crate::outbound::fake::CapturedLogs::start();
    let refused_before = crate::telemetry::test_support::exported_text()
        .matches("startup refused")
        .count();

    let error =
        super::refuse_by_hand("invalid configuration: one; two; three".into(), 3, None).await;

    assert_eq!(error.to_string(), "invalid configuration: one; two; three");
    let lines = lines_with(&logs, "startup refused");
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("reason=\"invalid_config\""), "{lines:?}");
    assert!(lines[0].contains("errors=3"), "{lines:?}");
    let refused_after = crate::telemetry::test_support::exported_text()
        .matches("startup refused")
        .count();
    assert_eq!(
        refused_after,
        refused_before + 1,
        "the refusal reaches the exporter"
    );
}
