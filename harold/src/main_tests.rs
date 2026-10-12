use std::sync::Arc;

use events::EventStreamVersion;
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
use super::harold::harold_client::HaroldClient;
use super::harold::harold_server::{Harold, HaroldServer};
use super::harold::{
    AgentState, ForwardReplyRequest, MonitorHealthState, ReportAgentStateRequest,
    ReportNotificationRequest, SendMessageToAwayDeviceRequest, SetMessagingPausedRequest,
    SetTtsMutedRequest, WatchAgentStatesRequest,
};
use super::inbound::ownership::recorded_notifications;
use super::peers::PeerEndpoint;
use super::settings::{PeerHub, PeerMember};
use super::{
    HaroldService, Request, TurnCompleteRequest, apply_setting, load_startup_agent_snapshot,
    map_agent_snapshot, pane_id_for_log, restore_startup_state, store,
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
            peers: super::peers::PeerEndpoint::Off,
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
        through_event_version: EventStreamVersion::start(),
        server_time_ms: 1_000,
        monitor_health: Vec::new(),
        panes: Vec::new(),
    }
}

fn populated_snapshot(revision: i64, summary: Option<&str>) -> AgentSnapshot {
    AgentSnapshot {
        tts_muted: false,
        messaging_paused: false,
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
            lifecycle_adapter_id: None,
            screen_state: None,
            screen_classifier_id: None,
            screen_observed_at_ms: None,
            effective_state: EffectiveAgentState::Busy,
            explicit_work_summary: summary.map(str::to_string),
            explicit_work_summary_updated_at_ms: summary.map(|_| 8_600),
            screen_work_summary: None,
            screen_work_summary_updated_at_ms: None,
            summary_basis_version: EventStreamVersion::start(),
            generated_work_summary: None,
            generated_summary_basis_version: None,
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

/// A Harold serving on an ephemeral 127.0.0.1 port, for tests of the calls between Macs.
pub(crate) struct PeerServer {
    pub(crate) url: String,
    pub(crate) store: Arc<store::HaroldStore>,
    _directory: TestDirectory,
    _shutdown: watch::Sender<()>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for PeerServer {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

pub(crate) async fn start_peer_server(peers: PeerEndpoint) -> PeerServer {
    let directory = TestDirectory::new();
    let store = Arc::new(store::HaroldStore::open(&directory.0).await.unwrap());
    let (mut service, shutdown, monitor_task) = test_service(Arc::clone(&store), empty_snapshot());
    service.peers = peers;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server_task = tokio::spawn(async move {
        let _ = tonic::transport::Server::builder()
            .add_service(HaroldServer::new(service))
            .serve_with_incoming(tonic::transport::server::TcpIncoming::from(listener))
            .await;
    });
    PeerServer {
        url,
        store,
        _directory: directory,
        _shutdown: shutdown,
        tasks: vec![monitor_task, server_task],
    }
}

/// Every reply stored in `store`, in order.
pub(crate) async fn inbound_messages(store: &store::HaroldStore) -> Vec<store::InboundMessage> {
    store
        .stream()
        .load_after_version(EventStreamVersion::start(), 100)
        .await
        .unwrap()
        .into_iter()
        .filter(|event| event.r#type == "InboundMessageReceived")
        .map(|event| serde_json::from_value(event.payload).unwrap())
        .collect()
}

/// An address on this machine that nothing listens on.
pub(crate) fn closed_port_url() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    format!("http://{}", listener.local_addr().unwrap())
}

fn hub_with(member: &str) -> PeerEndpoint {
    PeerEndpoint::Hub {
        members: vec![PeerMember {
            name: member.into(),
            url: "http://127.0.0.1:1".into(),
            secret: "s3cret".into(),
        }],
    }
}

fn member_of_a_hub() -> PeerEndpoint {
    PeerEndpoint::Member {
        hub: PeerHub {
            url: "http://127.0.0.1:1".into(),
            secret: "s3cret".into(),
        },
    }
}

async fn report(server: &PeerServer, machine: &str, secret: &str) -> Result<bool, tonic::Status> {
    let mut client = HaroldClient::connect(server.url.clone()).await.unwrap();
    client
        .report_notification(ReportNotificationRequest {
            machine: machine.into(),
            secret: secret.into(),
        })
        .await
        .map(|response| response.into_inner().accepted)
}

async fn forward(server: &PeerServer, text: &str, secret: &str) -> Result<bool, tonic::Status> {
    let mut client = HaroldClient::connect(server.url.clone()).await.unwrap();
    client
        .forward_reply(ForwardReplyRequest {
            text: text.into(),
            secret: secret.into(),
        })
        .await
        .map(|response| response.into_inner().accepted)
}

// Each test below names its member differently: the log of recorded names is shared by
// every test in the process.

#[tokio::test]
async fn hub_records_a_reported_notification_under_the_configured_name() {
    super::settings::init_settings_for_test();
    let server = start_peer_server(hub_with("laptop-rpc-ok")).await;

    let accepted = report(&server, "LAPTOP-RPC-OK", "s3cret").await;

    assert!(accepted.unwrap());
    assert!(recorded_notifications().contains(&"laptop-rpc-ok".to_string()));
}

#[tokio::test]
async fn hub_denies_a_report_with_a_wrong_secret_and_records_nothing() {
    super::settings::init_settings_for_test();
    let server = start_peer_server(hub_with("laptop-rpc-wrong")).await;

    for secret in ["wrong", ""] {
        let status = report(&server, "laptop-rpc-wrong", secret)
            .await
            .unwrap_err();

        assert_eq!(status.code(), tonic::Code::PermissionDenied, "{secret:?}");
        assert!(!status.message().contains("s3cret"));
    }
    assert!(!recorded_notifications().contains(&"laptop-rpc-wrong".to_string()));
}

#[tokio::test]
async fn hub_denies_a_report_from_an_unknown_member() {
    super::settings::init_settings_for_test();
    let server = start_peer_server(hub_with("laptop-rpc-known")).await;

    let status = report(&server, "laptop-rpc-stranger", "s3cret")
        .await
        .unwrap_err();

    assert_eq!(status.code(), tonic::Code::PermissionDenied);
    assert!(!recorded_notifications().contains(&"laptop-rpc-stranger".to_string()));
}

#[tokio::test]
async fn a_harold_that_is_not_the_hub_denies_a_report() {
    super::settings::init_settings_for_test();
    for endpoint in [member_of_a_hub(), PeerEndpoint::Off] {
        let server = start_peer_server(endpoint).await;

        let status = report(&server, "laptop-rpc-role", "s3cret")
            .await
            .unwrap_err();

        assert_eq!(status.code(), tonic::Code::PermissionDenied);
    }
    assert!(!recorded_notifications().contains(&"laptop-rpc-role".to_string()));
}

#[tokio::test]
async fn member_stores_a_forwarded_reply_marked_forwarded() {
    let server = start_peer_server(member_of_a_hub()).await;

    let accepted = forward(&server, "[laptop/harold main:0.0] revert that", "s3cret").await;

    assert!(accepted.unwrap());
    let stored = inbound_messages(&server.store).await;
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].text, "[laptop/harold main:0.0] revert that");
    assert!(stored[0].forwarded);
}

#[tokio::test]
async fn member_stores_a_forwarded_reply_trimmed() {
    let server = start_peer_server(member_of_a_hub()).await;

    let accepted = forward(&server, "  \n revert that \t", "s3cret").await;

    assert!(accepted.unwrap());
    let stored = inbound_messages(&server.store).await;
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].text, "revert that");
    assert!(stored[0].forwarded);
}

