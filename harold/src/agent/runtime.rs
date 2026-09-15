#[path = "activity_scheduler.rs"]
mod activity_scheduler;
#[path = "prompt_checkpoint.rs"]
mod prompt_checkpoint;

use crate::activity_summary::ActivitySummarizer;
use crate::settings::ActivitySummarySettings;
use activity_scheduler::{ActivityScheduler, SummaryJob, SummaryResult};
use events::EventStreamVersion;
use prompt_checkpoint::PromptAcquisitionCheckpoint;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::{Semaphore, mpsc, oneshot, watch};

use crate::settings::AgentProviderSettings;
use crate::store::{self, HaroldStore, TurnCompleted};

use super::domain::{
    AgentEvent, AgentIncarnation, AgentLifecycleObserved, AgentPaneDeparted, AgentPaneObservation,
    AgentPaneObserved, AgentScreenObserved, AgentSnapshot, CompletionSummaryUpdate,
    ObservedAgentState, WorkSummaryUpdate,
};
use super::inventory::{AgentInventoryPort, InventoryError};
use super::screen::{PromptScan, ScreenError, VisibleScreenPort, normalize_fallback_summary};
use super::summary::completion_summary_update;

const COMMAND_CAPACITY: usize = 64;

pub(crate) enum AgentMonitorCommand {
    #[allow(
        dead_code,
        reason = "the ReportAgentState RPC consumes this command in the next ingress slice"
    )]
    ReportLifecycle {
        pane_id: String,
        state: ObservedAgentState,
        adapter_id: String,
        work_summary: WorkSummaryUpdate,
        reply: oneshot::Sender<Result<(), MonitorCommandError>>,
    },
    TurnCompleted {
        turn: TurnCompleted,
        reply: oneshot::Sender<events::Result<events::AppendResult>>,
    },
    #[cfg(test)]
    InventoryTick {
        reply: Option<oneshot::Sender<Result<(), MonitorCommandError>>>,
    },
    #[cfg(test)]
    ScreenTick {
        reply: Option<oneshot::Sender<Result<(), MonitorCommandError>>>,
    },
}

#[derive(Clone)]
pub(crate) struct AgentMonitorHandle {
    sender: mpsc::Sender<AgentMonitorCommand>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum MonitorCommandError {
    #[allow(
        dead_code,
        reason = "the ReportAgentState RPC exposes lifecycle validation in the next ingress slice"
    )]
    #[error("invalid monitor command input")]
    InvalidInput,
    #[error("agent incarnation was not found")]
    AgentNotFound,
    #[error("agent inventory is unavailable")]
    InventoryUnavailable,
    #[error("event append failed: {0}")]
    EventAppend(events::EsError),
    #[allow(
        dead_code,
        reason = "the ReportAgentState RPC maps stopped-runtime replies in the next ingress slice"
    )]
    #[error("agent monitor runtime stopped")]
    RuntimeStopped,
}

impl AgentMonitorHandle {
    #[allow(
        dead_code,
        reason = "the ReportAgentState RPC calls this handle in the next ingress slice"
    )]
    pub(crate) async fn report_lifecycle(
        &self,
        pane_id: String,
        state: ObservedAgentState,
        adapter_id: String,
        work_summary: WorkSummaryUpdate,
    ) -> Result<(), MonitorCommandError> {
        if !valid_pane_id(&pane_id) || !valid_identifier(&adapter_id) {
            return Err(MonitorCommandError::InvalidInput);
        }
        let (reply, response) = oneshot::channel();
        self.sender
            .send(AgentMonitorCommand::ReportLifecycle {
                pane_id,
                state,
                adapter_id,
                work_summary,
                reply,
            })
            .await
            .map_err(|_| MonitorCommandError::RuntimeStopped)?;
        response
            .await
            .map_err(|_| MonitorCommandError::RuntimeStopped)?
    }

    pub(crate) async fn turn_completed(
        &self,
        turn: TurnCompleted,
    ) -> events::Result<events::AppendResult> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(AgentMonitorCommand::TurnCompleted { turn, reply })
            .await
            .map_err(|_| events::EsError::Migration("agent monitor runtime stopped".into()))?;
        response
            .await
            .map_err(|_| events::EsError::Migration("agent monitor runtime stopped".into()))?
    }

    #[cfg(test)]
    pub(crate) async fn inventory_tick(&self) -> Result<(), MonitorCommandError> {
        self.send_tick(true).await
    }

    #[cfg(test)]
    pub(crate) async fn screen_tick(&self) -> Result<(), MonitorCommandError> {
        self.send_tick(false).await
    }

    #[cfg(test)]
    async fn send_tick(&self, inventory: bool) -> Result<(), MonitorCommandError> {
        let (reply, response) = oneshot::channel();
        let command = if inventory {
            AgentMonitorCommand::InventoryTick { reply: Some(reply) }
        } else {
            AgentMonitorCommand::ScreenTick { reply: Some(reply) }
        };
        self.sender
            .send(command)
            .await
            .map_err(|_| MonitorCommandError::RuntimeStopped)?;
        response
            .await
            .map_err(|_| MonitorCommandError::RuntimeStopped)?
    }
}

struct TrackedPane {
    prompt_checkpoint: PromptAcquisitionCheckpoint,
    recovery_failure: Option<&'static str>,
    pane: AgentPaneObservation,
    consecutive_absences: u8,
    last_hook: Option<(ObservedAgentState, i64)>,
    explicit_summary: Option<String>,
    screen_state: Option<ObservedAgentState>,
    screen_summary: Option<String>,
    summary_basis_version: EventStreamVersion,
}

impl TrackedPane {
    fn new(pane: AgentPaneObservation) -> Self {
        Self {
            pane,
            prompt_checkpoint: PromptAcquisitionCheckpoint::default(),
            recovery_failure: None,
            consecutive_absences: 0,
            last_hook: None,
            explicit_summary: None,
            screen_state: None,
            screen_summary: None,
            summary_basis_version: EventStreamVersion::start(),
        }
    }
}

