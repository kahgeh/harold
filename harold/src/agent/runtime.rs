use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::{Semaphore, mpsc, oneshot, watch};

use crate::settings::AgentProviderSettings;
use crate::store::{self, HaroldStore, TurnCompleted};

use super::domain::{
    AgentEvent, AgentIncarnation, AgentLifecycleObserved, AgentPaneDeparted, AgentPaneObservation,
    AgentPaneObserved, AgentScreenObserved, AgentSnapshot, EffectiveAgentState, ObservedAgentState,
    WorkSummaryUpdate,
};
use super::inventory::{AgentInventoryPort, InventoryError};
use super::screen::{ScreenError, VisibleScreenPort};
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
    pane: AgentPaneObservation,
    consecutive_absences: u8,
    last_hook: Option<(ObservedAgentState, i64)>,
    screen_state: Option<ObservedAgentState>,
    screen_summary: Option<String>,
    pending_screen_state: Option<ObservedAgentState>,
}

impl TrackedPane {
    fn new(pane: AgentPaneObservation) -> Self {
        Self {
            pane,
            consecutive_absences: 0,
            last_hook: None,
            screen_state: None,
            screen_summary: None,
            pending_screen_state: None,
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
    inventory_gate: Arc<Semaphore>,
    screen_gate: Arc<Semaphore>,
    panes: HashMap<String, TrackedPane>,
    health: HashMap<String, HealthState>,
}

pub(crate) struct AgentMonitorRuntimeConfig {
    pub(crate) inventory_interval: Duration,
    pub(crate) screen_interval: Duration,
    pub(crate) hook_grace_ms: u64,
    pub(crate) acquisition_timeout: Duration,
}

struct RuntimeInputs {
    store: Arc<HaroldStore>,
    inventory: Arc<dyn AgentInventoryPort>,
    screen: Arc<dyn VisibleScreenPort>,
    providers: Vec<AgentProviderSettings>,
    initial_snapshot: AgentSnapshot,
    hook_grace_ms: u64,
    acquisition_timeout: Duration,
    intervals: Option<(Duration, Duration)>,
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
            intervals: Some((config.inventory_interval, config.screen_interval)),
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
            intervals: None,
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
            intervals: None,
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
        intervals,
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
            inventory_gate: Arc::new(Semaphore::new(1)),
            screen_gate: Arc::new(Semaphore::new(1)),
            panes: panes_from_snapshot(&initial_snapshot),
            health: health_from_snapshot(&initial_snapshot),
        };
        loop {
            tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || shutdown.has_changed().unwrap_or(true) {
                        break;
                    }
                }
                command = receiver.recv() => {
                    let Some(command) = command else { break };
                    runtime.handle(command).await;
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

fn panes_from_snapshot(snapshot: &AgentSnapshot) -> HashMap<String, TrackedPane> {
    snapshot
        .panes
        .iter()
        .map(|projection| {
            let tracked = TrackedPane {
                pane: projection.pane.clone(),
                consecutive_absences: 0,
                last_hook: projection.hook_state.zip(projection.hook_observed_at_ms),
                screen_state: seeded_screen_state(projection),
                screen_summary: projection.screen_work_summary.clone(),
                pending_screen_state: None,
            };
            (projection.pane.incarnation.pane_id.clone(), tracked)
        })
        .collect()
}

fn seeded_screen_state(
    projection: &super::domain::AgentPaneProjection,
) -> Option<ObservedAgentState> {
    let screen_state = projection.screen_state?;
    let screen_is_effective = matches!(
        (screen_state, projection.effective_state),
        (ObservedAgentState::Busy, EffectiveAgentState::Busy)
            | (ObservedAgentState::Idle, EffectiveAgentState::Idle)
    );
    if projection.hook_state.is_some() && !screen_is_effective {
        return None;
    }
    Some(screen_state)
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
        let lifecycle = AgentLifecycleObserved {
            incarnation: pane.incarnation.clone(),
            state,
            adapter_id,
            work_summary,
            observed_at_ms,
        };
        store::append_agent_events(
            &self.store,
            vec![
                AgentEvent::PaneObserved(AgentPaneObserved { pane: pane.clone() }),
                AgentEvent::LifecycleObserved(lifecycle),
            ],
        )
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
        tracked.screen_state = None;
        tracked.pending_screen_state = None;
        Ok(())
    }

    async fn turn_completed(
        &mut self,
        mut turn: TurnCompleted,
    ) -> events::Result<events::AppendResult> {
        let pane = match resolve(
            Arc::clone(&self.inventory),
            turn.pane_id.clone(),
            self.acquisition_timeout,
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
            tracked.screen_state = None;
            tracked.pending_screen_state = None;
        }
        Ok(result)
    }

    async fn inventory_tick(&mut self) -> Result<(), MonitorCommandError> {
        let observed = match scan(
            Arc::clone(&self.inventory),
            self.acquisition_timeout,
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
                store::append_agent_events(
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
                    self.panes.insert(pane_id, TrackedPane::new(pane));
                }
            } else if let Some(tracked) = self.panes.get_mut(&pane_id) {
                tracked.consecutive_absences = 0;
            }
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
                self.acquisition_timeout,
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
            }
        }
        Ok(())
    }