/// A reply the Telegram listener would not store is accepted and ignored: the hub did its
/// job, and retrying cannot make the text routable.
#[tokio::test]
async fn member_accepts_and_ignores_a_forwarded_reply_the_listener_would_skip() {
    let server = start_peer_server(member_of_a_hub()).await;

    for text in [
        "",
        "   \n\t",
        "\u{1F916} [laptop/harold main:0.0] Done.",
        "  \u{1F916} hi",
    ] {
        let accepted = forward(&server, text, "s3cret").await;

        assert!(accepted.unwrap(), "{text:?}");
    }
    assert!(inbound_messages(&server.store).await.is_empty());
}

#[tokio::test]
async fn forwarded_reply_with_a_wrong_secret_is_denied_before_any_filtering() {
    let server = start_peer_server(member_of_a_hub()).await;

    for text in ["", "\u{1F916} hi", "carry on"] {
        let status = forward(&server, text, "wrong").await.unwrap_err();

        assert_eq!(status.code(), tonic::Code::PermissionDenied, "{text:?}");
    }
    assert!(inbound_messages(&server.store).await.is_empty());
}

#[tokio::test]
async fn member_denies_a_forwarded_reply_with_a_wrong_secret_and_stores_nothing() {
    let server = start_peer_server(member_of_a_hub()).await;

    for secret in ["wrong", ""] {
        let status = forward(&server, "carry on", secret).await.unwrap_err();

        assert_eq!(status.code(), tonic::Code::PermissionDenied, "{secret:?}");
    }
    assert!(inbound_messages(&server.store).await.is_empty());
}