struct AgentMonitorRuntime {
    store: Arc<HaroldStore>,
    inventory: Arc<dyn AgentInventoryPort>,
    screen: Arc<dyn VisibleScreenPort>,
    providers: HashMap<String, AgentProviderSettings>,
    hook_grace_ms: u64,
    acquisition_timeout: Duration,
    inventory_timeout: Duration,
    inventory_gate: Arc<Semaphore>,
    screen_gate: Arc<Semaphore>,
    panes: HashMap<String, TrackedPane>,
    health: HashMap<String, HealthState>,
    logged_health: HashMap<String, HealthState>,
    summaries: Option<ActivityScheduler>,
}

pub(crate) struct AgentMonitorRuntimeConfig {
    pub(crate) activity_summary: Option<(Arc<dyn ActivitySummarizer>, ActivitySummarySettings)>,
    pub(crate) inventory_interval: Duration,
    pub(crate) screen_interval: Duration,
    pub(crate) hook_grace_ms: u64,
    pub(crate) acquisition_timeout: Duration,
    pub(crate) inventory_timeout: Duration,
}

struct RuntimeInputs {
    store: Arc<HaroldStore>,
    inventory: Arc<dyn AgentInventoryPort>,
    screen: Arc<dyn VisibleScreenPort>,
    providers: Vec<AgentProviderSettings>,
    initial_snapshot: AgentSnapshot,
    hook_grace_ms: u64,
    acquisition_timeout: Duration,
    inventory_timeout: Duration,
    intervals: Option<(Duration, Duration)>,
    activity_summary: Option<(Arc<dyn ActivitySummarizer>, ActivitySummarySettings)>,
}

#[cfg(test)]
pub(crate) struct AgentMonitorSeed {
    pub(crate) snapshot: AgentSnapshot,
    pub(crate) hook_grace_ms: u64,
    pub(crate) acquisition_timeout: Duration,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct HealthState {
    healthy: bool,
    reason_code: String,
}

#[derive(Clone, Copy)]
struct AcquisitionFailure {
    reason_code: &'static str,
}

#[derive(Clone)]
struct CoalescedTick {
    sender: mpsc::Sender<()>,
}

enum TickEnqueue {
    Enqueued,
    Coalesced,
    Closed,
}

impl CoalescedTick {
    fn try_enqueue(&self) -> TickEnqueue {
        match self.sender.try_send(()) {
            Ok(()) => TickEnqueue::Enqueued,
            Err(mpsc::error::TrySendError::Full(())) => TickEnqueue::Coalesced,
            Err(mpsc::error::TrySendError::Closed(())) => TickEnqueue::Closed,
        }
    }
}

#[cfg(test)]
pub(crate) struct CoalescedTickTestHandle(CoalescedTick);

#[cfg(test)]
impl CoalescedTickTestHandle {
    pub(crate) fn try_enqueue(&self) -> bool {
        matches!(self.0.try_enqueue(), TickEnqueue::Enqueued)
    }
}

#[cfg(test)]
pub(crate) fn coalesced_tick_channel_for_test() -> (CoalescedTickTestHandle, mpsc::Receiver<()>) {
    let (sender, receiver) = mpsc::channel(1);
    (CoalescedTickTestHandle(CoalescedTick { sender }), receiver)
}

pub(crate) fn spawn_agent_monitor(
    store: Arc<HaroldStore>,
    inventory: Arc<dyn AgentInventoryPort>,
    screen: Arc<dyn VisibleScreenPort>,
    providers: Vec<AgentProviderSettings>,
    initial_snapshot: AgentSnapshot,
    config: AgentMonitorRuntimeConfig,
    shutdown: watch::Receiver<()>,
) -> (AgentMonitorHandle, tokio::task::JoinHandle<()>) {
    spawn_runtime(
        RuntimeInputs {
            store,
            inventory,
            screen,
            providers,
            initial_snapshot,
            hook_grace_ms: config.hook_grace_ms,
            acquisition_timeout: config.acquisition_timeout,
            inventory_timeout: config.inventory_timeout,
            intervals: Some((config.inventory_interval, config.screen_interval)),
            activity_summary: config.activity_summary,
        },
        shutdown,
    )
}

#[cfg(test)]
pub(crate) fn spawn_agent_monitor_for_test<I, S>(
    store: Arc<HaroldStore>,
    inventory: Arc<I>,
    screen: Arc<S>,
    providers: Vec<AgentProviderSettings>,
    hook_grace_ms: u64,
    shutdown: watch::Receiver<()>,
) -> (AgentMonitorHandle, tokio::task::JoinHandle<()>)
where
    I: AgentInventoryPort + 'static,
    S: VisibleScreenPort + 'static,
{
    spawn_runtime(
        RuntimeInputs {
            store,
            inventory,
            screen,
            providers,
            initial_snapshot: empty_snapshot(),
            hook_grace_ms,
            acquisition_timeout: Duration::from_millis(100),
            inventory_timeout: Duration::from_millis(100),
            intervals: None,
            activity_summary: None,
        },
        shutdown,
    )
}

#[cfg(test)]
pub(crate) fn spawn_agent_monitor_seeded_for_test<I, S>(
    store: Arc<HaroldStore>,
    inventory: Arc<I>,
    screen: Arc<S>,
    providers: Vec<AgentProviderSettings>,
    seed: AgentMonitorSeed,
    shutdown: watch::Receiver<()>,
) -> (AgentMonitorHandle, tokio::task::JoinHandle<()>)
where
    I: AgentInventoryPort + 'static,
    S: VisibleScreenPort + 'static,
{
    spawn_runtime(
        RuntimeInputs {
            store,
            inventory,
            screen,
            providers,
            initial_snapshot: seed.snapshot,
            hook_grace_ms: seed.hook_grace_ms,
            acquisition_timeout: seed.acquisition_timeout,
            inventory_timeout: seed.acquisition_timeout,
            intervals: None,
            activity_summary: None,
        },
        shutdown,
    )
}

fn spawn_runtime(
    inputs: RuntimeInputs,
    mut shutdown: watch::Receiver<()>,
) -> (AgentMonitorHandle, tokio::task::JoinHandle<()>) {
    let RuntimeInputs {
        store,
        inventory,
        screen,
        providers,
        initial_snapshot,
        hook_grace_ms,
        acquisition_timeout,
        inventory_timeout,
        intervals,
        activity_summary,
    } = inputs;
    let (sender, mut receiver) = mpsc::channel(COMMAND_CAPACITY);
    let (inventory_tick_sender, mut inventory_ticks) = mpsc::channel(1);
    let (screen_tick_sender, mut screen_ticks) = mpsc::channel(1);
    let scheduled = intervals.is_some();
    if let Some((inventory_interval, screen_interval)) = intervals {
        spawn_tick(
            CoalescedTick {
                sender: inventory_tick_sender,
            },
            inventory_interval,
            shutdown.clone(),
        );
        spawn_tick(
            CoalescedTick {
                sender: screen_tick_sender,
            },
            screen_interval,
            shutdown.clone(),
        );
    }
    let handle = AgentMonitorHandle { sender };
    let task = tokio::spawn(async move {
        let mut runtime = AgentMonitorRuntime {
            store,
            inventory,
            screen,
            providers: providers
                .into_iter()
                .map(|provider| (provider.id.clone(), provider))
                .collect(),
            hook_grace_ms,
            acquisition_timeout,
            inventory_timeout,
            inventory_gate: Arc::new(Semaphore::new(1)),
            screen_gate: Arc::new(Semaphore::new(1)),
            panes: panes_from_snapshot(&initial_snapshot, hook_grace_ms),
            health: health_from_snapshot(&initial_snapshot),
            logged_health: HashMap::new(),
            summaries: activity_summary
                .map(|(provider, settings)| ActivityScheduler::new(provider, settings)),
        };
        let seeded_panes: Vec<_> = runtime.panes.keys().cloned().collect();
        for pane_id in seeded_panes {
            let _ = runtime.baseline_pane(&pane_id).await;
        }
        loop {
            tokio::select! {
                biased;
                _ = shutdown.changed() => break,
                command = receiver.recv() => {
                    let Some(command) = command else { break };
                    runtime.handle(command).await;
                }
                result = next_summary(&mut runtime.summaries) => {
                    runtime.accept_summary(result).await;
                }
                tick = inventory_ticks.recv(), if scheduled => {
                    if tick.is_none() { break; }
                    let _ = runtime.inventory_tick().await;
                }
                tick = screen_ticks.recv(), if scheduled => {
                    if tick.is_none() { break; }
                    let _ = runtime.screen_tick().await;
                }
            }
        }
        if let Some(summaries) = &mut runtime.summaries {
            summaries.shutdown().await;
        }
    });
    (handle, task)
}

fn spawn_tick(tick: CoalescedTick, interval: Duration, mut shutdown: watch::Receiver<()>) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = shutdown.changed() => break,
                _ = ticker.tick() => {
                    if matches!(tick.try_enqueue(), TickEnqueue::Closed) {
                        break;
                    }
                }
            }
        }
    });
}

