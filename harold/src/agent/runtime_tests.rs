use std::collections::VecDeque;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use events::EventStreamVersion;
use prost::Message;
use tokio::sync::watch;

use super::domain::{
    AgentEvent, AgentIncarnation, AgentLifecycleObserved, AgentPaneObservation, AgentPaneObserved,
    AgentSnapshot, CompletionSummaryUpdate, EffectiveAgentState, ObservedAgentState,
    ScreenObservation, WorkSummaryUpdate,
};
use super::inventory::{AgentInventoryPort, InventoryError};
use super::runtime::{
    AgentMonitorHandle, AgentMonitorSeed, MonitorCommandError, coalesced_tick_channel_for_test,
    spawn_agent_monitor_for_test, spawn_agent_monitor_seeded_for_test,
};
use super::screen::{
    CommandOutput, CommandRunner, ScreenError, TmuxVisibleScreen, VisibleScreenPort,
};
use super::snapshot::AgentSnapshotHub;
use crate::harold::harold_server::Harold;
use crate::harold::{AgentState, ReportAgentStateRequest, TurnCompleteRequest};
use crate::settings::AgentProviderSettings;
use crate::store::{HaroldStore, TurnCompleted};
use crate::{HaroldService, Request};

struct TestDirectory(std::path::PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("harold-monitor-{}", uuid::Uuid::new_v4()));
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
struct FakeInventory {
    scans: Mutex<VecDeque<Result<Vec<AgentPaneObservation>, InventoryError>>>,
    resolutions: Mutex<VecDeque<Result<Option<AgentPaneObservation>, InventoryError>>>,
    current: Mutex<VecDeque<Result<bool, InventoryError>>>,
    revalidated: Mutex<Vec<AgentIncarnation>>,
}

impl FakeInventory {
    fn scans(results: Vec<Result<Vec<AgentPaneObservation>, InventoryError>>) -> Self {
        Self {
            scans: Mutex::new(results.into()),
            ..Self::default()
        }
    }

    fn push_resolution(&self, result: Result<Option<AgentPaneObservation>, InventoryError>) {
        self.resolutions.lock().unwrap().push_back(result);
    }

    fn push_current(&self, result: Result<bool, InventoryError>) {
        self.current.lock().unwrap().push_back(result);
    }
}

impl AgentInventoryPort for FakeInventory {
    fn scan(&self) -> Result<Vec<AgentPaneObservation>, InventoryError> {
        self.scans
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Ok(Vec::new()))
    }

    fn resolve(&self, _pane_id: &str) -> Result<Option<AgentPaneObservation>, InventoryError> {
        self.resolutions
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Ok(None))
    }

    fn is_current(&self, incarnation: &AgentIncarnation) -> Result<bool, InventoryError> {
        self.revalidated.lock().unwrap().push(incarnation.clone());
        self.current
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Ok(false))
    }
}

#[derive(Default)]
struct FakeScreen {
    observations: Mutex<VecDeque<Result<ScreenObservation, ScreenError>>>,
}

struct CapturedScreenRunner {
    outputs: Mutex<VecDeque<CommandOutput>>,
}

impl CommandRunner for CapturedScreenRunner {
    fn output(&self, _program: &str, _args: &[&str]) -> std::io::Result<CommandOutput> {
        Ok(self
            .outputs
            .lock()
            .expect("captured screen output lock")
            .pop_front()
            .expect("captured screen output"))
    }
}

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

fn assert_persisted_files_exclude(root: &Path, sentinel: &[u8]) {
    for entry in std::fs::read_dir(root).expect("read store directory") {
        let path = entry.expect("read store entry").path();
        if path.is_dir() {
            assert_persisted_files_exclude(&path, sentinel);
            continue;
        }
        let bytes = std::fs::read(&path).expect("read persisted store file");
        assert!(
            !bytes
                .windows(sentinel.len())
                .any(|window| window == sentinel),
            "raw capture leaked into a persisted store file"
        );
    }
}

impl FakeScreen {
    fn push(&self, observation: Result<ScreenObservation, ScreenError>) {
        self.observations.lock().unwrap().push_back(observation);
    }
}

impl VisibleScreenPort for FakeScreen {
    fn observe(
        &self,
        _pane: &AgentPaneObservation,
        _provider: &AgentProviderSettings,
    ) -> Result<ScreenObservation, ScreenError> {
        self.observations
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Err(ScreenError::CaptureFailed))
    }
}

struct Fixture {
    _directory: TestDirectory,
    store: Arc<HaroldStore>,
    inventory: Arc<FakeInventory>,
    screen: Arc<FakeScreen>,
    handle: AgentMonitorHandle,
    _shutdown: watch::Sender<()>,
    task: tokio::task::JoinHandle<()>,
}

impl Fixture {
    async fn new(inventory: FakeInventory) -> Self {
        let directory = TestDirectory::new();
        let store = Arc::new(HaroldStore::open(&directory.0).await.unwrap());
        let inventory = Arc::new(inventory);
        let screen = Arc::new(FakeScreen::default());
        let (shutdown, shutdown_rx) = watch::channel(());
        let (handle, task) = spawn_agent_monitor_for_test(
            Arc::clone(&store),
            inventory.clone(),
            screen.clone(),
            vec![provider()],
            2_000,
            shutdown_rx,
        );
        Self {
            _directory: directory,
            store,
            inventory,
            screen,
            handle,
            _shutdown: shutdown,
            task,
        }
    }

    async fn events(&self) -> Vec<events::EventEnvelope> {
        self.store
            .stream()
            .load_after_version(EventStreamVersion::start(), 100)
            .await
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn fixture_event_count(store: &HaroldStore) -> usize {
    store
        .stream()
        .load_after_version(EventStreamVersion::start(), 100)
        .await
        .unwrap()
        .len()
}

#[tokio::test]
async fn lifecycle_resolves_full_incarnation_and_appends_one_contiguous_batch() {
    let fixture = Fixture::new(FakeInventory::default()).await;
    fixture
        .inventory
        .push_resolution(Ok(Some(pane("%8", 80, 800, 1_000, 100))));

    fixture
        .handle
        .report_lifecycle(
            "%8".into(),
            ObservedAgentState::Busy,
            "codex-hook".into(),
            WorkSummaryUpdate::Set("  refresh\n events  ".into()),
        )
        .await
        .unwrap();

    let events = fixture.events().await;
    assert_eq!(
        event_types(&events),
        ["AgentPaneObserved", "AgentLifecycleObserved"]
    );
    let observed: super::domain::AgentPaneObserved =
        serde_json::from_value(events[0].payload.clone()).unwrap();
    let lifecycle: super::domain::AgentLifecycleObserved =
        serde_json::from_value(events[1].payload.clone()).unwrap();
    assert_eq!(lifecycle.incarnation, observed.pane.incarnation);
    assert_eq!(
        lifecycle.work_summary,
        WorkSummaryUpdate::Set("refresh events".into())
    );
    assert_eq!(events[1].version.get(), events[0].version.get() + 1);
}

#[tokio::test]
async fn lifecycle_rejects_invalid_or_unresolved_input_without_appending() {
    let fixture = Fixture::new(FakeInventory::default()).await;
    assert!(matches!(
        fixture
            .handle
            .report_lifecycle(
                "8".into(),
                ObservedAgentState::Busy,
                "codex-hook".into(),
                WorkSummaryUpdate::Unchanged,
            )
            .await,
        Err(MonitorCommandError::InvalidInput)
    ));

    fixture.inventory.push_resolution(Ok(None));
    assert!(matches!(
        fixture
            .handle
            .report_lifecycle(
                "%8".into(),
                ObservedAgentState::Busy,
                "codex-hook".into(),
                WorkSummaryUpdate::Unchanged,
            )
            .await,
        Err(MonitorCommandError::AgentNotFound)
    ));
    assert!(fixture.events().await.is_empty());
}

#[tokio::test]
async fn failed_lifecycle_append_does_not_advance_runtime_dedupe() {
    let fixture = Fixture::new(FakeInventory::default()).await;
    let observation = pane("%8", 80, 800, 1_000, 100);
    fixture
        .inventory
        .push_resolution(Ok(Some(observation.clone())));
    fixture.inventory.push_resolution(Ok(Some(observation)));
    fixture.store.fail_next_monitor_append_for_test();

    assert!(matches!(
        fixture
            .handle
            .report_lifecycle(
                "%8".into(),
                ObservedAgentState::Busy,
                "codex-hook".into(),
                WorkSummaryUpdate::Unchanged,
            )
            .await,
        Err(MonitorCommandError::EventAppend(_))
    ));
    fixture
        .handle
        .report_lifecycle(
            "%8".into(),
            ObservedAgentState::Busy,
            "codex-hook".into(),
            WorkSummaryUpdate::Unchanged,
        )
        .await
        .unwrap();
    assert_eq!(
        event_types(&fixture.events().await),
        ["AgentPaneObserved", "AgentLifecycleObserved"]
    );
}

#[tokio::test]
async fn completion_preserves_legacy_payload_and_has_non_destructive_summary_semantics() {
    let fixture = Fixture::new(FakeInventory::default()).await;
    fixture
        .inventory
        .push_resolution(Ok(Some(pane("%8", 80, 800, 1_000, 100))));
    fixture.inventory.push_resolution(Ok(None));

    fixture
        .handle
        .turn_completed(turn("  current\n task  "))
        .await
        .unwrap();
    fixture
        .handle
        .turn_completed(turn(" \u{1b}[31m "))
        .await
        .unwrap();

    let events = fixture.events().await;
    assert_eq!(
        event_types(&events),
        ["AgentPaneObserved", "TurnCompleted", "TurnCompleted"]
    );
    let resolved: TurnCompleted = serde_json::from_value(events[1].payload.clone()).unwrap();
    assert_eq!(resolved.pane_id, "%8");
    assert_eq!(resolved.pane_label, "harold:0.8");
    assert_eq!(resolved.assistant_message, "assistant result");
    assert_eq!(resolved.main_context, "harold");
    assert_eq!(
        resolved.agent_incarnation,
        Some(pane("%8", 80, 800, 1_000, 100).incarnation)
    );
    assert_eq!(
        resolved.work_summary,
        CompletionSummaryUpdate::Set("current task".into())
    );
    let unresolved: TurnCompleted = serde_json::from_value(events[2].payload.clone()).unwrap();
    assert_eq!(unresolved.agent_incarnation, None);
    assert_eq!(unresolved.work_summary, CompletionSummaryUpdate::Unchanged);
}

#[tokio::test]
async fn later_explicit_ingress_rejects_exact_placeholders_and_preserves_update_semantics() {
    const PLACEHOLDER: &str = "Ask Codex to do anything";
    const LEGITIMATE: &str = "Explain why the UI says Ask Codex to do anything";

    let directory = TestDirectory::new();
    let store = Arc::new(HaroldStore::open(&directory.0).await.unwrap());
    let observation = pane("%8", 80, 800, 1_000, 100);
    let inventory = Arc::new(FakeInventory::default());
    for _ in 0..6 {
        inventory.push_resolution(Ok(Some(observation.clone())));
    }
    let mut codex = provider();
    codex.idle_all = vec![" Ask Codex\tto do anything ".into()];
    let (shutdown, shutdown_rx) = watch::channel(());
    let (handle, task) = spawn_agent_monitor_for_test(
        Arc::clone(&store),
        inventory,
        Arc::new(FakeScreen::default()),
        vec![codex],
        2_000,
        shutdown_rx,
    );

    handle
        .report_lifecycle(
            "%8".into(),
            ObservedAgentState::Busy,
            "codex-hook".into(),
            WorkSummaryUpdate::Set(PLACEHOLDER.into()),
        )
        .await
        .unwrap();
    store.project_unhandled_events(100).await.unwrap();
    assert_eq!(
        store.load_agent_snapshot().await.unwrap().panes[0].work_summary,
        None
    );

    handle
        .report_lifecycle(
            "%8".into(),
            ObservedAgentState::Busy,
            "codex-hook".into(),
            WorkSummaryUpdate::Unchanged,
        )
        .await
        .unwrap();
    handle
        .report_lifecycle(
            "%8".into(),
            ObservedAgentState::Busy,
            "codex-hook".into(),
            WorkSummaryUpdate::Set(LEGITIMATE.into()),
        )
        .await
        .unwrap();
    store.project_unhandled_events(100).await.unwrap();
    assert_eq!(
        store.load_agent_snapshot().await.unwrap().panes[0]
            .work_summary
            .as_deref(),
        Some(LEGITIMATE)
    );

    handle
        .report_lifecycle(
            "%8".into(),
            ObservedAgentState::Idle,
            "codex-hook".into(),
            WorkSummaryUpdate::Clear,
        )
        .await
        .unwrap();
    handle.turn_completed(turn(PLACEHOLDER)).await.unwrap();
    handle.turn_completed(turn(" \u{1b}[31m\t ")).await.unwrap();
    store.project_unhandled_events(100).await.unwrap();
    let snapshot = store.load_agent_snapshot().await.unwrap();
    assert_eq!(snapshot.panes[0].explicit_work_summary, None);
    assert_eq!(snapshot.panes[0].work_summary, None);

    let events = store
        .stream()
        .load_after_version(EventStreamVersion::start(), 100)
        .await
        .unwrap();
    assert_eq!(
        event_types(&events),
        [
            "AgentPaneObserved",
            "AgentLifecycleObserved",
            "AgentPaneObserved",
            "AgentLifecycleObserved",
            "AgentPaneObserved",
            "AgentLifecycleObserved",
            "AgentPaneObserved",
            "AgentLifecycleObserved",
            "AgentPaneObserved",
            "TurnCompleted",
            "AgentPaneObserved",
            "TurnCompleted",
        ]
    );
    for event in &events {
        if event.r#type == "AgentLifecycleObserved" {
            let lifecycle: AgentLifecycleObserved =
                serde_json::from_value(event.payload.clone()).unwrap();
            assert_ne!(
                lifecycle.work_summary,
                WorkSummaryUpdate::Set(PLACEHOLDER.into())
            );
        }
        if event.r#type == "TurnCompleted" {
            let completion: TurnCompleted = serde_json::from_value(event.payload.clone()).unwrap();
            assert_ne!(completion.last_user_prompt, PLACEHOLDER);
            assert_ne!(
                completion.work_summary,
                CompletionSummaryUpdate::Set(PLACEHOLDER.into())
            );
        }
    }

