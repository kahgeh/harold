#[path = "activity_scheduler.rs"]
mod activity_scheduler;
#[path = "prompt_checkpoint.rs"]
mod prompt_checkpoint;

use crate::activity_summary::ActivitySummarizer;
use crate::settings::ActivitySummarySettings;
use activity_scheduler::{ActivityScheduler, SummaryJob, SummaryResult};
use prompt_checkpoint::PromptAcquisitionCheckpoint;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::{Semaphore, mpsc, oneshot, watch};

use crate::settings::AgentProviderSettings;
use crate::store::{self, EventVersion, HaroldStore, StoreError, TurnCompleted};

use super::domain::{
    AgentEvent, AgentIncarnation, AgentLifecycleObserved, AgentPaneDeparted, AgentPaneObservation,
    AgentPaneObserved, AgentScreenObserved, AgentSnapshot, CompletionSummaryUpdate,
    ObservedAgentState, WorkSummaryUpdate,
};
use super::inventory::{AgentInventoryPort, InventoryError};
use super::reducer::GraceWindows;
use super::screen::{
    PromptScan, ScreenError, StopSignal, VisibleScreenPort, normalize_fallback_summary,
    run_stoppable,
};
use super::summary::completion_summary_update;

const COMMAND_CAPACITY: usize = 64;
/// A screen read's retry gets the capture command's own limit (`screen.rs`'s `SystemCommandRunner`).
pub(super) const SCREEN_RETRY_TIMEOUT: Duration = Duration::from_millis(2_000);
/// The part of an inventory tick kept for a timed-out scan's worker to stop and let go of the
/// gate, so the calls queued behind the tick find it free. At most a tenth of the timeout.
const INVENTORY_STOP_GRACE: Duration = Duration::from_millis(50);

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
        reply: oneshot::Sender<store::Result<store::AppendResult>>,
    },
    ResolvePane {
        pane_id: String,
        reply: oneshot::Sender<Result<AgentPaneObservation, MonitorCommandError>>,
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
    EventAppend(StoreError),
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
    ) -> store::Result<store::AppendResult> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(AgentMonitorCommand::TurnCompleted { turn, reply })
            .await
            .map_err(|_| StoreError::Stopped("agent monitor runtime stopped"))?;
        response
            .await
            .map_err(|_| StoreError::Stopped("agent monitor runtime stopped"))?
    }

    /// The agent pane with this id, by the lookup a lifecycle report makes.
    pub(crate) async fn resolve_agent_pane(
        &self,
        pane_id: String,
    ) -> Result<AgentPaneObservation, MonitorCommandError> {
        if !valid_pane_id(&pane_id) {
            return Err(MonitorCommandError::InvalidInput);
        }
        let (reply, response) = oneshot::channel();
        self.sender
            .send(AgentMonitorCommand::ResolvePane { pane_id, reply })
            .await
            .map_err(|_| MonitorCommandError::RuntimeStopped)?;
        response
            .await
            .map_err(|_| MonitorCommandError::RuntimeStopped)?
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
    /// The lifecycle adapter that last reported for this agent process.
    lifecycle_adapter_id: Option<String>,
    explicit_summary: Option<String>,
    screen_state: Option<ObservedAgentState>,
    /// When the unbroken busy reading began, for a pane whose adapter reported idle.
    screen_busy_since_ms: Option<i64>,
    screen_summary: Option<String>,
    summary_basis_version: EventVersion,
    /// Whether `agent monitor pane gone` has been logged for this incarnation.
    pane_gone_logged: bool,
}

impl TrackedPane {
    fn new(pane: AgentPaneObservation) -> Self {
        Self {
            pane,
            prompt_checkpoint: PromptAcquisitionCheckpoint::default(),
            recovery_failure: None,
            consecutive_absences: 0,
            last_hook: None,
            lifecycle_adapter_id: None,
            explicit_summary: None,
            screen_state: None,
            screen_busy_since_ms: None,
            screen_summary: None,
            summary_basis_version: EventVersion::start(),
            pane_gone_logged: false,
        }
    }
}