fn panes_from_snapshot(
    snapshot: &AgentSnapshot,
    hook_grace_ms: u64,
) -> HashMap<String, TrackedPane> {
    snapshot
        .panes
        .iter()
        .map(|projection| {
            let pane = store::normalize_pane_observation(projection.pane.clone());
            let tracked = TrackedPane {
                pane,
                prompt_checkpoint: PromptAcquisitionCheckpoint::default(),
                recovery_failure: None,
                consecutive_absences: 0,
                last_hook: projection.hook_state.zip(projection.hook_observed_at_ms),
                explicit_summary: projection.explicit_work_summary.clone(),
                screen_state: seeded_screen_state(projection, hook_grace_ms),
                screen_summary: projection.screen_work_summary.clone(),
                summary_basis_version: projection.summary_basis_version,
            };
            (projection.pane.incarnation.pane_id.clone(), tracked)
        })
        .collect()
}

fn seeded_screen_state(
    projection: &super::domain::AgentPaneProjection,
    hook_grace_ms: u64,
) -> Option<ObservedAgentState> {
    let screen_state = projection.screen_state?;
    let Some(hook_observed_at_ms) = projection.hook_observed_at_ms else {
        return Some(screen_state);
    };
    let screen_observed_at_ms = projection.screen_observed_at_ms?;
    let grace_ms = i64::try_from(hook_grace_ms).unwrap_or(i64::MAX);
    (screen_observed_at_ms >= hook_observed_at_ms.saturating_add(grace_ms)).then_some(screen_state)
}

fn health_from_snapshot(snapshot: &AgentSnapshot) -> HashMap<String, HealthState> {
    snapshot
        .monitor_health
        .iter()
        .map(|health| {
            (
                health.component.clone(),
                HealthState {
                    healthy: health.healthy,
                    reason_code: health.reason_code.clone(),
                },
            )
        })
        .collect()
}

#[cfg(test)]
fn empty_snapshot() -> AgentSnapshot {
    AgentSnapshot {
        through_event_version: events::EventStreamVersion::start(),
        server_time_ms: 0,
        monitor_health: Vec::new(),
        panes: Vec::new(),
    }
}

impl AgentMonitorRuntime {
    fn schedule_summary(
        &mut self,
        pane_id: &str,
        instruction: Option<&str>,
        assistant_reply: Option<&str>,
    ) {
        let Some(summaries) = &mut self.summaries else {
            return;
        };
        summaries.invalidate(pane_id);
        let input = summaries.input(instruction.unwrap_or_default(), assistant_reply);
        if input.instruction.trim().is_empty()
            && input
                .assistant_reply
                .as_deref()
                .is_none_or(|reply| reply.trim().is_empty())
        {
            return;
        }
        let Some(tracked) = self.panes.get(pane_id) else {
            return;
        };
        summaries.enqueue(SummaryJob {
            incarnation: tracked.pane.incarnation.clone(),
            basis_version: tracked.summary_basis_version,
            input,
        });
    }

