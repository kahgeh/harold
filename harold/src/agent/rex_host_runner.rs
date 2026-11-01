//! Reads each Rex host in `rex_hosts` on its own task, and keeps the agents in memory.
//!
//! The Mac's own scan never waits for a host. Each host has one tokio task. The task runs
//! each read on a blocking thread with an overall limit. After 3 failed reads in a row,
//! the task reads the host only every 30 seconds. The agents go to `HostAgents` only: not
//! to the reducer, the event log or `agent_panes`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::info;

use super::domain::ObservedAgentState;
use super::rex::RexError;
use super::rex_hosts::{HostAgentState, HostClock, HostScan, RexHostAgent, RexHostInventory};
use super::screen::{StopSignal, run_stoppable};
use crate::settings::{Multiplexer, Settings};
use crate::telemetry::AgentCount;

/// The longest time one read of a host can take. The calls run one after another, so a
/// host with many blocks on a slow link can need more than one call's limit.
pub(crate) const SCAN_BUDGET: Duration = Duration::from_secs(10);
/// The failed reads in a row after which the host is read less often.
const FAILURES_BEFORE_BACKOFF: u32 = 3;
/// The time between two reads of a host that keeps failing.
const BACKOFF_INTERVAL: Duration = Duration::from_secs(30);

/// One Rex host that Harold can read. `RexHostInventory` is the real one.
pub(crate) trait HostSource: Send + Sync {
    fn label(&self) -> &str;
    /// One read of the host. It blocks, so the runner calls it on a blocking thread.
    fn scan(&self) -> HostScan;
    fn icon(&self) -> String;
    /// Whether Rex knows the label. `None` when Harold cannot tell.
    fn listed(&self) -> Option<bool>;
}

impl HostSource for RexHostInventory {
    fn label(&self) -> &str {
        self.label()
    }

    fn scan(&self) -> HostScan {
        self.scan()
    }

    fn icon(&self) -> String {
        self.icon()
    }

    fn listed(&self) -> Option<bool> {
        self.listed()
    }
}

/// One agent on a Rex host, as the `WatchAgentStates` stream shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HostAgentRow {
    pub(crate) agent: RexHostAgent,
    /// When the shown state last changed: first the first-seen time.
    pub(crate) last_transition_at_ms: i64,
    /// The icon name of the agent's host.
    pub(crate) icon: String,
}

/// Why a read of a host failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unreached {
    Timeout,
    Failed,
    UnknownHost,
}

impl Unreached {
    fn reason(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::Failed => "failed",
            Self::UnknownHost => "unknown_host",
        }
    }
}

/// The result of one read of a host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum HostReport {
    Reached {
        agents: Vec<RexHostAgent>,
        host_id: Option<String>,
        icon: String,
    },
    Unreached {
        reason: Unreached,
        host_id: Option<String>,
    },
}

/// The last line logged for a host, so each change logs once.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Logged {
    Reachable,
    Unreachable(Unreached),
    LeftOut,
}

#[derive(Default)]
struct Host {
    /// The agents of the last good read.
    agents: Vec<RexHostAgent>,
    reachable: bool,
    host_id: Option<String>,
    icon: String,
    left_out: bool,
    /// For each host pane id: the pid, the shown state and when that state started.
    transitions: HashMap<String, (u32, HostAgentState, i64)>,
    logged: Option<Logged>,
}

struct Inner {
    labels: Vec<String>,
    hosts: Mutex<HashMap<String, Host>>,
    version: watch::Sender<u64>,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            labels: Vec::new(),
            hosts: Mutex::new(HashMap::new()),
            version: watch::channel(0).0,
        }
    }
}

/// The agents on the Rex hosts, in memory only. The version rises when the rows change.
#[derive(Clone, Default)]
pub(crate) struct HostAgents {
    inner: Arc<Inner>,
}