struct AgentMonitorRuntime {
    store: Arc<HaroldStore>,
    inventory: Arc<dyn AgentInventoryPort>,
    screen: Arc<dyn VisibleScreenPort>,
    providers: HashMap<String, AgentProviderSettings>,
    graces: GraceWindows,
    acquisition_timeout: Duration,
    inventory_timeout: Duration,
    inventory_gate: Arc<Semaphore>,
    screen_gate: Arc<Semaphore>,
    panes: HashMap<String, TrackedPane>,
    health: HashMap<String, HealthState>,
    logged_health: HashMap<String, HealthState>,
    summaries: Option<ActivityScheduler>,
    /// When a failed inventory scan started, and why it failed: the next tick's scan is its
    /// retry.
    held_inventory_failure: Option<(tokio::time::Instant, &'static str)>,
    /// How long a successful inventory scan stays good enough for a turn-complete: two
    /// inventory intervals. Zero means a turn-complete always scans.
    scan_freshness: Duration,
    /// When the last successful inventory scan finished.
    last_scan_finished: Option<Instant>,
}

pub(crate) struct AgentMonitorRuntimeConfig {
    pub(crate) activity_summary: Option<(Arc<dyn ActivitySummarizer>, ActivitySummarySettings)>,
    pub(crate) inventory_interval: Duration,
    pub(crate) screen_interval: Duration,
    pub(crate) hook_grace_ms: u64,
    pub(crate) report_grace_ms: u64,
    pub(crate) acquisition_timeout: Duration,
    pub(crate) inventory_timeout: Duration,
}

struct RuntimeInputs {
    store: Arc<HaroldStore>,
    inventory: Arc<dyn AgentInventoryPort>,
    screen: Arc<dyn VisibleScreenPort>,
    providers: Vec<AgentProviderSettings>,
    initial_snapshot: AgentSnapshot,
    graces: GraceWindows,
    acquisition_timeout: Duration,
    inventory_timeout: Duration,
    intervals: Option<(Duration, Duration)>,
    /// The inventory interval, which tests without ticks still set for the turn-complete window.
    inventory_interval: Duration,
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
            graces: GraceWindows {
                hook_ms: config.hook_grace_ms,
                report_ms: config.report_grace_ms,
            },
            acquisition_timeout: config.acquisition_timeout,
            inventory_timeout: config.inventory_timeout,
            intervals: Some((config.inventory_interval, config.screen_interval)),
            inventory_interval: config.inventory_interval,
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
    spawn_agent_monitor_with_graces_for_test(
        store,
        inventory,
        screen,
        providers,
        GraceWindows::same(hook_grace_ms),
        shutdown,
    )
}

#[cfg(test)]
pub(crate) fn spawn_agent_monitor_with_graces_for_test<I, S>(
    store: Arc<HaroldStore>,
    inventory: Arc<I>,
    screen: Arc<S>,
    providers: Vec<AgentProviderSettings>,
    graces: GraceWindows,
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
            graces,
            acquisition_timeout: Duration::from_millis(100),
            inventory_timeout: Duration::from_millis(100),
            intervals: None,
            inventory_interval: Duration::ZERO,
            activity_summary: None,
        },
        shutdown,
    )
}

/// Like the graces spawner, with the inventory interval a turn-complete's scan window comes from.
#[cfg(test)]
pub(crate) fn spawn_agent_monitor_with_inventory_interval_for_test<I, S>(
    store: Arc<HaroldStore>,
    inventory: Arc<I>,
    screen: Arc<S>,
    providers: Vec<AgentProviderSettings>,
    graces: GraceWindows,
    inventory_interval: Duration,
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
            graces,
            acquisition_timeout: Duration::from_millis(100),
            inventory_timeout: Duration::from_millis(100),
            intervals: None,
            inventory_interval,
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
            graces: GraceWindows::same(seed.hook_grace_ms),
            acquisition_timeout: seed.acquisition_timeout,
            inventory_timeout: seed.acquisition_timeout,
            intervals: None,
            inventory_interval: Duration::ZERO,
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
        graces,
        acquisition_timeout,
        inventory_timeout,
        intervals,
        inventory_interval,
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
            graces,
            acquisition_timeout,
            inventory_timeout,
            inventory_gate: Arc::new(Semaphore::new(1)),
            screen_gate: Arc::new(Semaphore::new(1)),
            panes: panes_from_snapshot(&initial_snapshot, graces),
            health: health_from_snapshot(&initial_snapshot),
            logged_health: HashMap::new(),
            summaries: activity_summary
                .map(|(provider, settings)| ActivityScheduler::new(provider, settings)),
            held_inventory_failure: None,
            scan_freshness: inventory_interval.saturating_mul(2),
            last_scan_finished: None,
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
    graces: GraceWindows,
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
                lifecycle_adapter_id: projection.lifecycle_adapter_id.clone(),
                explicit_summary: projection.explicit_work_summary.clone(),
                screen_state: seeded_screen_state(projection, graces),
                screen_busy_since_ms: None,
                screen_summary: projection.screen_work_summary.clone(),
                summary_basis_version: projection.summary_basis_version,
                pane_gone_logged: false,
            };
            (projection.pane.incarnation.pane_id.clone(), tracked)
        })
        .collect()
}