    async fn accept_summary(&mut self, result: SummaryResult) {
        let Some(tracked) = self.panes.get(&result.incarnation.pane_id) else {
            return;
        };
        if tracked.pane.incarnation != result.incarnation
            || tracked.summary_basis_version != result.basis_version
        {
            return;
        }
        let Some(description) = result
            .description
            .and_then(|value| super::summary::normalize_work_summary(&value))
        else {
            return;
        };
        if self.matches_configured_placeholder(&result.incarnation, &description) {
            return;
        }
        // Failed generation or persistence leaves the durable source fallback untouched.
        let _ = store::append_agent_events(
            &self.store,
            vec![AgentEvent::ActivitySummaryGenerated(
                super::domain::AgentActivitySummaryGenerated {
                    incarnation: result.incarnation,
                    basis_version: result.basis_version,
                    description,
                    generated_at_ms: now_ms(),
                },
            )],
        )
        .await;
    }

    async fn handle(&mut self, command: AgentMonitorCommand) {
        match command {
            AgentMonitorCommand::ReportLifecycle {
                pane_id,
                state,
                adapter_id,
                work_summary,
                reply,
            } => {
                let _ = reply.send(
                    self.report_lifecycle(pane_id, state, adapter_id, work_summary)
                        .await,
                );
            }
            AgentMonitorCommand::TurnCompleted { turn, reply } => {
                let _ = reply.send(self.turn_completed(turn).await);
            }
            #[cfg(test)]
            AgentMonitorCommand::InventoryTick { reply } => {
                let result = self.inventory_tick().await;
                if let Some(reply) = reply {
                    let _ = reply.send(result);
                }
            }
            #[cfg(test)]
            AgentMonitorCommand::ScreenTick { reply } => {
                let result = self.screen_tick().await;
                if let Some(reply) = reply {
                    let _ = reply.send(result);
                }
            }
        }
    }

    async fn report_lifecycle(
        &mut self,
        pane_id: String,
        state: ObservedAgentState,
        adapter_id: String,
        work_summary: WorkSummaryUpdate,
    ) -> Result<(), MonitorCommandError> {
        let pane = self.resolve_pane(pane_id).await?;
        let Some(pane) = pane else {
            return Err(MonitorCommandError::AgentNotFound);
        };
        let observed_at_ms = pane.observed_at_ms;
        let instruction = match &work_summary {
            WorkSummaryUpdate::Set(value) => Some(value.clone()),
            _ => None,
        };
        let work_summary = self.reject_configured_placeholder_update(
            &pane.incarnation,
            normalize_work_summary_update(work_summary),
        );
        let current = self
            .panes
            .get(&pane.incarnation.pane_id)
            .filter(|tracked| tracked.pane.incarnation == pane.incarnation);
        let next_explicit_summary = match &work_summary {
            WorkSummaryUpdate::Unchanged => {
                current.and_then(|tracked| tracked.explicit_summary.clone())
            }
            WorkSummaryUpdate::Clear => None,
            WorkSummaryUpdate::Set(summary) => Some(summary.clone()),
        };
        let lifecycle = AgentLifecycleObserved {
            incarnation: pane.incarnation.clone(),
            state,
            adapter_id,
            work_summary: work_summary.clone(),
            observed_at_ms,
        };
        let events = vec![
            AgentEvent::PaneObserved(AgentPaneObserved { pane: pane.clone() }),
            AgentEvent::LifecycleObserved(lifecycle),
        ];
        let appended = store::append_agent_events(&self.store, events)
            .await
            .map_err(MonitorCommandError::EventAppend)?;

        let tracked = self
            .panes
            .entry(pane.incarnation.pane_id.clone())
            .or_insert_with(|| TrackedPane::new(pane.clone()));
        if tracked.pane.incarnation != pane.incarnation {
            *tracked = TrackedPane::new(pane);
        } else {
            tracked.pane = pane;
        }
        tracked.last_hook = Some((state, observed_at_ms));
        tracked.explicit_summary = next_explicit_summary;
        tracked.screen_state = None;
        let pane_id = tracked.pane.incarnation.pane_id.clone();
        if let Some(basis) = source_version(&appended, &["AgentLifecycleObserved"]) {
            tracked.summary_basis_version = basis;
        }
        let instruction = instruction.filter(|_| matches!(work_summary, WorkSummaryUpdate::Set(_)));
        self.schedule_summary(&pane_id, instruction.as_deref(), None);
        self.baseline_pane(&pane_id).await?;
        Ok(())
    }

    async fn turn_completed(
        &mut self,
        mut turn: TurnCompleted,
    ) -> events::Result<events::AppendResult> {
        let pane = match resolve(
            Arc::clone(&self.inventory),
            turn.pane_id.clone(),
            self.inventory_timeout,
            Arc::clone(&self.inventory_gate),
        )
        .await
        {
            Ok(pane) => {
                let _ = self.set_health("inventory", true, "ok").await;
                pane
            }
            Err(failure) => {
                let _ = self
                    .set_health("inventory", false, failure.reason_code)
                    .await;
                None
            }
        };
        turn.work_summary = completion_summary_update(&turn.last_user_prompt);
        turn.agent_incarnation = pane.as_ref().map(|pane| pane.incarnation.clone());
        let candidate_incarnation = pane.as_ref().map(|pane| &pane.incarnation).or_else(|| {
            self.panes
                .get(&turn.pane_id)
                .map(|tracked| &tracked.pane.incarnation)
        });
        if matches!(
            &turn.work_summary,
            CompletionSummaryUpdate::Set(summary)
                if candidate_incarnation.map_or_else(
                    || self.matches_any_configured_placeholder(summary),
                    |incarnation| self.matches_configured_placeholder(incarnation, summary),
                )
        ) {
            turn.last_user_prompt.clear();
            turn.work_summary = CompletionSummaryUpdate::Unchanged;
        }
        let next_explicit_summary = pane.as_ref().and_then(|pane| match &turn.work_summary {
            CompletionSummaryUpdate::Unchanged => self
                .panes
                .get(&pane.incarnation.pane_id)
                .filter(|tracked| tracked.pane.incarnation == pane.incarnation)
                .and_then(|tracked| tracked.explicit_summary.clone()),
            CompletionSummaryUpdate::Set(summary) => Some(summary.clone()),
        });
        let result = store::append_monitor_turn_completed(&self.store, pane.clone(), &turn).await?;
        if let Some(pane) = pane {
            let observed_at_ms = pane.observed_at_ms;
            let tracked = self
                .panes
                .entry(pane.incarnation.pane_id.clone())
                .or_insert_with(|| TrackedPane::new(pane.clone()));
            if tracked.pane.incarnation != pane.incarnation {
                *tracked = TrackedPane::new(pane);
            } else {
                tracked.pane = pane;
            }
            tracked.last_hook = Some((ObservedAgentState::Idle, observed_at_ms));
            tracked.explicit_summary = next_explicit_summary;
            tracked.screen_state = None;
            let pane_id = tracked.pane.incarnation.pane_id.clone();
            if let Some(basis) = source_version(&result, &["TurnCompleted"]) {
                tracked.summary_basis_version = basis;
            }
            self.schedule_summary(
                &pane_id,
                Some(&turn.last_user_prompt),
                Some(&turn.assistant_message),
            );
            // The completion append has succeeded; capture failure degrades screen health only.
            let _ = self.baseline_pane(&pane_id).await;
        }
        Ok(result)
    }