    drop(shutdown);
    task.await.unwrap();
}

#[tokio::test]
async fn configured_placeholder_ingress_is_rejected_before_every_event_payload() {
    const PLACEHOLDER: &str = "Ask Codex to do anything";
    const RETAINED: &str = "Keep this real task";

    let directory = TestDirectory::new();
    let store = Arc::new(HaroldStore::open(&directory.0).await.unwrap());
    let observation = pane("%8", 80, 800, 1_000, 100);
    let inventory = Arc::new(FakeInventory::default());
    for _ in 0..3 {
        inventory.push_resolution(Ok(Some(observation.clone())));
    }
    inventory.push_resolution(Ok(None));
    let mut codex = provider();
    codex.idle_all = vec![format!(" \u{1b}[31m{PLACEHOLDER}\u{1b}[0m ")];
    let (shutdown, shutdown_rx) = watch::channel(());
    let (handle, task) = spawn_agent_monitor_for_test(
        Arc::clone(&store),
        inventory,
        Arc::new(FakeScreen::default()),
        vec![codex],
        2_000,
        shutdown_rx,
    );

    handle
        .report_lifecycle(
            "%8".into(),
            ObservedAgentState::Busy,
            "codex-hook".into(),
            WorkSummaryUpdate::Set(RETAINED.into()),
        )
        .await
        .unwrap();
    handle
        .report_lifecycle(
            "%8".into(),
            ObservedAgentState::Busy,
            "codex-hook".into(),
            WorkSummaryUpdate::Set(PLACEHOLDER.into()),
        )
        .await
        .unwrap();
    handle.turn_completed(turn(PLACEHOLDER)).await.unwrap();
    handle.turn_completed(turn(PLACEHOLDER)).await.unwrap();
    let mut never_tracked = turn(PLACEHOLDER);
    never_tracked.pane_id = "%99".into();
    handle.turn_completed(never_tracked).await.unwrap();

    let events = store
        .stream()
        .load_after_version(EventStreamVersion::start(), 100)
        .await
        .unwrap();
    assert!(events.iter().all(|event| {
        !serde_json::to_string(&event.payload)
            .unwrap()
            .contains(PLACEHOLDER)
    }));
    assert!(
        events
            .iter()
            .all(|event| event.r#type != "AgentWorkSummaryCandidatesRepaired")
    );
    store.project_unhandled_events(100).await.unwrap();
    assert_eq!(
        store.load_agent_snapshot().await.unwrap().panes[0]
            .work_summary
            .as_deref(),
        Some(RETAINED)
    );
    let completions: Vec<TurnCompleted> = events
        .iter()
        .filter(|event| event.r#type == "TurnCompleted")
        .map(|event| serde_json::from_value(event.payload.clone()).unwrap())
        .collect();
    assert_eq!(completions.len(), 3);
    assert_eq!(
        completions[0].agent_incarnation,
        Some(observation.incarnation)
    );
    assert_eq!(completions[1].agent_incarnation, None);
    assert_eq!(completions[2].agent_incarnation, None);

    drop(shutdown);
    task.await.unwrap();
}

#[tokio::test]
async fn resolved_unknown_provider_lifecycle_rejects_every_configured_exact_placeholder() {
    const PLACEHOLDER: &str = "Ask Codex to do anything";

    let directory = TestDirectory::new();
    let store = Arc::new(HaroldStore::open(&directory.0).await.unwrap());
    let mut unknown = pane("%8", 80, 800, 1_000, 100);
    unknown.incarnation.provider_id = "unknown".into();
    unknown.provider_display_name = "Unknown".into();
    let inventory = Arc::new(FakeInventory::default());
    inventory.push_resolution(Ok(Some(unknown.clone())));
    let mut codex = provider();
    codex.idle_all = vec![format!(" \u{1b}[31m{PLACEHOLDER}\u{1b}[0m ")];
    let (shutdown, shutdown_rx) = watch::channel(());
    let (handle, task) = spawn_agent_monitor_for_test(
        Arc::clone(&store),
        inventory,
        Arc::new(FakeScreen::default()),
        vec![codex],
        2_000,
        shutdown_rx,
    );

    handle
        .report_lifecycle(
            "%8".into(),
            ObservedAgentState::Busy,
            "unknown-hook".into(),
            WorkSummaryUpdate::Set(PLACEHOLDER.into()),
        )
        .await
        .unwrap();

    let events = store
        .stream()
        .load_after_version(EventStreamVersion::start(), 100)
        .await
        .unwrap();
    assert!(events.iter().all(|event| {
        !serde_json::to_string(&event.payload)
            .unwrap()
            .contains(PLACEHOLDER)
    }));
    let lifecycle: AgentLifecycleObserved = events
        .iter()
        .find(|event| event.r#type == "AgentLifecycleObserved")
        .map(|event| serde_json::from_value(event.payload.clone()).unwrap())
        .unwrap();
    assert_eq!(lifecycle.incarnation, unknown.incarnation);
    assert_eq!(lifecycle.work_summary, WorkSummaryUpdate::Unchanged);

    drop(shutdown);
    task.await.unwrap();
}

#[tokio::test]
async fn unknown_provider_completion_rejects_configured_placeholder_when_resolved_or_tracked() {
    const PLACEHOLDER: &str = "Ask Codex to do anything";

    let directory = TestDirectory::new();
    let store = Arc::new(HaroldStore::open(&directory.0).await.unwrap());
    let mut unknown = pane("%8", 80, 800, 1_000, 100);
    unknown.incarnation.provider_id = "unknown".into();
    unknown.provider_display_name = "Unknown".into();
    let inventory = Arc::new(FakeInventory::default());
    inventory.push_resolution(Ok(Some(unknown.clone())));
    inventory.push_resolution(Ok(None));
    let mut codex = provider();
    codex.idle_all = vec![PLACEHOLDER.into()];
    let (shutdown, shutdown_rx) = watch::channel(());
    let (handle, task) = spawn_agent_monitor_for_test(
        Arc::clone(&store),
        inventory,
        Arc::new(FakeScreen::default()),
        vec![codex],
        2_000,
        shutdown_rx,
    );

    handle.turn_completed(turn(PLACEHOLDER)).await.unwrap();
    handle.turn_completed(turn(PLACEHOLDER)).await.unwrap();

    let events = store
        .stream()
        .load_after_version(EventStreamVersion::start(), 100)
        .await
        .unwrap();
    assert!(events.iter().all(|event| {
        !serde_json::to_string(&event.payload)
            .unwrap()
            .contains(PLACEHOLDER)
    }));
    let completions: Vec<TurnCompleted> = events
        .iter()
        .filter(|event| event.r#type == "TurnCompleted")
        .map(|event| serde_json::from_value(event.payload.clone()).unwrap())
        .collect();
    assert_eq!(completions.len(), 2);
    assert_eq!(completions[0].agent_incarnation, Some(unknown.incarnation));
    assert_eq!(completions[1].agent_incarnation, None);
    assert!(completions.iter().all(|completion| {
        completion.last_user_prompt.is_empty()
            && completion.work_summary == CompletionSummaryUpdate::Unchanged
    }));

    drop(shutdown);
    task.await.unwrap();
}

#[tokio::test]
async fn known_provider_does_not_reject_another_providers_exact_placeholder() {
    const CODEX_PLACEHOLDER: &str = "Ask Codex to do anything";
    const CLAUDE_PLACEHOLDER: &str = "How can I help you today?";

    let directory = TestDirectory::new();
    let store = Arc::new(HaroldStore::open(&directory.0).await.unwrap());
    let observation = pane("%8", 80, 800, 1_000, 100);
    let inventory = Arc::new(FakeInventory::default());
    inventory.push_resolution(Ok(Some(observation)));
    let mut codex = provider();
    codex.idle_all = vec![CODEX_PLACEHOLDER.into()];
    let mut claude = provider();
    claude.id = "claude".into();
    claude.display_name = "Claude".into();
    claude.command_contains = vec!["claude".into()];
    claude.idle_all = vec![CLAUDE_PLACEHOLDER.into()];
    let (shutdown, shutdown_rx) = watch::channel(());
    let (handle, task) = spawn_agent_monitor_for_test(
        Arc::clone(&store),
        inventory,
        Arc::new(FakeScreen::default()),
        vec![codex, claude],
        2_000,
        shutdown_rx,
    );

    handle
        .report_lifecycle(
            "%8".into(),
            ObservedAgentState::Busy,
            "codex-hook".into(),
            WorkSummaryUpdate::Set(CLAUDE_PLACEHOLDER.into()),
        )
        .await
        .unwrap();

    store.project_unhandled_events(100).await.unwrap();
    assert_eq!(
        store.load_agent_snapshot().await.unwrap().panes[0]
            .work_summary
            .as_deref(),
        Some(CLAUDE_PLACEHOLDER)
    );

    drop(shutdown);
    task.await.unwrap();
}

#[tokio::test]
async fn lifecycle_placeholder_cannot_leak_at_a_projection_page_boundary() {
    const PLACEHOLDER: &str = "Ask Codex to do anything";
    const RETAINED: &str = "Keep this real task";

    let (directory, store, snapshot, observation) = unprojected_page_boundary_store(RETAINED).await;
    let inventory = Arc::new(FakeInventory::default());
    inventory.push_resolution(Ok(Some(observation)));
    let mut codex = provider();
    codex.idle_all = vec![PLACEHOLDER.into()];
    let (shutdown, shutdown_rx) = watch::channel(());
    let (handle, task) = spawn_agent_monitor_seeded_for_test(
        Arc::clone(&store),
        inventory,
        Arc::new(FakeScreen::default()),
        vec![codex],
        AgentMonitorSeed {
            snapshot,
            hook_grace_ms: 2_000,
            acquisition_timeout: Duration::from_millis(50),
        },
        shutdown_rx,
    );

    handle
        .report_lifecycle(
            "%8".into(),
            ObservedAgentState::Busy,
            "codex-hook".into(),
            WorkSummaryUpdate::Set(PLACEHOLDER.into()),
        )
        .await
        .unwrap();

    let hub = AgentSnapshotHub::new(empty_snapshot());
    crate::projector::project_and_publish_agent_snapshot(&store, &hub, 500)
        .await
        .unwrap();
    assert_eq!(
        hub.subscribe().borrow().panes[0].work_summary.as_deref(),
        Some(RETAINED)
    );
    let ingress = store
        .stream()
        .load_after_version(EventStreamVersion::new(498).unwrap(), 10)
        .await
        .unwrap();
    assert!(ingress.iter().all(|event| {
        !serde_json::to_string(&event.payload)
            .unwrap()
            .contains(PLACEHOLDER)
    }));

    drop(shutdown);
    task.await.unwrap();
    drop(directory);
}

#[tokio::test]
async fn completion_placeholder_cannot_leak_at_a_projection_page_boundary() {
    const PLACEHOLDER: &str = "Ask Codex to do anything";
    const RETAINED: &str = "Keep this real task";

    let (directory, store, snapshot, observation) = unprojected_page_boundary_store(RETAINED).await;
    let inventory = Arc::new(FakeInventory::default());
    inventory.push_resolution(Ok(Some(observation)));
    let mut codex = provider();
    codex.idle_all = vec![PLACEHOLDER.into()];
    let (shutdown, shutdown_rx) = watch::channel(());
    let (handle, task) = spawn_agent_monitor_seeded_for_test(
        Arc::clone(&store),
        inventory,
        Arc::new(FakeScreen::default()),
        vec![codex],
        AgentMonitorSeed {
            snapshot,
            hook_grace_ms: 2_000,
            acquisition_timeout: Duration::from_millis(50),
        },
        shutdown_rx,
    );

    handle.turn_completed(turn(PLACEHOLDER)).await.unwrap();

    let hub = AgentSnapshotHub::new(empty_snapshot());
    crate::projector::project_and_publish_agent_snapshot(&store, &hub, 500)
        .await
        .unwrap();
    assert_eq!(
        hub.subscribe().borrow().panes[0].work_summary.as_deref(),
        Some(RETAINED)
    );
    let ingress = store
        .stream()
        .load_after_version(EventStreamVersion::new(498).unwrap(), 10)
        .await
        .unwrap();
    assert!(ingress.iter().all(|event| {
        !serde_json::to_string(&event.payload)
            .unwrap()
            .contains(PLACEHOLDER)
    }));

    drop(shutdown);
    task.await.unwrap();
    drop(directory);
}

#[test]
fn legacy_turn_completed_payload_defaults_new_state_fields() {
    let turn: TurnCompleted = serde_json::from_value(serde_json::json!({
        "pane_id": "%8",
        "pane_label": "harold:0.8",
        "last_user_prompt": "legacy prompt",
        "assistant_message": "legacy response",
        "main_context": "harold"
    }))
    .unwrap();

    assert_eq!(turn.agent_incarnation, None);
    assert_eq!(turn.work_summary, CompletionSummaryUpdate::Unchanged);
}

#[tokio::test]
async fn resolved_completion_starts_an_idle_reconciliation_epoch() {
    let observation = pane("%8", 80, 800, 1_000, 100);
    let fixture = Fixture::new(FakeInventory::default()).await;
    fixture
        .inventory
        .push_resolution(Ok(Some(observation.clone())));
    fixture
        .handle
        .turn_completed(turn("current task"))
        .await
        .unwrap();
    fixture.screen.push(Ok(screen(
        &observation,
        Some(ObservedAgentState::Busy),
        Some("fallback"),
        1_500,
    )));
    fixture.handle.screen_tick().await.unwrap();

    let events = fixture.events().await;
    assert_eq!(
        event_types(&events),
        ["AgentPaneObserved", "TurnCompleted", "AgentScreenObserved"]
    );
    let during_grace: super::domain::AgentScreenObserved =
        serde_json::from_value(events[2].payload.clone()).unwrap();
    assert_eq!(during_grace.state, None);
    assert_eq!(during_grace.fallback_summary.as_deref(), Some("fallback"));

    fixture.store.project_unhandled_events(100).await.unwrap();
    let snapshot = fixture.store.load_agent_snapshot().await.unwrap();
    assert_eq!(snapshot.panes.len(), 1);
    assert_eq!(snapshot.panes[0].effective_state, EffectiveAgentState::Idle);
    assert_eq!(snapshot.panes[0].work_summary.as_deref(), Some("fallback"));
    assert_eq!(
        snapshot.panes[0].explicit_work_summary_updated_at_ms,
        Some(100)
    );
    assert_eq!(
        snapshot.panes[0].screen_work_summary_updated_at_ms,
        Some(1_500)
    );
}

#[tokio::test]
async fn resolved_completion_projection_uses_the_runtime_grace_epoch() {
    let observation = pane("%8", 80, 800, 1_000, 100);
    let fixture = Fixture::new(FakeInventory::default()).await;
    fixture
        .inventory
        .push_resolution(Ok(Some(observation.clone())));
    fixture
        .handle
        .turn_completed(turn("current task"))
        .await
        .unwrap();
    fixture.screen.push(Ok(screen(
        &observation,
        Some(ObservedAgentState::Busy),
        None,
        2_100,
    )));
    fixture.handle.screen_tick().await.unwrap();

    fixture.store.project_unhandled_events(100).await.unwrap();
    let snapshot = fixture.store.load_agent_snapshot().await.unwrap();
    assert_eq!(snapshot.panes[0].effective_state, EffectiveAgentState::Busy);
}

#[tokio::test]
async fn departure_requires_two_successful_absences_and_exact_incarnation_revalidation() {
    let original = pane("%8", 80, 800, 1_000, 100);
    let inventory = FakeInventory::scans(vec![
        Ok(vec![original.clone()]),
        Ok(Vec::new()),
        Err(InventoryError::CommandFailed),
        Ok(Vec::new()),
    ]);
    inventory.push_current(Ok(false));
    let fixture = Fixture::new(inventory).await;

    fixture.handle.inventory_tick().await.unwrap();
    fixture.handle.inventory_tick().await.unwrap();
    assert!(matches!(
        fixture.handle.inventory_tick().await,
        Err(MonitorCommandError::InventoryUnavailable)
    ));
    assert_eq!(
        event_types(&fixture.events().await),
        ["AgentPaneObserved", "AgentMonitorHealthChanged"]
    );
    fixture.handle.inventory_tick().await.unwrap();

    let events = fixture.events().await;
    assert_eq!(
        event_types(&events),
        [
            "AgentPaneObserved",
            "AgentMonitorHealthChanged",
            "AgentMonitorHealthChanged",
            "AgentPaneDeparted"
        ]
    );
    assert_eq!(
        *fixture.inventory.revalidated.lock().unwrap(),
        [original.incarnation]
    );
}

#[tokio::test]
async fn lifecycle_and_departure_orders_converge_on_exact_liveness() {
    let original = pane("%8", 80, 800, 1_000, 100);
    let hook_first_inventory = FakeInventory::scans(vec![
        Ok(vec![original.clone()]),
        Ok(Vec::new()),
        Ok(Vec::new()),
    ]);
    hook_first_inventory.push_resolution(Ok(Some(original.clone())));
    hook_first_inventory.push_current(Ok(false));
    let hook_first = Fixture::new(hook_first_inventory).await;
    hook_first.handle.inventory_tick().await.unwrap();
    hook_first.handle.inventory_tick().await.unwrap();
    hook_first
        .handle
        .report_lifecycle(
            "%8".into(),
            ObservedAgentState::Busy,
            "codex-hook".into(),
            WorkSummaryUpdate::Unchanged,
        )
        .await
        .unwrap();
    hook_first.handle.inventory_tick().await.unwrap();
    hook_first
        .store
        .project_unhandled_events(100)
        .await
        .unwrap();
    assert!(
        hook_first
            .store
            .load_agent_snapshot()
            .await
            .unwrap()
            .panes
            .is_empty()
    );

    let replacement = pane("%8", 80, 801, 2_000, 500);
    let departure_first_inventory =
        FakeInventory::scans(vec![Ok(vec![original]), Ok(Vec::new()), Ok(Vec::new())]);
    departure_first_inventory.push_current(Ok(false));
    departure_first_inventory.push_resolution(Ok(Some(replacement.clone())));
    let departure_first = Fixture::new(departure_first_inventory).await;
    departure_first.handle.inventory_tick().await.unwrap();
    departure_first.handle.inventory_tick().await.unwrap();
    departure_first.handle.inventory_tick().await.unwrap();
    departure_first
        .handle
        .report_lifecycle(
            "%8".into(),
            ObservedAgentState::Busy,
            "codex-hook".into(),
            WorkSummaryUpdate::Unchanged,
        )
        .await
        .unwrap();
    departure_first
        .store
        .project_unhandled_events(100)
        .await
        .unwrap();
    let snapshot = departure_first.store.load_agent_snapshot().await.unwrap();
    assert_eq!(snapshot.panes.len(), 1);
    assert_eq!(snapshot.panes[0].pane.incarnation, replacement.incarnation);
    assert_eq!(snapshot.panes[0].effective_state, EffectiveAgentState::Busy);
}

#[tokio::test]
async fn changed_screen_facts_append_independently_and_only_advance_after_success() {
    let observation = pane("%8", 80, 800, 1_000, 100);
    let fixture = Fixture::new(FakeInventory::scans(vec![Ok(vec![observation.clone()])])).await;
    fixture.handle.inventory_tick().await.unwrap();

    fixture.screen.push(Ok(screen(
        &observation,
        Some(ObservedAgentState::Busy),
        None,
        1_000,
    )));
    fixture.handle.screen_tick().await.unwrap();
    fixture
        .screen
        .push(Ok(screen(&observation, None, Some("task a"), 1_100)));
    fixture.handle.screen_tick().await.unwrap();
    fixture.screen.push(Ok(screen(
        &observation,
        Some(ObservedAgentState::Busy),
        Some("task a"),
        1_200,
    )));
    fixture.handle.screen_tick().await.unwrap();
    fixture
        .screen
        .push(Ok(screen(&observation, None, None, 1_300)));
    fixture.handle.screen_tick().await.unwrap();

    fixture.store.fail_next_monitor_append_for_test();
    fixture.screen.push(Ok(screen(
        &observation,
        Some(ObservedAgentState::Idle),
        Some("task b"),
        1_400,
    )));
    assert!(fixture.handle.screen_tick().await.is_err());
    fixture.screen.push(Ok(screen(
        &observation,
        Some(ObservedAgentState::Idle),
        Some("task b"),
        1_500,
    )));
    fixture.handle.screen_tick().await.unwrap();

    let events = fixture.events().await;
    assert_eq!(
        event_types(&events),
        [
            "AgentPaneObserved",
            "AgentScreenObserved",
            "AgentScreenObserved",
            "AgentScreenObserved"
        ]
    );
    let screen_events: Vec<super::domain::AgentScreenObserved> = events
        .iter()
        .skip(1)
        .map(|event| serde_json::from_value(event.payload.clone()).unwrap())
        .collect();
    assert_eq!(
        (
            screen_events[0].state,
            screen_events[0].fallback_summary.as_deref()
        ),
        (Some(ObservedAgentState::Busy), None)
    );
    assert_eq!(
        (
            screen_events[1].state,
            screen_events[1].fallback_summary.as_deref()
        ),
        (None, Some("task a"))
    );
    assert_eq!(
        (
            screen_events[2].state,
            screen_events[2].fallback_summary.as_deref()
        ),
        (Some(ObservedAgentState::Idle), Some("task b"))
    );
}

#[tokio::test]
async fn configured_idle_placeholder_never_replaces_a_durable_screen_summary() {
    let directory = TestDirectory::new();
    let store = Arc::new(HaroldStore::open(&directory.0).await.unwrap());
    let observation = pane("%8", 80, 800, 1_000, 100);
    let inventory = Arc::new(FakeInventory::scans(vec![Ok(vec![observation.clone()])]));
    let screen_port = Arc::new(FakeScreen::default());
    screen_port.push(Ok(screen(
        &observation,
        Some(ObservedAgentState::Busy),
        Some("Review durable state"),
        200,
    )));
    screen_port.push(Ok(screen(
        &observation,
        Some(ObservedAgentState::Idle),
        Some("Ask Codex to do anything"),
        300,
    )));
    let mut codex = provider();
    codex.idle_all = vec!["Ask Codex to do anything".into()];
    let (shutdown, shutdown_rx) = watch::channel(());
    let (handle, task) = spawn_agent_monitor_for_test(
        Arc::clone(&store),
        inventory,
        screen_port,
        vec![codex],
        2_000,
        shutdown_rx,
    );

    handle.inventory_tick().await.unwrap();
    handle.screen_tick().await.unwrap();
    handle.screen_tick().await.unwrap();

    let events = store
        .stream()
        .load_after_version(EventStreamVersion::start(), 100)
        .await
        .unwrap();
    assert!(events.iter().all(|event| {
        !serde_json::to_string(&event.payload)
            .unwrap()
            .contains("Ask Codex to do anything")
    }));
    let latest_screen: super::domain::AgentScreenObserved =
        serde_json::from_value(events.last().unwrap().payload.clone()).unwrap();
    assert_eq!(latest_screen.state, Some(ObservedAgentState::Idle));
    assert_eq!(latest_screen.fallback_summary, None);

    store.project_unhandled_events(100).await.unwrap();
    let snapshot = store.load_agent_snapshot().await.unwrap();
    assert_eq!(snapshot.panes[0].effective_state, EffectiveAgentState::Idle);
    assert_eq!(
        snapshot.panes[0].screen_work_summary.as_deref(),
        Some("Review durable state")
    );
    assert_eq!(
        snapshot.panes[0].work_summary.as_deref(),
        Some("Review durable state")
    );

    drop(shutdown);
    task.await.unwrap();
}

#[tokio::test]
async fn runtime_keeps_a_normalized_typed_summary_that_mentions_the_idle_placeholder() {
    let directory = TestDirectory::new();
    let store = Arc::new(HaroldStore::open(&directory.0).await.unwrap());
    let observation = pane("%8", 80, 800, 1_000, 100);
    let inventory = Arc::new(FakeInventory::scans(vec![Ok(vec![observation.clone()])]));
    let screen_port = Arc::new(FakeScreen::default());
    screen_port.push(Ok(screen(
        &observation,
        None,
        Some(" \u{1b}[31mExplain why the UI says Ask Codex to do anything\u{1b}[0m "),
        200,
    )));
    let mut codex = provider();
    codex.idle_all = vec!["Ask Codex to do anything".into()];
    let (shutdown, shutdown_rx) = watch::channel(());
    let (handle, task) = spawn_agent_monitor_for_test(
        Arc::clone(&store),
        inventory,
        screen_port,
        vec![codex],
        2_000,
        shutdown_rx,
    );

    handle.inventory_tick().await.unwrap();
    handle.screen_tick().await.unwrap();

    let events = store
        .stream()
        .load_after_version(EventStreamVersion::start(), 100)
        .await
        .unwrap();
    assert_eq!(
        event_types(&events),
        ["AgentPaneObserved", "AgentScreenObserved"]
    );
    let screen: super::domain::AgentScreenObserved =
        serde_json::from_value(events[1].payload.clone()).unwrap();
    assert_eq!(
        screen.fallback_summary.as_deref(),
        Some("Explain why the UI says Ask Codex to do anything")
    );

    drop(shutdown);
    task.await.unwrap();
}

#[tokio::test]
async fn lifecycle_epoch_holds_conflicting_screen_state_but_allows_summary_then_repairs_once() {
    let observation = pane("%8", 80, 800, 1_000, 100);
    let fixture = Fixture::new(FakeInventory::scans(vec![Ok(vec![observation.clone()])])).await;
    fixture.handle.inventory_tick().await.unwrap();
    fixture
        .inventory
        .push_resolution(Ok(Some(observation.clone())));
    fixture
        .handle
        .report_lifecycle(
            "%8".into(),
            ObservedAgentState::Busy,
            "codex-hook".into(),
            WorkSummaryUpdate::Unchanged,
        )
        .await
        .unwrap();

    fixture.screen.push(Ok(screen(
        &observation,
        Some(ObservedAgentState::Idle),
        Some("fallback"),
        1_500,
    )));
    fixture.handle.screen_tick().await.unwrap();
    fixture.screen.push(Ok(screen(
        &observation,
        Some(ObservedAgentState::Idle),
        Some("fallback"),
        3_000,
    )));
    fixture.handle.screen_tick().await.unwrap();
    fixture.screen.push(Ok(screen(
        &observation,
        Some(ObservedAgentState::Idle),
        Some("fallback"),
        3_100,
    )));
    fixture.handle.screen_tick().await.unwrap();

    let events = fixture.events().await;
    assert_eq!(
        event_types(&events),
        [
            "AgentPaneObserved",
            "AgentPaneObserved",
            "AgentLifecycleObserved",
            "AgentScreenObserved",
            "AgentScreenObserved",
        ]
    );
    let during_grace: super::domain::AgentScreenObserved =
        serde_json::from_value(events[3].payload.clone()).unwrap();
    assert_eq!(during_grace.state, None);
    assert_eq!(during_grace.fallback_summary.as_deref(), Some("fallback"));
    let after_grace: super::domain::AgentScreenObserved =
        serde_json::from_value(events[4].payload.clone()).unwrap();
    assert_eq!(after_grace.state, Some(ObservedAgentState::Idle));
    assert_eq!(after_grace.fallback_summary, None);
}

#[tokio::test]
async fn metadata_refresh_preserves_the_incarnations_reconciliation_epoch() {
    let original = pane("%8", 80, 800, 1_000, 100);
    let mut refreshed = original.clone();
    refreshed.working_directory = "/work/harold-next".into();
    refreshed.observed_at_ms = 200;
    let fixture = Fixture::new(FakeInventory::scans(vec![Ok(vec![refreshed])])).await;
    fixture
        .inventory
        .push_resolution(Ok(Some(original.clone())));
    fixture
        .handle
        .report_lifecycle(
            "%8".into(),
            ObservedAgentState::Busy,
            "codex-hook".into(),
            WorkSummaryUpdate::Unchanged,
        )
        .await
        .unwrap();
    fixture.handle.inventory_tick().await.unwrap();
    fixture.screen.push(Ok(screen(
        &original,
        Some(ObservedAgentState::Idle),
        Some("fallback"),
        1_500,
    )));
    fixture.handle.screen_tick().await.unwrap();

    let events = fixture.events().await;
    let screen: super::domain::AgentScreenObserved =
        serde_json::from_value(events.last().unwrap().payload.clone()).unwrap();
    assert_eq!(screen.state, None);
    assert_eq!(screen.fallback_summary.as_deref(), Some("fallback"));
}

#[tokio::test]
async fn real_screen_adapter_keeps_unrelated_capture_out_of_every_published_boundary() {
    let directory = TestDirectory::new();
    let store = Arc::new(HaroldStore::open(&directory.0).await.unwrap());
    let observed = pane("%8", 80, 800, 1_000, 100);
    let inventory = Arc::new(FakeInventory::scans(vec![Ok(vec![observed.clone()])]));
    let sentinel = ["RAW", "_SCREEN_", "TASK11_", "PRIVATE_", "7F3A"].concat();
    let raw_capture = format!("{sentinel}\nWorking\nesc to interrupt\nTask:   Review\t tests  \n");
    let screen = Arc::new(TmuxVisibleScreen::with_runner(
        CapturedScreenRunner {
            outputs: Mutex::new(
                [
                    CommandOutput {
                        success: true,
                        stdout: raw_capture.as_bytes().to_vec(),
                    },
                    CommandOutput {
                        success: false,
                        stdout: raw_capture.into_bytes(),
                    },
                ]
                .into(),
            ),
        },
        || 110,
    ));
    let (shutdown, shutdown_rx) = watch::channel(());
    let (handle, monitor_task) = spawn_agent_monitor_for_test(
        Arc::clone(&store),
        inventory,
        screen,
        vec![provider()],
        2_000,
        shutdown_rx.clone(),
    );
    let snapshots = AgentSnapshotHub::new(empty_snapshot());
    let service = HaroldService {
        monitor: handle.clone(),
        snapshots: snapshots.clone(),
        shutdown: shutdown_rx,
    };
    let captured_logs = Arc::new(Mutex::new(Vec::new()));
    let log_writer = Arc::clone(&captured_logs);
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_writer(move || CapturedLogWriter(Arc::clone(&log_writer)))
        .finish();
    let _subscriber_guard = tracing::subscriber::set_default(subscriber);

    handle.inventory_tick().await.unwrap();
    handle.screen_tick().await.unwrap();
    handle.screen_tick().await.unwrap();
    let error = service
        .report_agent_state(Request::new(ReportAgentStateRequest {
            pane_id: "not-a-pane".into(),
            state: AgentState::Busy.into(),
            adapter_id: "codex-hook".into(),
            work_summary: Some(sentinel.clone()),
        }))
        .await
        .unwrap_err();
    assert!(!format!("{error:?}").contains(&sentinel));

    let events = store
        .stream()
        .load_after_version(EventStreamVersion::start(), 100)
        .await
        .unwrap();
    assert_eq!(
        event_types(&events),
        [
            "AgentPaneObserved",
            "AgentScreenObserved",
            "AgentMonitorHealthChanged"
        ]
    );
    let screen_event: super::domain::AgentScreenObserved =
        serde_json::from_value(events[1].payload.clone()).unwrap();
    assert_eq!(screen_event.state, Some(ObservedAgentState::Busy));
    assert_eq!(
        screen_event.fallback_summary.as_deref(),
        Some("Review tests")
    );
    let health: super::domain::AgentMonitorHealthChanged =
        serde_json::from_value(events[2].payload.clone()).unwrap();
    assert_eq!(health.reason_code, "capture_failed");
    assert!(events.iter().all(|event| {
        !serde_json::to_string(&event.payload)
            .expect("event payload serializes")
            .contains(&sentinel)
    }));

    let mut published = snapshots.subscribe();
    let (handler_shutdown, handler_shutdown_rx) = watch::channel(());
    let event_handler = tokio::spawn(crate::projector::run_event_handler(
        Arc::clone(&store),
        snapshots,
        handler_shutdown_rx,
    ));
    tokio::time::timeout(Duration::from_secs(1), async {
        while published.borrow().through_event_version.get() < 3 {
            published
                .changed()
                .await
                .expect("snapshot hub remains open");
        }
    })
    .await
    .expect("projector did not publish the screen observation");

    let stored_snapshot = store.load_agent_snapshot().await.unwrap();
    assert_eq!(
        stored_snapshot.panes[0].work_summary.as_deref(),
        Some("Review tests")
    );
    assert_eq!(
        stored_snapshot.monitor_health[0].reason_code,
        "capture_failed"
    );
    assert!(!format!("{stored_snapshot:?}").contains(&sentinel));
    let protobuf = crate::map_agent_snapshot(stored_snapshot);
    assert!(!format!("{protobuf:?}").contains(&sentinel));
    assert!(
        !protobuf
            .encode_to_vec()
            .windows(sentinel.len())
            .any(|window| window == sentinel.as_bytes())
    );

    drop(handler_shutdown);
    tokio::time::timeout(Duration::from_secs(1), event_handler)
        .await
        .expect("event handler exceeded shutdown deadline")
        .expect("event handler task panicked");
    let logs = String::from_utf8(captured_logs.lock().expect("captured log lock").clone())
        .expect("logs are utf-8");
    assert!(logs.contains("event handler starting"));
    assert!(!logs.contains(&sentinel));

    drop(service);
    drop(handle);
    drop(shutdown);
    tokio::time::timeout(Duration::from_secs(1), monitor_task)
        .await
        .expect("monitor exceeded shutdown deadline")
        .expect("monitor task panicked");
    drop(published);
    drop(store);
    assert_persisted_files_exclude(&directory.0, sentinel.as_bytes());
}

#[tokio::test]
async fn grpc_fallback_explicit_legacy_clear_and_replacement_converge_in_projection_order() {
    let original = pane("%8", 80, 800, 1_000, 100);
    let explicit_observation = pane("%8", 80, 800, 1_000, 120);
    let completion_observation = pane("%8", 80, 800, 1_000, 130);
    let clear_observation = pane("%8", 80, 800, 1_000, 140);
    let replacement = pane("%8", 80, 801, 2_000, 500);
    let inventory = FakeInventory::scans(vec![
        Ok(vec![original.clone()]),
        Ok(vec![replacement.clone()]),
    ]);
    inventory.push_resolution(Ok(Some(explicit_observation)));
    inventory.push_resolution(Ok(Some(completion_observation)));
    inventory.push_resolution(Ok(Some(clear_observation)));
    let fixture = Fixture::new(inventory).await;
    let service = HaroldService {
        monitor: fixture.handle.clone(),
        snapshots: AgentSnapshotHub::new(empty_snapshot()),
        shutdown: fixture._shutdown.subscribe(),
    };

    fixture.handle.inventory_tick().await.unwrap();
    fixture
        .screen
        .push(Ok(screen(&original, None, Some("  Review\t tests  "), 110)));
    fixture.handle.screen_tick().await.unwrap();
    fixture.store.project_unhandled_events(100).await.unwrap();
    let fallback = fixture.store.load_agent_snapshot().await.unwrap();
    assert_eq!(
        fallback.panes[0].work_summary.as_deref(),
        Some("Review tests")
    );

    service
        .report_agent_state(Request::new(ReportAgentStateRequest {
            pane_id: "%8".into(),
            state: AgentState::Busy.into(),
            adapter_id: "codex-hook".into(),
            work_summary: Some("  Fix\n projector  ".into()),
        }))
        .await
        .unwrap();
    fixture.store.project_unhandled_events(100).await.unwrap();
    let explicit = fixture.store.load_agent_snapshot().await.unwrap();
    assert_eq!(explicit.panes[0].effective_state, EffectiveAgentState::Busy);
    assert_eq!(
        explicit.panes[0].work_summary.as_deref(),
        Some("Fix projector")
    );
    assert_eq!(
        explicit.panes[0].screen_work_summary.as_deref(),
        Some("Review tests")
    );
    assert_eq!(
        explicit.panes[0].screen_work_summary_updated_at_ms,
        Some(110)
    );
    assert_eq!(
        explicit.panes[0].explicit_work_summary_updated_at_ms,
        Some(120)
    );

    service
        .turn_complete(Request::new(TurnCompleteRequest {
            pane_id: "%8".into(),
            pane_label: "harold:0.8".into(),
            last_user_prompt: " \u{1b}[31m\t ".into(),
            assistant_message: "assistant result".into(),
            main_context: "harold".into(),
        }))
        .await
        .unwrap();
    fixture.store.project_unhandled_events(100).await.unwrap();
    let preserved = fixture.store.load_agent_snapshot().await.unwrap();
    assert_eq!(
        preserved.panes[0].effective_state,
        EffectiveAgentState::Idle
    );
    assert_eq!(
        preserved.panes[0].work_summary.as_deref(),
        Some("Fix projector")
    );

    service
        .report_agent_state(Request::new(ReportAgentStateRequest {
            pane_id: "%8".into(),
            state: AgentState::Idle.into(),
            adapter_id: "codex-hook".into(),
            work_summary: Some(" \u{1b}[31m\t ".into()),
        }))
        .await
        .unwrap();
    fixture.store.project_unhandled_events(100).await.unwrap();
    let cleared = fixture.store.load_agent_snapshot().await.unwrap();
    assert_eq!(
        cleared.panes[0].work_summary.as_deref(),
        Some("Review tests")
    );
    assert_eq!(cleared.panes[0].explicit_work_summary, None);

    fixture.handle.inventory_tick().await.unwrap();
    fixture.store.project_unhandled_events(100).await.unwrap();
    let replaced = fixture.store.load_agent_snapshot().await.unwrap();
    assert_eq!(replaced.panes.len(), 1);
    assert_eq!(replaced.panes[0].pane.incarnation, replacement.incarnation);
    assert_eq!(
        replaced.panes[0].effective_state,
        EffectiveAgentState::Unknown
    );
    assert_eq!(replaced.panes[0].work_summary, None);
    assert_eq!(replaced.panes[0].explicit_work_summary, None);
    assert_eq!(replaced.panes[0].screen_work_summary, None);
}

#[tokio::test]
async fn restart_seed_departure_requires_two_new_successful_absence_scans() {
    let directory = TestDirectory::new();
    let original_store = HaroldStore::open(&directory.0).await.unwrap();
    let observed = pane("%8", 80, 800, 1_000, 100);
    crate::store::append_agent_events(
        &original_store,
        vec![
            AgentEvent::PaneObserved(AgentPaneObserved {
                pane: observed.clone(),
            }),
            AgentEvent::LifecycleObserved(AgentLifecycleObserved {
                incarnation: observed.incarnation.clone(),
                state: ObservedAgentState::Busy,
                adapter_id: "codex-hook".into(),
                work_summary: WorkSummaryUpdate::Set("restart task".into()),
                observed_at_ms: 100,
            }),
        ],
    )
    .await
    .unwrap();
    original_store.project_unhandled_events(100).await.unwrap();
    drop(original_store);

    let store = Arc::new(HaroldStore::open(&directory.0).await.unwrap());
    let snapshot = store.load_agent_snapshot().await.unwrap();
    let inventory = Arc::new(FakeInventory::scans(vec![Ok(Vec::new()), Ok(Vec::new())]));
    inventory.push_current(Ok(false));
    let screen = Arc::new(FakeScreen::default());
    let (shutdown, shutdown_rx) = watch::channel(());
    let (handle, task) = spawn_agent_monitor_seeded_for_test(
        Arc::clone(&store),
        inventory,
        screen,
        vec![provider()],
        AgentMonitorSeed {
            snapshot,
            hook_grace_ms: 2_000,
            acquisition_timeout: Duration::from_millis(50),
        },
        shutdown_rx,
    );

    handle.inventory_tick().await.unwrap();
    assert_eq!(
        store
            .stream()
            .load_after_version(EventStreamVersion::start(), 100)
            .await
            .unwrap()
            .len(),
        2
    );
    handle.inventory_tick().await.unwrap();
    let events = store
        .stream()
        .load_after_version(EventStreamVersion::start(), 100)
        .await
        .unwrap();
    assert_eq!(events.last().unwrap().r#type, "AgentPaneDeparted");
    drop(shutdown);
    task.await.unwrap();
}

#[tokio::test]
async fn restart_seed_retains_screen_dedupe_for_a_live_incarnation() {
    let directory = TestDirectory::new();
    let store = Arc::new(HaroldStore::open(&directory.0).await.unwrap());
    let observed = pane("%8", 80, 800, 1_000, 100);
    crate::store::append_agent_events(
        &store,
        vec![
            AgentEvent::PaneObserved(AgentPaneObserved {
                pane: observed.clone(),
            }),
            AgentEvent::ScreenObserved(super::domain::AgentScreenObserved {
                incarnation: observed.incarnation.clone(),
                state: Some(ObservedAgentState::Busy),
                classifier_id: "tmux-visible-v1".into(),
                fallback_summary: Some("seeded task".into()),
                observed_at_ms: 200,
            }),
        ],
    )
    .await
    .unwrap();
    store.project_unhandled_events(100).await.unwrap();
    let snapshot = store.load_agent_snapshot().await.unwrap();
    let inventory = Arc::new(FakeInventory::default());
    let screen_port = Arc::new(FakeScreen::default());
    screen_port.push(Ok(screen(
        &observed,
        Some(ObservedAgentState::Busy),
        Some("seeded task"),
        300,
    )));
    let (shutdown, shutdown_rx) = watch::channel(());
    let (handle, task) = spawn_agent_monitor_seeded_for_test(
        Arc::clone(&store),
        inventory,
        screen_port,
        vec![provider()],
        AgentMonitorSeed {
            snapshot,
            hook_grace_ms: 2_000,
            acquisition_timeout: Duration::from_millis(50),
        },
        shutdown_rx,
    );
    handle.screen_tick().await.unwrap();
    let events = store
        .stream()
        .load_after_version(EventStreamVersion::start(), 100)
        .await
        .unwrap();
    assert_eq!(events.len(), 2);
    drop(shutdown);
    task.await.unwrap();
}

#[tokio::test]
async fn restart_seed_repairs_exact_configured_placeholders_once_and_retries_failed_append() {
    const PLACEHOLDER: &str = "Ask Codex to do anything";

    let directory = TestDirectory::new();
    let store = Arc::new(HaroldStore::open(&directory.0).await.unwrap());
    let observed = pane("%8", 80, 800, 1_000, 100);
    crate::store::append_agent_events(
        &store,
        vec![
            AgentEvent::PaneObserved(AgentPaneObserved {
                pane: observed.clone(),
            }),
            AgentEvent::LifecycleObserved(AgentLifecycleObserved {
                incarnation: observed.incarnation.clone(),
                state: ObservedAgentState::Idle,
                adapter_id: "legacy-hook".into(),
                work_summary: WorkSummaryUpdate::Set(PLACEHOLDER.into()),
                observed_at_ms: 110,
            }),
            AgentEvent::ScreenObserved(super::domain::AgentScreenObserved {
                incarnation: observed.incarnation.clone(),
                state: None,
                classifier_id: "legacy-screen".into(),
                fallback_summary: Some(PLACEHOLDER.into()),
                observed_at_ms: 120,
            }),
        ],
    )
    .await
    .unwrap();
    store.project_unhandled_events(100).await.unwrap();
    let snapshot = store.load_agent_snapshot().await.unwrap();
    assert_eq!(snapshot.panes[0].work_summary.as_deref(), Some(PLACEHOLDER));

    let inventory = Arc::new(FakeInventory::scans(vec![
        Ok(vec![observed.clone()]),
        Ok(vec![observed.clone()]),
        Ok(vec![observed]),
    ]));
    let mut codex = provider();
    codex.idle_all = vec![format!(" \u{1b}[31m{PLACEHOLDER}\u{1b}[0m ")];
    let (shutdown, shutdown_rx) = watch::channel(());
    let (handle, task) = spawn_agent_monitor_seeded_for_test(
        Arc::clone(&store),
        inventory,
        Arc::new(FakeScreen::default()),
        vec![codex],
        AgentMonitorSeed {
            snapshot,
            hook_grace_ms: 2_000,
            acquisition_timeout: Duration::from_millis(50),
        },
        shutdown_rx,
    );

    store.fail_next_monitor_append_for_test();
    assert!(matches!(
        handle.inventory_tick().await,
        Err(MonitorCommandError::EventAppend(_))
    ));
    assert_eq!(fixture_event_count(&store).await, 3);

    handle.inventory_tick().await.unwrap();
    let events = store
        .stream()
        .load_after_version(EventStreamVersion::start(), 100)
        .await
        .unwrap();
    assert_eq!(
        events.last().unwrap().r#type,
        "AgentWorkSummaryCandidatesRepaired"
    );
    assert_eq!(events.len(), 4);
    let repair_json = serde_json::to_string(&events[3].payload).unwrap();
    assert!(!repair_json.contains(PLACEHOLDER));

    handle.inventory_tick().await.unwrap();
    assert_eq!(fixture_event_count(&store).await, 4);
    store.project_unhandled_events(100).await.unwrap();
    let repaired = store.load_agent_snapshot().await.unwrap();
    assert_eq!(repaired.panes[0].explicit_work_summary, None);
    assert_eq!(repaired.panes[0].screen_work_summary, None);
    assert_eq!(repaired.panes[0].work_summary, None);

    drop(shutdown);
    task.await.unwrap();
}

#[tokio::test]
async fn restart_seed_never_repairs_a_legitimate_summary_containing_the_placeholder() {
    const LEGITIMATE: &str = "Explain why the UI says Ask Codex to do anything";

    let directory = TestDirectory::new();
    let store = Arc::new(HaroldStore::open(&directory.0).await.unwrap());
    let observed = pane("%8", 80, 800, 1_000, 100);
    crate::store::append_agent_events(
        &store,
        vec![
            AgentEvent::PaneObserved(AgentPaneObserved {
                pane: observed.clone(),
            }),
            AgentEvent::LifecycleObserved(AgentLifecycleObserved {
                incarnation: observed.incarnation.clone(),
                state: ObservedAgentState::Busy,
                adapter_id: "legacy-hook".into(),
                work_summary: WorkSummaryUpdate::Set(LEGITIMATE.into()),
                observed_at_ms: 110,
            }),
        ],
    )
    .await
    .unwrap();
    store.project_unhandled_events(100).await.unwrap();
    let snapshot = store.load_agent_snapshot().await.unwrap();

    let inventory = Arc::new(FakeInventory::scans(vec![Ok(vec![observed])]));
    let mut codex = provider();
    codex.idle_all = vec!["Ask Codex to do anything".into()];
    let (shutdown, shutdown_rx) = watch::channel(());
    let (handle, task) = spawn_agent_monitor_seeded_for_test(
        Arc::clone(&store),
        inventory,
        Arc::new(FakeScreen::default()),
        vec![codex],
        AgentMonitorSeed {
            snapshot,
            hook_grace_ms: 2_000,
            acquisition_timeout: Duration::from_millis(50),
        },
        shutdown_rx,
    );

    handle.inventory_tick().await.unwrap();
    assert_eq!(fixture_event_count(&store).await, 2);
    store.project_unhandled_events(100).await.unwrap();
    let unchanged = store.load_agent_snapshot().await.unwrap();
    assert_eq!(unchanged.panes[0].work_summary.as_deref(), Some(LEGITIMATE));

    drop(shutdown);
    task.await.unwrap();
    drop(store);

    for suffix in ["", "-shm", "-wal"] {
        let path = directory.0.join(format!("harold-state.db{suffix}"));
        if path.exists() {
            std::fs::remove_file(path).unwrap();
        }
    }
    let replayed = HaroldStore::open(&directory.0).await.unwrap();
    replayed.project_unhandled_events(100).await.unwrap();
    assert_eq!(
        replayed.load_agent_snapshot().await.unwrap().panes[0]
            .work_summary
            .as_deref(),
        Some(LEGITIMATE)
    );
}

#[tokio::test]
async fn restart_seed_revalidates_same_screen_state_once_when_it_predates_lifecycle() {
    let directory = TestDirectory::new();
    let store = Arc::new(HaroldStore::open(&directory.0).await.unwrap());
    let observed = pane("%8", 80, 800, 1_000, 100);
    crate::store::append_agent_events(
        &store,
        vec![
            AgentEvent::PaneObserved(AgentPaneObserved {
                pane: observed.clone(),
            }),
            AgentEvent::ScreenObserved(super::domain::AgentScreenObserved {
                incarnation: observed.incarnation.clone(),
                state: Some(ObservedAgentState::Busy),
                classifier_id: "tmux-visible-v1".into(),
                fallback_summary: None,
                observed_at_ms: 100,
            }),
            AgentEvent::LifecycleObserved(AgentLifecycleObserved {
                incarnation: observed.incarnation.clone(),
                state: ObservedAgentState::Busy,
                adapter_id: "codex-hook".into(),
                work_summary: WorkSummaryUpdate::Unchanged,
                observed_at_ms: 200,
            }),
        ],
    )
    .await
    .unwrap();
    store.project_unhandled_events(100).await.unwrap();
    let snapshot = store.load_agent_snapshot().await.unwrap();
    let screen_port = Arc::new(FakeScreen::default());
    screen_port.push(Ok(screen(
        &observed,
        Some(ObservedAgentState::Busy),
        None,
        2_200,
    )));
    screen_port.push(Ok(screen(
        &observed,
        Some(ObservedAgentState::Busy),
        None,
        2_300,
    )));
    let (shutdown, shutdown_rx) = watch::channel(());
    let (handle, task) = spawn_agent_monitor_seeded_for_test(
        Arc::clone(&store),
        Arc::new(FakeInventory::default()),
        screen_port,
        vec![provider()],
        AgentMonitorSeed {
            snapshot,
            hook_grace_ms: 2_000,
            acquisition_timeout: Duration::from_millis(50),
        },
        shutdown_rx,
    );

    handle.screen_tick().await.unwrap();
    assert_eq!(fixture_event_count(&store).await, 4);
    handle.screen_tick().await.unwrap();
    assert_eq!(fixture_event_count(&store).await, 4);
    drop(shutdown);
    task.await.unwrap();

    store.project_unhandled_events(100).await.unwrap();
    let snapshot = store.load_agent_snapshot().await.unwrap();
    let screen_port = Arc::new(FakeScreen::default());
    screen_port.push(Ok(screen(
        &observed,
        Some(ObservedAgentState::Busy),
        None,
        2_400,
    )));
    let (shutdown, shutdown_rx) = watch::channel(());
    let (handle, task) = spawn_agent_monitor_seeded_for_test(
        Arc::clone(&store),
        Arc::new(FakeInventory::default()),
        screen_port,
        vec![provider()],
        AgentMonitorSeed {
            snapshot,
            hook_grace_ms: 2_000,
            acquisition_timeout: Duration::from_millis(50),
        },
        shutdown_rx,
    );
    handle.screen_tick().await.unwrap();
    assert_eq!(fixture_event_count(&store).await, 4);
    drop(shutdown);
    task.await.unwrap();
}

#[tokio::test]
async fn zero_grace_same_millisecond_lifecycle_revalidates_once_across_two_restarts() {
    let directory = TestDirectory::new();
    let store = Arc::new(
        HaroldStore::open_with_hook_grace(&directory.0, 0)
            .await
            .unwrap(),
    );
    let observed = pane("%8", 80, 800, 1_000, 50);
    crate::store::append_agent_events(
        &store,
        vec![
            AgentEvent::PaneObserved(AgentPaneObserved {
                pane: observed.clone(),
            }),
            AgentEvent::ScreenObserved(super::domain::AgentScreenObserved {
                incarnation: observed.incarnation.clone(),
                state: Some(ObservedAgentState::Idle),
                classifier_id: "tmux-visible-v1".into(),
                fallback_summary: Some("Keep fallback".into()),
                observed_at_ms: 100,
            }),
            AgentEvent::LifecycleObserved(AgentLifecycleObserved {
                incarnation: observed.incarnation.clone(),
                state: ObservedAgentState::Busy,
                adapter_id: "codex-hook".into(),
                work_summary: WorkSummaryUpdate::Unchanged,
                observed_at_ms: 100,
            }),
        ],
    )
    .await
    .unwrap();
    store.project_unhandled_events(100).await.unwrap();
    let snapshot = store.load_agent_snapshot().await.unwrap();
    assert_eq!(snapshot.panes[0].effective_state, EffectiveAgentState::Busy);
    assert_eq!(snapshot.panes[0].screen_state, None);
    assert_eq!(
        snapshot.panes[0].screen_work_summary.as_deref(),
        Some("Keep fallback")
    );

    let screen_port = Arc::new(FakeScreen::default());
    screen_port.push(Ok(screen(
        &observed,
        Some(ObservedAgentState::Idle),
        None,
        100,
    )));
    let (shutdown, shutdown_rx) = watch::channel(());
    let (handle, task) = spawn_agent_monitor_seeded_for_test(
        Arc::clone(&store),
        Arc::new(FakeInventory::default()),
        screen_port,
        vec![provider()],
        AgentMonitorSeed {
            snapshot,
            hook_grace_ms: 0,
            acquisition_timeout: Duration::from_millis(50),
        },
        shutdown_rx,
    );
    handle.screen_tick().await.unwrap();
    assert_eq!(fixture_event_count(&store).await, 4);
    drop(shutdown);
    task.await.unwrap();

    store.project_unhandled_events(100).await.unwrap();
    let snapshot = store.load_agent_snapshot().await.unwrap();
    let screen_port = Arc::new(FakeScreen::default());
    screen_port.push(Ok(screen(
        &observed,
        Some(ObservedAgentState::Idle),
        None,
        100,
    )));
    let (shutdown, shutdown_rx) = watch::channel(());
    let (handle, task) = spawn_agent_monitor_seeded_for_test(
        Arc::clone(&store),
        Arc::new(FakeInventory::default()),
        screen_port,
        vec![provider()],
        AgentMonitorSeed {
            snapshot,
            hook_grace_ms: 0,
            acquisition_timeout: Duration::from_millis(50),
        },
        shutdown_rx,
    );
    handle.screen_tick().await.unwrap();
    assert_eq!(fixture_event_count(&store).await, 4);
    drop(shutdown);
    task.await.unwrap();
}

#[tokio::test]
async fn restart_seed_compares_normalized_metadata_with_the_next_raw_scan() {
    let directory = TestDirectory::new();
    let store = Arc::new(HaroldStore::open(&directory.0).await.unwrap());
    let mut raw = pane("%8", 80, 800, 1_000, 100);
    raw.tmux_target = format!("session  target\u{1b}]0;secret\u{7}{}", "t".repeat(300));
    raw.session_name = format!("session  name\u{90}secret\u{9c}{}", "s".repeat(300));
    raw.provider_display_name = format!("Provider  Name{}", "p".repeat(300));
    raw.working_directory = format!("/path  with  spaces/{}", "w".repeat(1_100));
    crate::store::append_agent_events(
        &store,
        vec![AgentEvent::PaneObserved(AgentPaneObserved {
            pane: raw.clone(),
        })],
    )
    .await
    .unwrap();
    store.project_unhandled_events(100).await.unwrap();
    let snapshot = store.load_agent_snapshot().await.unwrap();
    let inventory = Arc::new(FakeInventory::scans(vec![Ok(vec![raw])]));
    let (shutdown, shutdown_rx) = watch::channel(());
    let (handle, task) = spawn_agent_monitor_seeded_for_test(
        Arc::clone(&store),
        inventory,
        Arc::new(FakeScreen::default()),
        vec![provider()],
        AgentMonitorSeed {
            snapshot,
            hook_grace_ms: 2_000,
            acquisition_timeout: Duration::from_millis(50),
        },
        shutdown_rx,
    );

    handle.inventory_tick().await.unwrap();
    assert_eq!(fixture_event_count(&store).await, 1);
    drop(shutdown);
    task.await.unwrap();
}

#[tokio::test]
async fn restart_seed_preserves_one_pending_post_grace_screen_repair() {
    let directory = TestDirectory::new();
    let store = Arc::new(HaroldStore::open(&directory.0).await.unwrap());
    let observed = pane("%8", 80, 800, 1_000, 100);
    crate::store::append_agent_events(
        &store,
        vec![
            AgentEvent::PaneObserved(AgentPaneObserved {
                pane: observed.clone(),
            }),
            AgentEvent::LifecycleObserved(AgentLifecycleObserved {
                incarnation: observed.incarnation.clone(),
                state: ObservedAgentState::Busy,
                adapter_id: "codex-hook".into(),
                work_summary: WorkSummaryUpdate::Unchanged,
                observed_at_ms: 100,
            }),
            AgentEvent::ScreenObserved(super::domain::AgentScreenObserved {
                incarnation: observed.incarnation.clone(),
                state: Some(ObservedAgentState::Idle),
                classifier_id: "tmux-visible-v1".into(),
                fallback_summary: None,
                observed_at_ms: 1_500,
            }),
        ],
    )
    .await
    .unwrap();
    store.project_unhandled_events(100).await.unwrap();
    let snapshot = store.load_agent_snapshot().await.unwrap();
    assert_eq!(snapshot.panes[0].effective_state, EffectiveAgentState::Busy);
    let screen_port = Arc::new(FakeScreen::default());
    screen_port.push(Ok(screen(
        &observed,
        Some(ObservedAgentState::Idle),
        None,
        2_100,
    )));
    let (shutdown, shutdown_rx) = watch::channel(());
    let (handle, task) = spawn_agent_monitor_seeded_for_test(
        Arc::clone(&store),
        Arc::new(FakeInventory::default()),
        screen_port,
        vec![provider()],
        AgentMonitorSeed {
            snapshot,
            hook_grace_ms: 2_000,
            acquisition_timeout: Duration::from_millis(50),
        },
        shutdown_rx,
    );
    handle.screen_tick().await.unwrap();
    let events = store
        .stream()
        .load_after_version(EventStreamVersion::start(), 100)
        .await
        .unwrap();
    assert_eq!(events.last().unwrap().r#type, "AgentScreenObserved");
    assert_eq!(events.len(), 4);
    drop(shutdown);
    task.await.unwrap();
}

struct WedgedInventory {
    scan_calls: AtomicUsize,
}

impl AgentInventoryPort for WedgedInventory {
    fn scan(&self) -> Result<Vec<AgentPaneObservation>, InventoryError> {
        self.scan_calls.fetch_add(1, Ordering::SeqCst);
        loop {
            std::thread::park();
        }
    }

    fn resolve(&self, _pane_id: &str) -> Result<Option<AgentPaneObservation>, InventoryError> {
        Ok(None)
    }

    fn is_current(&self, _incarnation: &AgentIncarnation) -> Result<bool, InventoryError> {
        Ok(false)
    }
}

#[tokio::test]
async fn shutdown_completes_when_a_blocking_inventory_port_wedges() {
    let directory = TestDirectory::new();
    let store = Arc::new(HaroldStore::open(&directory.0).await.unwrap());
    let inventory = Arc::new(WedgedInventory {
        scan_calls: AtomicUsize::new(0),
    });
    let (shutdown, shutdown_rx) = watch::channel(());
    let (handle, task) = spawn_agent_monitor_seeded_for_test(
        store,
        Arc::clone(&inventory),
        Arc::new(FakeScreen::default()),
        vec![provider()],
        AgentMonitorSeed {
            snapshot: empty_snapshot(),
            hook_grace_ms: 2_000,
            acquisition_timeout: Duration::from_millis(40),
        },
        shutdown_rx,
    );
    assert!(handle.inventory_tick().await.is_err());
    assert!(handle.inventory_tick().await.is_err());
    assert_eq!(inventory.scan_calls.load(Ordering::SeqCst), 1);
    tokio::time::sleep(Duration::from_millis(10)).await;
    drop(shutdown);

    tokio::time::timeout(Duration::from_millis(250), task)
        .await
        .expect("runtime shutdown remained blocked")
        .unwrap();
}

#[tokio::test]
async fn inventory_health_degrades_recovers_and_deduplicates_without_losing_panes() {
    let observed = pane("%8", 80, 800, 1_000, 100);
    let fixture = Fixture::new(FakeInventory::scans(vec![
        Ok(vec![observed]),
        Err(InventoryError::CommandFailed),
        Err(InventoryError::CommandFailed),
        Ok(Vec::new()),
    ]))
    .await;
    fixture.handle.inventory_tick().await.unwrap();
    assert!(fixture.handle.inventory_tick().await.is_err());
    assert!(fixture.handle.inventory_tick().await.is_err());
    fixture.handle.inventory_tick().await.unwrap();

    fixture.store.project_unhandled_events(100).await.unwrap();
    let snapshot = fixture.store.load_agent_snapshot().await.unwrap();
    assert_eq!(snapshot.panes.len(), 1);
    let inventory_health: Vec<_> = snapshot
        .monitor_health
        .iter()
        .filter(|health| health.component == "inventory")
        .collect();
    assert_eq!(inventory_health.len(), 1);
    assert!(inventory_health[0].healthy);
    assert_eq!(inventory_health[0].reason_code, "ok");
    assert_eq!(
        event_types(&fixture.events().await)
            .into_iter()
            .filter(|event_type| *event_type == "AgentMonitorHealthChanged")
            .count(),
        2
    );
}

#[tokio::test]
async fn failed_health_append_is_retried_without_advancing_health_dedupe() {
    let fixture = Fixture::new(FakeInventory::scans(vec![
        Err(InventoryError::MalformedOutput),
        Err(InventoryError::MalformedOutput),
    ]))
    .await;
    fixture.store.fail_next_monitor_append_for_test();
    assert!(matches!(
        fixture.handle.inventory_tick().await,
        Err(MonitorCommandError::EventAppend(_))
    ));
    assert!(matches!(
        fixture.handle.inventory_tick().await,
        Err(MonitorCommandError::InventoryUnavailable)
    ));
    let events = fixture.events().await;
    assert_eq!(event_types(&events), ["AgentMonitorHealthChanged"]);
    let health: super::domain::AgentMonitorHealthChanged =
        serde_json::from_value(events[0].payload.clone()).unwrap();
    assert_eq!(health.component, "inventory");
    assert!(!health.healthy);
    assert_eq!(health.reason_code, "malformed_output");
}

#[tokio::test]
async fn screen_health_degrades_recovers_and_deduplicates_without_raw_errors() {
    let observed = pane("%8", 80, 800, 1_000, 100);
    let fixture = Fixture::new(FakeInventory::scans(vec![Ok(vec![observed.clone()])])).await;
    fixture.handle.inventory_tick().await.unwrap();
    fixture.screen.push(Err(ScreenError::CaptureFailed));
    fixture.screen.push(Err(ScreenError::CaptureFailed));
    fixture.screen.push(Ok(screen(
        &observed,
        Some(ObservedAgentState::Busy),
        None,
        200,
    )));
    fixture.handle.screen_tick().await.unwrap();
    fixture.handle.screen_tick().await.unwrap();
    fixture.handle.screen_tick().await.unwrap();

    fixture.store.project_unhandled_events(100).await.unwrap();
    let snapshot = fixture.store.load_agent_snapshot().await.unwrap();
    let health = snapshot
        .monitor_health
        .iter()
        .find(|health| health.component == "screen")
        .unwrap();
    assert!(health.healthy);
    assert_eq!(health.reason_code, "ok");
    assert_eq!(
        event_types(&fixture.events().await)
            .into_iter()
            .filter(|event_type| *event_type == "AgentMonitorHealthChanged")
            .count(),
        2
    );
}

#[tokio::test]
async fn pane_metadata_is_sanitized_and_scalar_bounded_for_inventory_and_completion() {
    let mut unsafe_pane = pane("%8", 80, 800, 1_000, 100);
    let sentinel = "RAW_SCREEN_SECRET";
    unsafe_pane.tmux_target = format!("\u{1b}]0;{sentinel}\u{7}{}", "t".repeat(300));
    unsafe_pane.session_name = format!("\u{9d}{sentinel}\u{9c}{}", "s".repeat(300));
    unsafe_pane.provider_display_name = format!("\u{1b}P{sentinel}\u{1b}\\{}", "p".repeat(300));
    unsafe_pane.working_directory = format!("/\u{90}{sentinel}\u{9c}{}", "w".repeat(1_100));
    let fixture = Fixture::new(FakeInventory::scans(vec![Ok(vec![unsafe_pane.clone()])])).await;
    fixture.handle.inventory_tick().await.unwrap();
    fixture.inventory.push_resolution(Ok(Some(unsafe_pane)));
    fixture
        .handle
        .turn_completed(turn("safe task"))
        .await
        .unwrap();

    let events = fixture.events().await;
    for event in events
        .iter()
        .filter(|event| event.r#type == "AgentPaneObserved")
    {
        let pane: AgentPaneObserved = serde_json::from_value(event.payload.clone()).unwrap();
        assert!(pane.pane.tmux_target.chars().count() <= 256);
        assert!(pane.pane.session_name.chars().count() <= 256);
        assert!(pane.pane.provider_display_name.chars().count() <= 256);
        assert!(pane.pane.working_directory.chars().count() <= 1_024);
        assert!(!serde_json::to_string(&pane).unwrap().contains(sentinel));
    }
}

#[tokio::test]
async fn interval_tick_channel_coalesces_without_waiting_or_filling_ingress() {
    let (tick, mut receiver) = coalesced_tick_channel_for_test();
    assert!(tick.try_enqueue());
    assert!(!tick.try_enqueue());
    receiver.recv().await.unwrap();
    assert!(tick.try_enqueue());
}

fn empty_snapshot() -> AgentSnapshot {
    AgentSnapshot {
        through_event_version: EventStreamVersion::start(),
        server_time_ms: 0,
        monitor_health: Vec::new(),
        panes: Vec::new(),
    }
}

async fn unprojected_page_boundary_store(
    retained_summary: &str,
) -> (
    TestDirectory,
    Arc<HaroldStore>,
    AgentSnapshot,
    AgentPaneObservation,
) {
    let directory = TestDirectory::new();
    let initial_store = HaroldStore::open(&directory.0).await.unwrap();
    let observation = pane("%8", 80, 800, 1_000, 100);
    let mut history = vec![
        AgentEvent::PaneObserved(AgentPaneObserved {
            pane: observation.clone(),
        }),
        AgentEvent::LifecycleObserved(AgentLifecycleObserved {
            incarnation: observation.incarnation.clone(),
            state: ObservedAgentState::Busy,
            adapter_id: "legacy-hook".into(),
            work_summary: WorkSummaryUpdate::Set(retained_summary.into()),
            observed_at_ms: 101,
        }),
    ];
    history.extend((0..496).map(|offset| {
        let mut pane = observation.clone();
        pane.observed_at_ms = 102 + offset;
        AgentEvent::PaneObserved(AgentPaneObserved { pane })
    }));
    crate::store::append_agent_events(&initial_store, history)
        .await
        .unwrap();
    initial_store.project_unhandled_events(500).await.unwrap();
    let snapshot = initial_store.load_agent_snapshot().await.unwrap();
    assert_eq!(snapshot.through_event_version.get(), 498);
    drop(initial_store);

    for suffix in ["", "-shm", "-wal"] {
        let path = directory.0.join(format!("harold-state.db{suffix}"));
        if path.exists() {
            std::fs::remove_file(path).unwrap();
        }
    }
    let store = Arc::new(HaroldStore::open(&directory.0).await.unwrap());
    (directory, store, snapshot, observation)
}

fn pane(
    pane_id: &str,
    pane_pid: u32,
    agent_pid: u32,
    started_at_ms: i64,
    observed_at_ms: i64,
) -> AgentPaneObservation {
    AgentPaneObservation {
        incarnation: AgentIncarnation {
            pane_id: pane_id.into(),
            pane_pid,
            agent_pid,
            agent_started_at_ms: started_at_ms,
            provider_id: "codex".into(),
        },
        tmux_target: "harold:0.8".into(),
        session_name: "harold".into(),
        window_index: 0,
        pane_index: 8,
        working_directory: "/work/harold".into(),
        provider_display_name: "Codex".into(),
        observed_at_ms,
    }
}

fn screen(
    pane: &AgentPaneObservation,
    state: Option<ObservedAgentState>,
    summary: Option<&str>,
    observed_at_ms: i64,
) -> ScreenObservation {
    ScreenObservation {
        incarnation: pane.incarnation.clone(),
        state,
        fallback_summary: summary.map(str::to_string),
        classifier_id: "tmux-visible-v1".into(),
        observed_at_ms,
    }
}

fn provider() -> AgentProviderSettings {
    AgentProviderSettings {
        id: "codex".into(),
        display_name: "Codex".into(),
        command_contains: vec!["codex".into()],
        busy_all: vec!["Working".into()],
        idle_all: vec!["Ready".into()],
        summary_line_prefixes: vec!["Task: ".into()],
    }
}

fn turn(last_user_prompt: &str) -> TurnCompleted {
    TurnCompleted {
        pane_id: "%8".into(),
        pane_label: "harold:0.8".into(),
        last_user_prompt: last_user_prompt.into(),
        assistant_message: "assistant result".into(),
        main_context: "harold".into(),
        agent_incarnation: None,
        work_summary: CompletionSummaryUpdate::Unchanged,
    }
}

fn event_types(events: &[events::EventEnvelope]) -> Vec<&str> {
    events.iter().map(|event| event.r#type.as_str()).collect()
}
