use std::sync::Arc;

use events::EventStreamVersion;
use tokio::sync::watch;
use tokio_stream::StreamExt;

use super::agent::domain::{
    AgentEvent, AgentIncarnation, AgentPaneObservation, AgentPaneObserved, AgentPaneProjection,
    AgentSnapshot, EffectiveAgentState, MonitorHealthProjection,
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
    let (shutdown, shutdown_rx) = watch::channel(());
    let (monitor, task) = spawn_agent_monitor_for_test(
        store,
        Arc::new(EmptyInventory),
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
async fn report_agent_state_remains_unimplemented_until_its_ingress_slice() {
    let directory = TestDirectory::new();
    let store = Arc::new(store::HaroldStore::open(&directory.0).await.unwrap());
    let (service, _shutdown, task) = test_service(store, empty_snapshot());

    let report_error = service
        .report_agent_state(Request::new(ReportAgentStateRequest {
            pane_id: "%8".into(),
            state: AgentState::Busy.into(),
            adapter_id: "codex-hook".into(),
            work_summary: Some("refresh events".into()),
        }))
        .await
        .unwrap_err();
    assert_eq!(report_error.code(), tonic::Code::Unimplemented);

    task.abort();
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