    async fn inventory_tick(&mut self) -> Result<(), MonitorCommandError> {
        let observed = match scan(
            Arc::clone(&self.inventory),
            self.inventory_timeout,
            Arc::clone(&self.inventory_gate),
        )
        .await
        {
            Ok(observed) => observed,
            Err(failure) => {
                self.set_health("inventory", false, failure.reason_code)
                    .await?;
                return Err(MonitorCommandError::InventoryUnavailable);
            }
        };
        self.set_health("inventory", true, "ok").await?;
        let seen: HashSet<String> = observed
            .iter()
            .map(|pane| pane.incarnation.pane_id.clone())
            .collect();

        for pane in observed {
            let pane_id = pane.incarnation.pane_id.clone();
            let changed = self
                .panes
                .get(&pane_id)
                .is_none_or(|tracked| !same_pane_metadata(&tracked.pane, &pane));
            if changed {
                let appended = store::append_agent_events(
                    &self.store,
                    vec![AgentEvent::PaneObserved(AgentPaneObserved {
                        pane: pane.clone(),
                    })],
                )
                .await
                .map_err(MonitorCommandError::EventAppend)?;
                if let Some(tracked) = self
                    .panes
                    .get_mut(&pane_id)
                    .filter(|tracked| tracked.pane.incarnation == pane.incarnation)
                {
                    tracked.pane = pane;
                    tracked.consecutive_absences = 0;
                } else {
                    let mut tracked = TrackedPane::new(pane);
                    if let Some(basis) = source_version(&appended, &["AgentPaneObserved"]) {
                        tracked.summary_basis_version = basis;
                    }
                    if let Some(summaries) = &mut self.summaries {
                        summaries.invalidate(&pane_id);
                    }
                    self.panes.insert(pane_id.clone(), tracked);
                }
            } else if let Some(tracked) = self.panes.get_mut(&pane_id) {
                tracked.consecutive_absences = 0;
            }
            self.baseline_pane(&pane_id).await?;
        }

        let candidates: Vec<AgentIncarnation> = self
            .panes
            .iter_mut()
            .filter_map(|(pane_id, tracked)| {
                if seen.contains(pane_id) {
                    tracked.consecutive_absences = 0;
                    return None;
                }
                tracked.consecutive_absences = tracked.consecutive_absences.saturating_add(1);
                (tracked.consecutive_absences >= 2).then(|| tracked.pane.incarnation.clone())
            })
            .collect();

        for incarnation in candidates {
            let current = match is_current(
                Arc::clone(&self.inventory),
                incarnation.clone(),
                self.inventory_timeout,
                Arc::clone(&self.inventory_gate),
            )
            .await
            {
                Ok(current) => current,
                Err(failure) => {
                    self.set_health("inventory", false, failure.reason_code)
                        .await?;
                    return Err(MonitorCommandError::InventoryUnavailable);
                }
            };
            if current {
                if let Some(tracked) = self.panes.get_mut(&incarnation.pane_id) {
                    tracked.consecutive_absences = 0;
                }
                continue;
            }
            store::append_agent_events(
                &self.store,
                vec![AgentEvent::PaneDeparted(AgentPaneDeparted {
                    incarnation: incarnation.clone(),
                    observed_at_ms: now_ms(),
                })],
            )
            .await
            .map_err(MonitorCommandError::EventAppend)?;
            if self
                .panes
                .get(&incarnation.pane_id)
                .is_some_and(|tracked| tracked.pane.incarnation == incarnation)
            {
                self.panes.remove(&incarnation.pane_id);
                if let Some(summaries) = &mut self.summaries {
                    summaries.invalidate(&incarnation.pane_id);
                }
            }
        }
        Ok(())
    }

    /// Discovery through inventory, lifecycle, completion, or restart uses the same baseline path.
    async fn baseline_pane(&mut self, pane_id: &str) -> Result<(), MonitorCommandError> {
        let Some(tracked) = self.panes.get(pane_id) else {
            return Ok(());
        };
        if !tracked.prompt_checkpoint.baseline_due(Instant::now()) {
            return Ok(());
        }
        let pane = tracked.pane.clone();
        let Some(provider) = self.providers.get(&pane.incarnation.provider_id).cloned() else {
            return Ok(());
        };
        self.recover_prompts(&pane, &provider, None).await;
        if let Some(reason) = self
            .panes
            .get(pane_id)
            .and_then(|tracked| tracked.recovery_failure)
        {
            self.set_health("screen", false, reason).await?;
        }
        Ok(())
    }