    async fn screen_tick(&mut self) -> Result<(), MonitorCommandError> {
        let panes: Vec<AgentPaneObservation> = self
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
            let observation = match observe_screen(
                Arc::clone(&self.screen),
                pane.clone(),
                provider,
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
            let Some(tracked) = self.panes.get_mut(&pane.incarnation.pane_id) else {
                continue;
            };
            if tracked.pane.incarnation != observation.incarnation {
                continue;
            }

            let state = screen_state_delta(
                tracked,
                observation.state,
                observation.observed_at_ms,
                self.hook_grace_ms,
            );
            let summary = observation
                .fallback_summary
                .filter(|summary| tracked.screen_summary.as_deref() != Some(summary));
            if state.is_none() && summary.is_none() {
                continue;
            }
            let event = AgentScreenObserved {
                incarnation: observation.incarnation,
                state,
                classifier_id: observation.classifier_id,
                fallback_summary: summary.clone(),
                observed_at_ms: observation.observed_at_ms,
            };
            store::append_agent_events(&self.store, vec![AgentEvent::ScreenObserved(event)])
                .await
                .map_err(MonitorCommandError::EventAppend)?;
            if let Some(state) = state {
                tracked.screen_state = Some(state);
                tracked.pending_screen_state = None;
            }
            if summary.is_some() {
                tracked.screen_summary = summary;
            }
        }
        if let Some(reason_code) = failure_reason {
            self.set_health("screen", false, reason_code).await?;
        } else if attempted {
            self.set_health("screen", true, "ok").await?;
        }
        Ok(())
    }

    async fn resolve_pane(
        &mut self,
        pane_id: String,
    ) -> Result<Option<AgentPaneObservation>, MonitorCommandError> {
        match resolve(
            Arc::clone(&self.inventory),
            pane_id,
            self.acquisition_timeout,
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
        let next = HealthState {
            healthy,
            reason_code: reason_code.into(),
        };
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
}

fn screen_state_delta(
    tracked: &mut TrackedPane,
    observed: Option<ObservedAgentState>,
    observed_at_ms: i64,
    hook_grace_ms: u64,
) -> Option<ObservedAgentState> {
    let state = observed?;
    if let Some((hook_state, hook_observed_at_ms)) = tracked.last_hook {
        let grace = i64::try_from(hook_grace_ms).unwrap_or(i64::MAX);
        if observed_at_ms < hook_observed_at_ms.saturating_add(grace) {
            if state != hook_state {
                tracked.pending_screen_state = Some(state);
            }
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
    run_inventory(timeout, gate, move || inventory.scan()).await
}

async fn resolve(
    inventory: Arc<dyn AgentInventoryPort>,
    pane_id: String,
    timeout: Duration,
    gate: Arc<Semaphore>,
) -> Result<Option<AgentPaneObservation>, AcquisitionFailure> {
    run_inventory(timeout, gate, move || inventory.resolve(&pane_id)).await
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
        reason_code: "timeout",
    })?;
    let (sender, receiver) = oneshot::channel();
    std::thread::Builder::new()
        .name(thread_name.into())
        .spawn(move || {
            let _permit = permit;
            let _ = sender.send(operation());
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