fn seeded_screen_state(
    projection: &super::domain::AgentPaneProjection,
    graces: GraceWindows,
) -> Option<ObservedAgentState> {
    let screen_state = projection.screen_state?;
    let Some(hook_observed_at_ms) = projection.hook_observed_at_ms else {
        return Some(screen_state);
    };
    let screen_observed_at_ms = projection.screen_observed_at_ms?;
    let grace_ms = graces.for_pane(projection.lifecycle_adapter_id.is_some());
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
        tts_muted: false,
        messaging_paused: false,
        through_event_version: EventVersion::start(),
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
            AgentMonitorCommand::ResolvePane { pane_id, reply } => {
                let pane = self
                    .resolve_pane(pane_id)
                    .await
                    .and_then(|pane| pane.ok_or(MonitorCommandError::AgentNotFound));
                let _ = reply.send(pane);
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

    /// The first time Harold tracks this incarnation, whichever of the inventory or a hook sees it.
    fn log_pane_found_if_new(&self, pane: &AgentPaneObservation) {
        let known = self
            .panes
            .get(&pane.incarnation.pane_id)
            .is_some_and(|tracked| tracked.pane.incarnation == pane.incarnation);
        if !known {
            self.log_pane_found(pane);
        }
    }

    fn log_pane_found(&self, pane: &AgentPaneObservation) {
        tracing::info!(
            pane_id = pane.incarnation.pane_id.as_str(),
            provider_id = pane.incarnation.provider_id.as_str(),
            program = self
                .inventory
                .program(&pane.incarnation)
                .as_deref()
                .unwrap_or(""),
            "agent monitor pane found"
        );
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
        // The same adapter repeating the pane's state, with no summary: the reducer applies
        // the same test to the stored event.
        let is_repeat = work_summary == WorkSummaryUpdate::Unchanged
            && current.is_some_and(|tracked| {
                tracked.lifecycle_adapter_id.as_deref() == Some(adapter_id.as_str())
                    && tracked.last_hook.map(|(state, _)| state) == Some(state)
            });
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
            adapter_id: adapter_id.clone(),
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

        self.log_pane_found_if_new(&pane);
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
        tracked.lifecycle_adapter_id = Some(adapter_id);
        tracked.explicit_summary = next_explicit_summary;
        tracked.screen_state = None;
        tracked.screen_busy_since_ms = None;
        let pane_id = tracked.pane.incarnation.pane_id.clone();
        if state == ObservedAgentState::Busy {
            crate::telemetry::pane_turned_busy(&pane_id, observed_at_ms);
        }
        // A repeat starts no new work, so the summary in progress stands.
        if !is_repeat {
            if let Some(basis) = source_version(&appended, &["AgentLifecycleObserved"]) {
                tracked.summary_basis_version = basis;
            }
            let instruction =
                instruction.filter(|_| matches!(work_summary, WorkSummaryUpdate::Set(_)));
            self.schedule_summary(&pane_id, instruction.as_deref(), None);
        }
        self.baseline_pane(&pane_id).await?;
        Ok(())
    }

    /// The tracked pane, as of `observed_at_ms`, when the last successful inventory scan found
    /// it and finished at most two inventory intervals ago: no scan of its own is needed.
    fn recently_scanned_pane(
        &self,
        pane_id: &str,
        observed_at_ms: i64,
    ) -> Option<AgentPaneObservation> {
        let scanned = self.last_scan_finished?;
        if self.scan_freshness.is_zero() || scanned.elapsed() > self.scan_freshness {
            return None;
        }
        let tracked = self.panes.get(pane_id)?;
        (tracked.consecutive_absences == 0).then(|| AgentPaneObservation {
            observed_at_ms,
            ..tracked.pane.clone()
        })
    }

    async fn turn_completed(
        &mut self,
        mut turn: TurnCompleted,
    ) -> store::Result<store::AppendResult> {
        let reached_at_ms = now_ms();
        let pane = match self.recently_scanned_pane(&turn.pane_id, reached_at_ms) {
            Some(pane) => Some(pane),
            None => match resolve(
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
            },
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
            self.log_pane_found_if_new(&pane);
            let tracked = self
                .panes
                .entry(pane.incarnation.pane_id.clone())
                .or_insert_with(|| TrackedPane::new(pane.clone()));
            if tracked.pane.incarnation != pane.incarnation {
                *tracked = TrackedPane::new(pane);
            } else {
                tracked.pane = pane;
            }
            tracked.explicit_summary = next_explicit_summary;
            // The Stop hook sets the state only for a pane no lifecycle adapter reports on.
            if tracked.lifecycle_adapter_id.is_none() {
                tracked.last_hook = Some((ObservedAgentState::Idle, observed_at_ms));
                tracked.screen_state = None;
            }
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

    /// A failed scan is retried by the next tick's scan, not at once: hook calls queue behind a
    /// tick, and some of their clients give up after 3 seconds, so a tick's scan takes at most
    /// `inventory_timeout`, retry or not. Prompt baselines and revalidating missing panes come
    /// on top, with their own deadlines.
    async fn inventory_tick(&mut self) -> Result<(), MonitorCommandError> {
        let held = self.held_inventory_failure.take();
        let started = tokio::time::Instant::now();
        let stop = StopSignal::default();
        let grace = INVENTORY_STOP_GRACE.min(self.inventory_timeout / 10);
        let result = scan(
            Arc::clone(&self.inventory),
            self.inventory_timeout - grace,
            Arc::clone(&self.inventory_gate),
            stop.clone(),
        )
        .await;
        let observed = match result {
            Ok(observed) => {
                if let Some((first_started, _)) = held {
                    log_read_retried("inventory", None, first_started);
                }
                observed
            }
            Err(failure) => {
                if failure.reason_code == "timeout" {
                    stop.request();
                    let _ = tokio::time::timeout(grace, self.inventory_gate.acquire()).await;
                }
                match (failure.reason_code, held) {
                    // No scan ran, because an earlier one still holds the gate. That is today's
                    // no-op, unless it was the retry: then the retry has failed.
                    ("busy", None) => {}
                    ("busy", Some((_, reason_code))) | (reason_code, Some(_)) => {
                        self.set_health("inventory", false, reason_code).await?;
                    }
                    (reason_code, None) => {
                        self.held_inventory_failure = Some((started, reason_code));
                    }
                }
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
                    self.log_pane_found(&pane);
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
        // Only now do the tracked panes and their absence counts reflect this scan.
        self.last_scan_finished = Some(Instant::now());
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
                // A closed pane is no degradation: the inventory departs it in time.
                tracked.recovery_failure =
                    (failure.reason_code != "pane_departed").then_some(failure.reason_code);
                if failure.reason_code == "pane_departed" {
                    log_pane_gone(tracked);
                }
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
        // One retry per pass, whether it works or not: the rest of the pass reads each pane
        // once, so a slow tmux does not hold queued hook calls for a retry per pane.
        let mut retried = false;
        for pane in panes {
            let Some(provider) = self.providers.get(&pane.incarnation.provider_id).cloned() else {
                continue;
            };
            attempted = true;
            // Failed baselines retry in Idle and even when visible classification is unavailable.
            self.baseline_pane(&pane.incarnation.pane_id).await?;
            let screen = &self.screen;
            let gate = &self.screen_gate;
            let mut reads = 0_u32;
            let mut read = |timeout, stop| {
                reads += 1;
                observe_screen(
                    Arc::clone(screen),
                    pane.clone(),
                    provider.clone(),
                    timeout,
                    Arc::clone(gate),
                    stop,
                )
            };
            let result = if retried {
                read(self.acquisition_timeout, StopSignal::default()).await
            } else {
                read_with_retry(
                    &pane.incarnation.pane_id,
                    gate,
                    self.acquisition_timeout,
                    SCREEN_RETRY_TIMEOUT,
                    &mut read,
                )
                .await
            };
            // A first try that failed spent the retry, even when the wait for the gate gave out
            // before the retry ran. A busy gate means no read ran at all.
            // A pane that departed costs no retry either: it isn't coming back.
            retried |= reads > 1
                || result
                    .as_ref()
                    .is_err_and(|failure| !matches!(failure.reason_code, "busy" | "pane_departed"));
            let observation = match result {
                Ok(observation) => observation,
                Err(failure) if failure.reason_code == "pane_departed" => {
                    if let Some(tracked) = self
                        .panes
                        .get_mut(&pane.incarnation.pane_id)
                        .filter(|tracked| tracked.pane.incarnation == pane.incarnation)
                    {
                        log_pane_gone(tracked);
                    }
                    continue;
                }
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
                self.graces,
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

/// The screen state to store for this sample, if any.
fn screen_state_delta(
    tracked: &mut TrackedPane,
    observed: Option<ObservedAgentState>,
    observed_at_ms: i64,
    graces: GraceWindows,
) -> Option<ObservedAgentState> {
    let Some((hook_state, hook_observed_at_ms)) = tracked.last_hook else {
        return observed.filter(|state| tracked.screen_state != Some(*state));
    };
    let has_adapter = tracked.lifecycle_adapter_id.is_some();
    let grace = graces.for_pane(has_adapter);

    // An adapter's idle report has no repeats to keep it fresh. It stands until the screen
    // has read busy for the whole grace without a break; then the screen decides.
    if has_adapter && hook_state == ObservedAgentState::Idle && tracked.screen_state.is_none() {
        if observed != Some(ObservedAgentState::Busy) {
            tracked.screen_busy_since_ms = None;
            return None;
        }
        if tracked.screen_busy_since_ms.is_none() {
            crate::telemetry::pane_turned_busy(&tracked.pane.incarnation.pane_id, observed_at_ms);
        }
        let busy_since_ms = *tracked
            .screen_busy_since_ms
            .get_or_insert(observed_at_ms.max(hook_observed_at_ms));
        return (observed_at_ms >= busy_since_ms.saturating_add(grace))
            .then_some(ObservedAgentState::Busy);
    }

    let state = observed?;
    if observed_at_ms < hook_observed_at_ms.saturating_add(grace) {
        return None;
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
    stop: StopSignal,
) -> Result<Vec<AgentPaneObservation>, AcquisitionFailure> {
    run_inventory(timeout, gate, stop, move || inventory.scan())
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
    run_inventory(timeout, gate, StopSignal::default(), move || {
        inventory.resolve(&pane_id)
    })
    .await
    .map(|pane| pane.map(store::normalize_pane_observation))
}

async fn is_current(
    inventory: Arc<dyn AgentInventoryPort>,
    incarnation: AgentIncarnation,
    timeout: Duration,
    gate: Arc<Semaphore>,
) -> Result<bool, AcquisitionFailure> {
    run_inventory(timeout, gate, StopSignal::default(), move || {
        inventory.is_current(&incarnation)
    })
    .await
}

async fn observe_screen(
    screen: Arc<dyn VisibleScreenPort>,
    pane: AgentPaneObservation,
    provider: AgentProviderSettings,
    timeout: Duration,
    gate: Arc<Semaphore>,
    stop: StopSignal,
) -> Result<super::domain::ScreenObservation, AcquisitionFailure> {
    let result = run_bounded_thread("harold-screen", timeout, gate, stop, move || {
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
    run_bounded_thread(
        "harold-screen-history",
        timeout,
        gate,
        StopSignal::default(),
        move || screen.scan_prompts(&pane, &provider),
    )
    .await?
    .map_err(|error| AcquisitionFailure {
        reason_code: screen_reason(error),
    })
}

async fn run_inventory<T, F>(
    timeout: Duration,
    gate: Arc<Semaphore>,
    stop: StopSignal,
    operation: F,
) -> Result<T, AcquisitionFailure>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, InventoryError> + Send + 'static,
{
    let result = run_bounded_thread("harold-inventory", timeout, gate, stop, operation).await?;
    result.map_err(|error| AcquisitionFailure {
        reason_code: inventory_reason(error),
    })
}

async fn run_bounded_thread<T, F>(
    thread_name: &'static str,
    timeout: Duration,
    gate: Arc<Semaphore>,
    stop: StopSignal,
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
            let result = run_stoppable(stop, operation);
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

/// Tries a failed screen read once more, at once, with the longer `retry` deadline. The first
/// try, if still running, is asked to stop, and the retry waits within its own deadline for it
/// to let go of the gate, so the two never run together.
async fn read_with_retry<T, F, Fut>(
    pane_id: &str,
    gate: &Arc<Semaphore>,
    first: Duration,
    retry: Duration,
    mut attempt: F,
) -> Result<T, AcquisitionFailure>
where
    F: FnMut(Duration, StopSignal) -> Fut,
    Fut: Future<Output = Result<T, AcquisitionFailure>>,
{
    let started = tokio::time::Instant::now();
    let first_try = StopSignal::default();
    match attempt(first, first_try.clone()).await {
        Ok(value) => return Ok(value),
        // An earlier timed-out worker still owns the gate: there is no new read to retry.
        Err(failure) if failure.reason_code == "busy" => return Err(failure),
        // A closed pane has nothing to retry.
        Err(failure) if failure.reason_code == "pane_departed" => return Err(failure),
        Err(_) => first_try.request(),
    }
    let retry_started = tokio::time::Instant::now();
    let Ok(Ok(permit)) = tokio::time::timeout(retry, gate.acquire()).await else {
        return Err(AcquisitionFailure {
            reason_code: "timeout",
        });
    };
    drop(permit);
    let remaining = retry.saturating_sub(retry_started.elapsed());
    let retry_try = StopSignal::default();
    let result = attempt(remaining, retry_try.clone()).await;
    if result.is_err() {
        // A retry that timed out may still be running: stop it, so its child is reaped too.
        retry_try.request();
    }
    let value = result?;
    log_read_retried("screen", Some(pane_id), started);
    Ok(value)
}

/// The first read of an incarnation to find its pane closed. The inventory departs the pane
/// after two scans miss it.
fn log_pane_gone(tracked: &mut TrackedPane) {
    if !std::mem::replace(&mut tracked.pane_gone_logged, true) {
        tracing::info!(
            pane_id = tracked.pane.incarnation.pane_id.as_str(),
            "agent monitor pane gone"
        );
    }
}

/// A retried read that worked, `started` being when its first try began.
fn log_read_retried(component: &'static str, pane_id: Option<&str>, started: tokio::time::Instant) {
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    match pane_id {
        Some(pane_id) => {
            tracing::info!(component, pane_id, elapsed_ms, "agent monitor read retried");
        }
        None => tracing::info!(component, elapsed_ms, "agent monitor read retried"),
    }
}

/// `read_with_retry` with bare reason codes, for tests outside this module.
#[cfg(test)]
pub(crate) async fn read_with_retry_for_test<T, F, Fut>(
    pane_id: &str,
    gate: &Arc<Semaphore>,
    first: Duration,
    retry: Duration,
    mut attempt: F,
) -> Result<T, &'static str>
where
    F: FnMut(Duration, StopSignal) -> Fut,
    Fut: Future<Output = Result<T, &'static str>>,
{
    read_with_retry(pane_id, gate, first, retry, |timeout, stop| {
        let read = attempt(timeout, stop);
        async move {
            read.await
                .map_err(|reason_code| AcquisitionFailure { reason_code })
        }
    })
    .await
    .map_err(|failure| failure.reason_code)
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

pub(crate) fn valid_pane_id(value: &str) -> bool {
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

fn source_version(appended: &store::AppendResult, types: &[&str]) -> Option<EventVersion> {
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
                StopSignal::default(),
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
            StopSignal::default(),
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
                StopSignal::default(),
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
            run_bounded_thread(
                "test-inventory",
                Duration::from_secs(1),
                gate,
                StopSignal::default(),
                || ()
            )
            .await
            .is_ok()
        );
    }
}