    /// The tentative checkpoint is committed by screen_tick only after its candidate is durable.
    async fn recover_prompts(
        &mut self,
        pane: &AgentPaneObservation,
        provider: &AgentProviderSettings,
        state: Option<ObservedAgentState>,
    ) -> Option<(PromptAcquisitionCheckpoint, Option<String>)> {
        let tracked = self.panes.get_mut(&pane.incarnation.pane_id)?;
        if tracked.pane.incarnation != pane.incarnation
            || !tracked.prompt_checkpoint.should_scan(state, Instant::now())
        {
            return None;
        }
        tracked.prompt_checkpoint.attempted(Instant::now());
        let result = scan_prompts(
            Arc::clone(&self.screen),
            pane.clone(),
            provider.clone(),
            self.acquisition_timeout,
            Arc::clone(&self.screen_gate),
        )
        .await;
        let tracked = self.panes.get_mut(&pane.incarnation.pane_id)?;
        if tracked.pane.incarnation != pane.incarnation {
            return None;
        }
        let scan = match result {
            Ok(scan) => {
                tracked.recovery_failure = None;
                scan
            }
            Err(failure) => {
                tracked.recovery_failure = Some(failure.reason_code);
                tracked.prompt_checkpoint.capture_failed(state);
                return None;
            }
        };
        let mut checkpoint = tracked.prompt_checkpoint.clone();
        let candidate = checkpoint
            .acquire(scan, state)
            .and_then(|value| normalize_fallback_summary(&value, &provider.idle_all));
        if candidate.is_none() {
            tracked.prompt_checkpoint = checkpoint.clone();
        }
        Some((checkpoint, candidate))
    }

    async fn screen_tick(&mut self) -> Result<(), MonitorCommandError> {
        let panes: Vec<_> = self
            .panes
            .values()
            .map(|tracked| tracked.pane.clone())
            .collect();
        let mut attempted = false;
        let mut failure_reason = None;
        for pane in panes {
            let Some(provider) = self.providers.get(&pane.incarnation.provider_id).cloned() else {
                continue;
            };
            attempted = true;
            // Failed baselines retry in Idle and even when visible classification is unavailable.
            self.baseline_pane(&pane.incarnation.pane_id).await?;
            let observation = match observe_screen(
                Arc::clone(&self.screen),
                pane.clone(),
                provider.clone(),
                self.acquisition_timeout,
                Arc::clone(&self.screen_gate),
            )
            .await
            {
                Ok(observation) => observation,
                Err(failure) => {
                    failure_reason.get_or_insert(failure.reason_code);
                    continue;
                }
            };
            if observation.incarnation != pane.incarnation {
                continue;
            }
            // Recovery triggers consume raw visible state, independently of hook-grace filtering.
            let recovery = self
                .recover_prompts(&pane, &provider, observation.state)
                .await;
            let Some(tracked) = self
                .panes
                .get_mut(&pane.incarnation.pane_id)
                .filter(|tracked| tracked.pane.incarnation == observation.incarnation)
            else {
                continue;
            };
            let state = screen_state_delta(
                tracked,
                observation.state,
                observation.observed_at_ms,
                self.hook_grace_ms,
            );
            let recovered = recovery
                .as_ref()
                .and_then(|(_, candidate)| candidate.clone());
            let summary = recovered.clone().or_else(|| {
                observation
                    .fallback_summary
                    .as_deref()
                    .and_then(|summary| normalize_fallback_summary(summary, &provider.idle_all))
                    .filter(|summary| tracked.screen_summary.as_deref() != Some(summary))
            });
            if state.is_none() && summary.is_none() {
                tracked.prompt_checkpoint.observe_state(observation.state);
                continue;
            }
            let event = AgentScreenObserved {
                incarnation: observation.incarnation,
                state,
                classifier_id: if recovered.is_some() {
                    super::reducer::SUBMITTED_PROMPT_CLASSIFIER_ID.into()
                } else {
                    observation.classifier_id
                },
                fallback_summary: summary.clone(),
                observed_at_ms: observation.observed_at_ms,
            };
            let previous_state = match tracked
                .screen_state
                .or(tracked.last_hook.map(|(state, _)| state))
            {
                Some(ObservedAgentState::Busy) => super::domain::EffectiveAgentState::Busy,
                Some(ObservedAgentState::Idle) => super::domain::EffectiveAgentState::Idle,
                None => super::domain::EffectiveAgentState::Unknown,
            };
            let changes_activity = super::reducer::screen_changes_activity(
                previous_state,
                tracked.screen_summary.as_deref(),
                &event,
            );
            let appended = match store::append_agent_events(
                &self.store,
                vec![AgentEvent::ScreenObserved(event)],
            )
            .await
            {
                Ok(appended) => appended,
                Err(error) => {
                    if recovered.is_some() {
                        tracked
                            .prompt_checkpoint
                            .source_append_failed(observation.state);
                    }
                    return Err(MonitorCommandError::EventAppend(error));
                }
            };
            if let Some((checkpoint, _)) = recovery {
                tracked.prompt_checkpoint = checkpoint;
            }
            tracked.prompt_checkpoint.observe_state(observation.state);
            if let Some(state) = state {
                tracked.screen_state = Some(state);
            }
            if summary.is_some() {
                tracked.screen_summary = summary.clone();
            }
            if changes_activity {
                if let Some(basis) = source_version(&appended, &["AgentScreenObserved"]) {
                    tracked.summary_basis_version = basis;
                }
                let pane_id = tracked.pane.incarnation.pane_id.clone();
                self.schedule_summary(&pane_id, summary.as_deref(), None);
            }
        }
        let failure_reason = failure_reason.or_else(|| {
            self.panes
                .values()
                .find_map(|tracked| tracked.recovery_failure)
        });
        if let Some(reason_code) = failure_reason {
            self.set_health("screen", false, reason_code).await?;
        } else if attempted {
            self.set_health("screen", true, "ok").await?;
        }
        Ok(())
    }

    fn reject_configured_placeholder_update(
        &self,
        incarnation: &AgentIncarnation,
        update: WorkSummaryUpdate,
    ) -> WorkSummaryUpdate {
        match update {
            WorkSummaryUpdate::Set(summary)
                if self.matches_configured_placeholder(incarnation, &summary) =>
            {
                WorkSummaryUpdate::Unchanged
            }
            update => update,
        }
    }

