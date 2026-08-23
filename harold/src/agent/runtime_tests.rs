use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use events::EventStreamVersion;
use tokio::sync::watch;

use super::domain::{
    AgentIncarnation, AgentPaneObservation, CompletionSummaryUpdate, EffectiveAgentState,
    ObservedAgentState, ScreenObservation, WorkSummaryUpdate,
};
use super::inventory::{AgentInventoryPort, InventoryError};
use super::runtime::{AgentMonitorHandle, MonitorCommandError, spawn_agent_monitor_for_test};
use super::screen::{ScreenError, VisibleScreenPort};
use crate::settings::AgentProviderSettings;
use crate::store::{HaroldStore, TurnCompleted};

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
    assert_eq!(
        snapshot.panes[0].work_summary.as_deref(),
        Some("current task")
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
    assert_eq!(event_types(&fixture.events().await), ["AgentPaneObserved"]);
    fixture.handle.inventory_tick().await.unwrap();

    let events = fixture.events().await;
    assert_eq!(
        event_types(&events),
        ["AgentPaneObserved", "AgentPaneDeparted"]
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