#[tokio::test]
async fn a_harold_that_is_not_a_member_denies_a_forwarded_reply() {
    for endpoint in [hub_with("laptop-rpc-fwd"), PeerEndpoint::Off] {
        let server = start_peer_server(endpoint).await;

        let status = forward(&server, "carry on", "s3cret").await.unwrap_err();

        assert_eq!(status.code(), tonic::Code::PermissionDenied);
        assert!(inbound_messages(&server.store).await.is_empty());
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
            state: ObservedAgentState::Busy,
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
        .load_after_version(EventStreamVersion::start(), 1_000)
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
            forwarded: false,
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
fn snapshot_mapping_carries_tts_muted() {
    let mut snapshot = empty_snapshot();
    snapshot.tts_muted = true;
    assert!(map_agent_snapshot(snapshot).tts_muted);
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
    assert!(map_agent_snapshot(snapshot).messaging_paused);
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

const WIRING_HUB: &str = r#"
    machine_name = "studio"
    machines = ["studio", "laptop"]
    [notify]
    away_channel = "telegram"
    [[peers.members]]
    name = "laptop"
    url = "http://laptop.tailnet:50060"
    secret = "s3cret"
"#;

const WIRING_MEMBER: &str = r#"
    machine_name = "laptop"
    machines = ["studio", "laptop"]
    [notify]
    away_channel = "telegram"
    [peers]
    role = "member"
    [peers.hub]
    url = "http://studio.tailnet:50060"
    secret = "s3cret"
"#;

/// The peer endpoint of the service built from `toml` laid over the default settings.
async fn served_peers(toml: &str) -> PeerEndpoint {
    let settings = super::peers::settings_with(toml);
    let directory = TestDirectory::new();
    let store = Arc::new(store::HaroldStore::open(&directory.0).await.unwrap());
    let (base, _shutdown, task) = test_service(Arc::clone(&store), empty_snapshot());
    let service = super::harold_service(
        &settings,
        base.monitor,
        store,
        base.snapshots,
        base.shutdown,
    );
    task.abort();
    service.peers
}

#[tokio::test]
async fn service_answers_the_peer_calls_its_settings_call_for() {
    assert_eq!(
        served_peers(WIRING_HUB).await,
        PeerEndpoint::Hub {
            members: vec![PeerMember {
                name: "laptop".into(),
                url: "http://laptop.tailnet:50060".into(),
                secret: "s3cret".into(),
            }]
        }
    );
    assert_eq!(
        served_peers(WIRING_MEMBER).await,
        PeerEndpoint::Member {
            hub: PeerHub {
                url: "http://studio.tailnet:50060".into(),
                secret: "s3cret".into(),
            }
        }
    );
    let imessage = WIRING_MEMBER.replace("telegram", "imessage");
    assert_eq!(served_peers(&imessage).await, PeerEndpoint::Off);
}

/// Every agent message stored in `store`, in order, and the count of all stored events.
async fn away_messages(store: &store::HaroldStore) -> (Vec<store::AwayMessageRequested>, usize) {
    let events = store
        .stream()
        .load_after_version(EventStreamVersion::start(), 100)
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