    fn matches_configured_placeholder(
        &self,
        incarnation: &AgentIncarnation,
        summary: &str,
    ) -> bool {
        matches_configured_placeholder_for_provider(
            self.providers.values(),
            &incarnation.provider_id,
            summary,
        )
    }

    fn matches_any_configured_placeholder(&self, summary: &str) -> bool {
        self.providers
            .values()
            .any(|provider| matches_configured_placeholder(summary, &provider.idle_all))
    }

    async fn resolve_pane(
        &mut self,
        pane_id: String,
    ) -> Result<Option<AgentPaneObservation>, MonitorCommandError> {
        match resolve(
            Arc::clone(&self.inventory),
            pane_id,
            self.inventory_timeout,
            Arc::clone(&self.inventory_gate),
        )
        .await
        {
            Ok(pane) => {
                self.set_health("inventory", true, "ok").await?;
                Ok(pane)
            }
            Err(failure) => {
                self.set_health("inventory", false, failure.reason_code)
                    .await?;
                Err(MonitorCommandError::InventoryUnavailable)
            }
        }
    }

    async fn set_health(
        &mut self,
        component: &'static str,
        healthy: bool,
        reason_code: &'static str,
    ) -> Result<(), MonitorCommandError> {
        // A previous timed-out worker still owns the gate: there is no new observation.
        if reason_code == "busy" {
            return Ok(());
        }
        let next = HealthState {
            healthy,
            reason_code: reason_code.into(),
        };
        self.log_health_transition(component, &next);
        if self.health.get(component) == Some(&next)
            || (healthy && !self.health.contains_key(component))
        {
            return Ok(());
        }
        store::append_agent_events(
            &self.store,
            vec![AgentEvent::MonitorHealthChanged(
                super::domain::AgentMonitorHealthChanged {
                    component: component.into(),
                    healthy,
                    reason_code: reason_code.into(),
                    observed_at_ms: now_ms(),
                },
            )],
        )
        .await
        .map_err(MonitorCommandError::EventAppend)?;
        self.health.insert(component.into(), next);
        Ok(())
    }

    fn log_health_transition(&mut self, component: &'static str, next: &HealthState) {
        if self.logged_health.get(component) == Some(next) {
            return;
        }
        let previous = self
            .logged_health
            .get(component)
            .or_else(|| self.health.get(component));
        if !next.healthy {
            tracing::warn!(component, reason_code = %next.reason_code, "agent monitor degraded");
        } else if previous.is_some_and(|previous| !previous.healthy) {
            tracing::info!(component, reason_code = %next.reason_code, "agent monitor recovered");
        }
        // Track current-process observations separately so restored failures are logged once,
        // without making a failed durable append suppress its retry.
        self.logged_health.insert(component.into(), next.clone());
    }
}

fn normalize_work_summary_update(update: WorkSummaryUpdate) -> WorkSummaryUpdate {
    match update {
        WorkSummaryUpdate::Set(summary) => super::summary::normalize_work_summary(&summary)
            .map_or(WorkSummaryUpdate::Clear, WorkSummaryUpdate::Set),
        update => update,
    }
}

fn matches_configured_placeholder(summary: &str, fragments: &[String]) -> bool {
    super::summary::normalize_work_summary(summary).is_some()
        && normalize_fallback_summary(summary, fragments).is_none()
}

fn matches_configured_placeholder_for_provider<'a>(
    mut providers: impl Iterator<Item = &'a AgentProviderSettings> + Clone,
    provider_id: &str,
    summary: &str,
) -> bool {
    if let Some(provider) = providers
        .clone()
        .find(|provider| provider.id == provider_id)
    {
        return matches_configured_placeholder(summary, &provider.idle_all);
    }
    providers.any(|provider| matches_configured_placeholder(summary, &provider.idle_all))
}

fn screen_state_delta(
    tracked: &TrackedPane,
    observed: Option<ObservedAgentState>,
    observed_at_ms: i64,
    hook_grace_ms: u64,
) -> Option<ObservedAgentState> {
    let state = observed?;
    if let Some((_, hook_observed_at_ms)) = tracked.last_hook {
        let grace = i64::try_from(hook_grace_ms).unwrap_or(i64::MAX);
        if observed_at_ms < hook_observed_at_ms.saturating_add(grace) {
            return None;
        }
    }
    (tracked.screen_state != Some(state)).then_some(state)
}

fn same_pane_metadata(left: &AgentPaneObservation, right: &AgentPaneObservation) -> bool {
    left.incarnation == right.incarnation
        && left.tmux_target == right.tmux_target
        && left.session_name == right.session_name
        && left.window_index == right.window_index
        && left.pane_index == right.pane_index
        && left.working_directory == right.working_directory
        && left.provider_display_name == right.provider_display_name
}

async fn scan(
    inventory: Arc<dyn AgentInventoryPort>,
    timeout: Duration,
    gate: Arc<Semaphore>,
) -> Result<Vec<AgentPaneObservation>, AcquisitionFailure> {
    run_inventory(timeout, gate, move || inventory.scan())
        .await
        .map(|panes| {
            panes
                .into_iter()
                .map(store::normalize_pane_observation)
                .collect()
        })
}

async fn resolve(
    inventory: Arc<dyn AgentInventoryPort>,
    pane_id: String,
    timeout: Duration,
    gate: Arc<Semaphore>,
) -> Result<Option<AgentPaneObservation>, AcquisitionFailure> {
    run_inventory(timeout, gate, move || inventory.resolve(&pane_id))
        .await
        .map(|pane| pane.map(store::normalize_pane_observation))
}

async fn is_current(
    inventory: Arc<dyn AgentInventoryPort>,
    incarnation: AgentIncarnation,
    timeout: Duration,
    gate: Arc<Semaphore>,
) -> Result<bool, AcquisitionFailure> {
    run_inventory(timeout, gate, move || inventory.is_current(&incarnation)).await
}