impl HostAgents {
    /// The store for the hosts `labels`, in `rex_hosts` order.
    pub(crate) fn new(labels: Vec<String>) -> Self {
        Self {
            inner: Arc::new(Inner {
                labels,
                ..Inner::default()
            }),
        }
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<u64> {
        self.inner.version.subscribe()
    }

    /// The agents of every host that is not left out, in `rex_hosts` order.
    pub(crate) fn snapshot(&self) -> Vec<HostAgentRow> {
        let hosts = self.lock();
        rows(&self.inner.labels, &hosts)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Host>> {
        self.inner
            .hosts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn is_left_out(&self, label: &str) -> bool {
        self.lock().get(label).is_some_and(|host| host.left_out)
    }

    /// Keeps the result of one read of the host `label` at the time `now_ms`.
    pub(crate) fn record(&self, label: &str, report: HostReport, now_ms: i64) {
        let changed = {
            let mut hosts = self.lock();
            let before = rows(&self.inner.labels, &hosts);
            let host = hosts.entry(label.to_string()).or_default();
            if host.left_out {
                return;
            }
            match report {
                HostReport::Reached {
                    agents,
                    host_id,
                    icon,
                } => {
                    host.reachable = true;
                    host.host_id = host_id.or(host.host_id.take());
                    host.icon = icon;
                    host.transitions = agents
                        .iter()
                        .map(|agent| {
                            let since = match host.transitions.get(&agent.host_pane_id) {
                                Some((pid, state, since)) if *pid == agent.pid => {
                                    if *state == agent.state {
                                        *since
                                    } else {
                                        now_ms
                                    }
                                }
                                _ => agent.first_seen_ms,
                            };
                            (agent.host_pane_id.clone(), (agent.pid, agent.state, since))
                        })
                        .collect();
                    host.agents = agents;
                    log_change(label, host, Logged::Reachable);
                }
                HostReport::Unreached { reason, host_id } => {
                    host.reachable = false;
                    host.host_id = host_id.or(host.host_id.take());
                    for (_, state, since) in host.transitions.values_mut() {
                        if *state != HostAgentState::Unknown {
                            *state = HostAgentState::Unknown;
                            *since = now_ms;
                        }
                    }
                    log_change(label, host, Logged::Unreachable(reason));
                }
            }
            leave_out_duplicates(&self.inner.labels, &mut hosts);
            rows(&self.inner.labels, &hosts) != before
        };
        if changed {
            self.note_counts();
            self.inner.version.send_modify(|version| *version += 1);
        }
    }

    /// Gives `harold.agents` the count of the agents on each host, by state and provider.
    fn note_counts(&self) {
        let rows = self.snapshot();
        for label in &self.inner.labels {
            let own = rows.iter().filter(|row| row.agent.host == *label);
            crate::telemetry::note_host_agent_counts(label, host_agent_counts(own));
        }
    }
}

/// The agents in each state for each provider. A state with no agent counts 0, as for the
/// Mac's agents. An agent whose state is unknown counts as `unknown`.
fn host_agent_counts<'a>(rows: impl Iterator<Item = &'a HostAgentRow> + Clone) -> Vec<AgentCount> {
    let mut counts: BTreeMap<(&'static str, &str), u64> = BTreeMap::new();
    for row in rows.clone() {
        for state in ["working", "idle", "unknown"] {
            counts
                .entry((state, row.agent.provider_id.as_str()))
                .or_default();
        }
    }
    for row in rows {
        let state = match row.agent.state {
            HostAgentState::Observed(ObservedAgentState::Working) => "working",
            HostAgentState::Observed(ObservedAgentState::Idle) => "idle",
            HostAgentState::Unknown => "unknown",
        };
        *counts
            .entry((state, row.agent.provider_id.as_str()))
            .or_default() += 1;
    }
    counts
        .into_iter()
        .map(|((state, provider_id), count)| AgentCount {
            state,
            provider_id: provider_id.to_owned(),
            count,
        })
        .collect()
}

fn rows(labels: &[String], hosts: &HashMap<String, Host>) -> Vec<HostAgentRow> {
    labels
        .iter()
        .filter_map(|label| hosts.get(label))
        .filter(|host| !host.left_out)
        .flat_map(|host| {
            host.agents.iter().map(|agent| {
                let mut agent = agent.clone();
                let since = host
                    .transitions
                    .get(&agent.host_pane_id)
                    .map_or(agent.first_seen_ms, |(_, _, since)| *since);
                if !host.reachable {
                    // The last good read is old: its state and step may no longer hold.
                    agent.state = HostAgentState::Unknown;
                    agent.current_step = None;
                }
                HostAgentRow {
                    agent,
                    last_transition_at_ms: since,
                    icon: host.icon.clone(),
                }
            })
        })
        .collect()
}

/// Leaves out a later label whose `host_id` an earlier label in `rex_hosts` has too.
fn leave_out_duplicates(labels: &[String], hosts: &mut HashMap<String, Host>) {
    let mut seen_ids = HashSet::new();
    for label in labels {
        let Some(host) = hosts.get_mut(label) else {
            continue;
        };
        let Some(host_id) = host.host_id.clone() else {
            continue;
        };
        if seen_ids.insert(host_id) {
            continue;
        }
        if !host.left_out {
            host.left_out = true;
            log_change(label, host, Logged::LeftOut);
        }
    }
}

/// Logs `agent monitor host` when the host's state differs from the last line.
fn log_change(label: &str, host: &mut Host, next: Logged) {
    if host.logged == Some(next) {
        return;
    }
    host.logged = Some(next);
    match next {
        Logged::Reachable => info!(host = %label, state = "reachable", "agent monitor host"),
        Logged::Unreachable(reason) => info!(
            host = %label,
            state = "unreachable",
            reason = reason.reason(),
            "agent monitor host"
        ),
        Logged::LeftOut => info!(
            host = %label,
            state = "left_out",
            reason = "duplicate_host",
            "agent monitor host"
        ),
    }
}

/// Starts one task for each host in `[agent_monitor] rex_hosts`. With no host, or with
/// tmux, it starts nothing and the store stays empty.
pub(crate) fn start(settings: &Settings, shutdown: watch::Receiver<()>) -> HostAgents {
    let monitor = &settings.agent_monitor;
    if monitor.multiplexer() != Multiplexer::Rex || monitor.rex_hosts.is_empty() {
        return HostAgents::default();
    }
    let clock: HostClock = Arc::new(epoch_ms);
    let sources = monitor
        .rex_hosts
        .iter()
        .map(|label| {
            Arc::new(RexHostInventory::new(
                label.clone(),
                settings.agents.clone(),
                monitor.rex_host_marks.get(label).cloned(),
                Arc::clone(&clock),
            )) as Arc<dyn HostSource>
        })
        .collect();
    let interval = Duration::from_millis(monitor.inventory_interval_ms);
    spawn_hosts(sources, interval, clock, shutdown).0
}

/// Starts one task for each source, in the order of `sources`. Each task stops at shutdown.
pub(crate) fn spawn_hosts(
    sources: Vec<Arc<dyn HostSource>>,
    interval: Duration,
    clock: HostClock,
    shutdown: watch::Receiver<()>,
) -> (HostAgents, Vec<JoinHandle<()>>) {
    let agents = HostAgents::new(
        sources
            .iter()
            .map(|source| source.label().to_string())
            .collect(),
    );
    let tasks = sources
        .into_iter()
        .map(|source| {
            tokio::spawn(run_host(
                source,
                agents.clone(),
                interval,
                Arc::clone(&clock),
                shutdown.clone(),
            ))
        })
        .collect();
    (agents, tasks)
}

async fn run_host(
    source: Arc<dyn HostSource>,
    agents: HostAgents,
    interval: Duration,
    clock: HostClock,
    mut shutdown: watch::Receiver<()>,
) {
    let label = source.label().to_string();
    // The reads stop with the task: at shutdown, the child command of a read ends at once.
    let stop = StopOnDrop(StopSignal::default());
    let mut failures = 0_u32;
    // A read that went past the budget. The next read starts only after it ends.
    let mut late: Option<JoinHandle<HostReport>> = None;
    loop {
        if agents.is_left_out(&label) {
            break;
        }
        if let Some(read) = late.take() {
            tokio::select! {
                biased;
                _ = shutdown.changed() => break,
                _ = read => {}
            }
        }
        let reader = Arc::clone(&source);
        let signal = stop.0.clone();
        let mut read = tokio::task::spawn_blocking(move || {
            run_stoppable(signal, || read_host(reader.as_ref()))
        });
        let report = tokio::select! {
            biased;
            _ = shutdown.changed() => break,
            result = tokio::time::timeout(SCAN_BUDGET, &mut read) => match result {
                Ok(Ok(report)) => report,
                Ok(Err(_)) => unreached(Unreached::Failed),
                Err(_) => {
                    late = Some(read);
                    unreached(Unreached::Timeout)
                }
            },
        };
        failures = match report {
            HostReport::Reached { .. } => 0,
            HostReport::Unreached { .. } => failures.saturating_add(1),
        };
        agents.record(&label, report, clock());
        let wait = if failures >= FAILURES_BEFORE_BACKOFF {
            BACKOFF_INTERVAL
        } else {
            interval
        };
        tokio::select! {
            biased;
            _ = shutdown.changed() => break,
            () = tokio::time::sleep(wait) => {}
        }
    }
}

/// Asks the reads of a host task to stop when the task ends.
struct StopOnDrop(StopSignal);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.request();
    }
}

fn unreached(reason: Unreached) -> HostReport {
    HostReport::Unreached {
        reason,
        host_id: None,
    }
}

/// One read of the host, on a blocking thread.
fn read_host(source: &dyn HostSource) -> HostReport {
    let HostScan {
        agents,
        host_id,
        outcome,
    } = source.scan();
    match outcome {
        Ok(()) => HostReport::Reached {
            agents,
            host_id,
            icon: source.icon(),
        },
        Err(error) => {
            let reason = if source.listed() == Some(false) {
                Unreached::UnknownHost
            } else if error == RexError::Timeout {
                Unreached::Timeout
            } else {
                Unreached::Failed
            };
            HostReport::Unreached { reason, host_id }
        }
    }
}

fn epoch_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(i64::MAX)
}