async fn observe_screen(
    screen: Arc<dyn VisibleScreenPort>,
    pane: AgentPaneObservation,
    provider: AgentProviderSettings,
    timeout: Duration,
    gate: Arc<Semaphore>,
) -> Result<super::domain::ScreenObservation, AcquisitionFailure> {
    let result = run_bounded_thread("harold-screen", timeout, gate, move || {
        screen.observe(&pane, &provider)
    })
    .await?;
    result.map_err(|error| AcquisitionFailure {
        reason_code: screen_reason(error),
    })
}

async fn scan_prompts(
    screen: Arc<dyn VisibleScreenPort>,
    pane: AgentPaneObservation,
    provider: AgentProviderSettings,
    timeout: Duration,
    gate: Arc<Semaphore>,
) -> Result<PromptScan, AcquisitionFailure> {
    run_bounded_thread("harold-screen-history", timeout, gate, move || {
        screen.scan_prompts(&pane, &provider)
    })
    .await?
    .map_err(|error| AcquisitionFailure {
        reason_code: screen_reason(error),
    })
}

async fn run_inventory<T, F>(
    timeout: Duration,
    gate: Arc<Semaphore>,
    operation: F,
) -> Result<T, AcquisitionFailure>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, InventoryError> + Send + 'static,
{
    let result = run_bounded_thread("harold-inventory", timeout, gate, operation).await?;
    result.map_err(|error| AcquisitionFailure {
        reason_code: inventory_reason(error),
    })
}

async fn run_bounded_thread<T, F>(
    thread_name: &'static str,
    timeout: Duration,
    gate: Arc<Semaphore>,
    operation: F,
) -> Result<T, AcquisitionFailure>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let permit = gate.try_acquire_owned().map_err(|_| AcquisitionFailure {
        reason_code: "busy",
    })?;
    let (sender, receiver) = oneshot::channel();
    std::thread::Builder::new()
        .name(thread_name.into())
        .spawn(move || {
            let result = operation();
            // A waiting caller may immediately start the next visible/history acquisition.
            drop(permit);
            let _ = sender.send(result);
        })
        .map_err(|_| AcquisitionFailure {
            reason_code: "task_failed",
        })?;
    tokio::time::timeout(timeout, receiver)
        .await
        .map_err(|_| AcquisitionFailure {
            reason_code: "timeout",
        })?
        .map_err(|_| AcquisitionFailure {
            reason_code: "task_failed",
        })
}

fn inventory_reason(error: InventoryError) -> &'static str {
    match error {
        InventoryError::CommandUnavailable => "command_unavailable",
        InventoryError::CommandFailed => "command_failed",
        InventoryError::MalformedOutput => "malformed_output",
        InventoryError::MissingProcessStartTime => "missing_start_time",
    }
}

fn screen_reason(error: ScreenError) -> &'static str {
    match error {
        ScreenError::CaptureUnavailable => "capture_unavailable",
        ScreenError::CaptureFailed => "capture_failed",
        ScreenError::PaneDeparted => "pane_departed",
    }
}

#[allow(
    dead_code,
    reason = "the ReportAgentState RPC activates lifecycle validation in the next ingress slice"
)]
fn valid_pane_id(value: &str) -> bool {
    value.strip_prefix('%').is_some_and(|suffix| {
        !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
    })
}

#[allow(
    dead_code,
    reason = "the ReportAgentState RPC activates lifecycle validation in the next ingress slice"
)]
fn valid_identifier(value: &str) -> bool {
    let bytes = value.as_bytes();
    (1..=64).contains(&bytes.len())
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(byte))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(i64::MAX)
}

async fn next_summary(summaries: &mut Option<ActivityScheduler>) -> SummaryResult {
    match summaries {
        Some(summaries) => summaries.next().await,
        None => std::future::pending().await,
    }
}

fn source_version(appended: &events::AppendResult, types: &[&str]) -> Option<EventStreamVersion> {
    appended
        .events
        .iter()
        .rev()
        .find(|event| types.contains(&event.r#type.as_str()))
        .map(|event| event.version)
}

#[cfg(test)]
mod acquisition_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn completed_acquisition_releases_gate_before_notifying_caller() {
        let gate = Arc::new(Semaphore::new(1));
        for _ in 0..1_000 {
            run_bounded_thread(
                "harold-gate-test",
                Duration::from_secs(1),
                gate.clone(),
                || (),
            )
            .await
            .unwrap_or_else(|failure| {
                panic!("unexpected acquisition failure: {}", failure.reason_code)
            });
            assert!(
                gate.try_acquire().is_ok(),
                "a completed operation must no longer hold its gate"
            );
        }
    }

    #[tokio::test]
    async fn timed_out_worker_holds_gate_and_busy_attempt_does_not_start_another_worker() {
        let gate = Arc::new(Semaphore::new(1));
        let calls = Arc::new(AtomicUsize::new(0));
        let worker_calls = Arc::clone(&calls);
        let (release, held) = std::sync::mpsc::channel();
        let failure = run_bounded_thread(
            "test-inventory",
            Duration::from_millis(20),
            Arc::clone(&gate),
            move || {
                worker_calls.fetch_add(1, Ordering::SeqCst);
                let _ = held.recv();
            },
        )
        .await
        .expect_err("held worker must time out");
        assert_eq!(failure.reason_code, "timeout");
        for _ in 0..3 {
            let worker_calls = Arc::clone(&calls);
            let failure = run_bounded_thread(
                "test-inventory",
                Duration::from_secs(1),
                Arc::clone(&gate),
                move || {
                    worker_calls.fetch_add(1, Ordering::SeqCst);
                },
            )
            .await
            .expect_err("held gate must reject another worker");
            assert_eq!(failure.reason_code, "busy");
        }
        // The worker might only get scheduled after the timeout, so release it before joining.
        release.send(()).unwrap();
        let permit = tokio::time::timeout(Duration::from_secs(1), gate.acquire())
            .await
            .expect("worker must release the gate")
            .unwrap();
        drop(permit);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(
            run_bounded_thread("test-inventory", Duration::from_secs(1), gate, || ())
                .await
                .is_ok()
        );
    }
}
