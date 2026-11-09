//! What the master keeps of its slaves: the link state, the agents of each linked host and
//! each slave's last lock. It is the `MasterSink` of the slave link listener.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use harold_api::harold::slave::v1::Turn;
use harold_api::harold::{AgentPaneState, AgentState, AgentStateSnapshot, TurnCompleteRequest};
use tracing::warn;

use super::master::{Applied, MasterSink};
use crate::agent::domain::ObservedAgentState;
use crate::agent::pane_route::{host_pane_id, parse_host_pane_id, valid_host_label};
use crate::agent::rex::{BLOCK_PREFIX, valid_rex_pane_id};
use crate::agent::rex_host_runner::{HostAgents, HostReport};
use crate::agent::rex_hosts::{HostAgentState, RexHostAgent};
use crate::tools::job::JobsFeed;

/// The result of the master's turn-end path for one turn.
pub(crate) type TurnFuture<'a> = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

/// The master's turn-end path. `main.rs` implements it with the code that
/// `TurnComplete` uses.
pub(crate) trait TurnSink: Send + Sync + 'static {
    fn record(&self, request: TurnCompleteRequest) -> TurnFuture<'_>;
}

#[derive(Default)]
struct Entry {
    /// The slave's last `Lock`. `None` until the first one.
    locked: Option<bool>,
}

pub(crate) struct SlaveRegistry {
    hosts: HostAgents,
    turns: Arc<dyn TurnSink>,
    slaves: Mutex<HashMap<String, Entry>>,
    /// Where the jobs of the slaves go. The snapshot that the master serves reads it.
    jobs: Arc<JobsFeed>,
}

impl SlaveRegistry {
    pub(crate) fn new(hosts: HostAgents, turns: Arc<dyn TurnSink>) -> Self {
        Self {
            hosts,
            turns,
            slaves: Mutex::new(HashMap::new()),
            jobs: Arc::clone(JobsFeed::global()),
        }
    }

    /// Puts the jobs of the slaves in `jobs` and not in the feed of this process. Tests use it.
    #[cfg(test)]
    pub(crate) fn with_jobs_feed(mut self, jobs: Arc<JobsFeed>) -> Self {
        self.jobs = jobs;
        self
    }

    /// Whether the slave link of `label` is up. The Rex inventory skips such a label: it reads
    /// the same flag from `HostAgents`, where `link_changed` sets it.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn is_linked(&self, label: &str) -> bool {
        self.hosts.is_linked(label)
    }

    /// The last lock of the slave `label`. `None` until its first `Lock`.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn lock_of(&self, label: &str) -> Option<bool> {
        self.entries().get(label).and_then(|entry| entry.locked)
    }

    /// For a host pane id of a slave this master knows: whether the screen of that slave is
    /// locked. Until its first `Lock` the slave counts unlocked: the master never claims a
    /// lock it was not told. `None` for any other pane id.
    pub(crate) fn pane_lock(&self, pane_id: &str) -> Option<bool> {
        let (label, _) = parse_host_pane_id(pane_id)?;
        self.entries()
            .get(label)
            .map(|entry| entry.locked.unwrap_or(false))
    }

    fn entries(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        self.slaves.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl MasterSink for SlaveRegistry {
    /// Replaces the agents of the host with those of the snapshot. The recap never crosses
    /// the link: the slave clears it before it sends (`agent::slave_feed`), and a row holds none.
    fn apply_snapshot(&self, slave: &str, snapshot: AgentStateSnapshot) {
        if !valid_host_label(slave) {
            warn!("slave snapshot dropped: the slave name is not a host label");
            return;
        }
        let now_ms = epoch_ms();
        // The jobs of the slave go into the snapshot that this master serves.
        self.jobs
            .set_slave_jobs(slave, snapshot.jobs.clone(), now_ms);
        let mut rows: Vec<RexHostAgent> = Vec::new();
        let mut icon = String::new();
        for pane in &snapshot.panes {
            let Some(agent) = host_agent(slave, pane) else {
                continue;
            };
            if icon.is_empty() {
                icon.clone_from(&pane.host_icon);
            }
            // One pane id is one row: the last of the snapshot wins, in the place of the first.
            match rows
                .iter_mut()
                .find(|row| row.host_pane_id == agent.host_pane_id)
            {
                Some(row) => *row = agent,
                None => rows.push(agent),
            }
        }
        let known = self.hosts.icon_of(slave);
        if !known.is_empty() {
            icon = known;
        }
        self.hosts.record(
            slave,
            HostReport::Reached {
                agents: rows,
                host_id: None,
                icon,
            },
            now_ms,
        );
    }

    /// Takes the turn into the same code as a hook's `TurnComplete`, with the host pane id.
    /// It fails only when that code cannot record the turn.
    fn apply_turn(&self, slave: &str, turn: Turn) -> Applied {
        let Some(request) = turn_request(slave, turn) else {
            // The slave can never fix this report: keep the number and move on.
            warn!(slave, "slave turn dropped: no valid pane id");
            return Applied::Applied;
        };
        let turns = Arc::clone(&self.turns);
        match block_on(async move { turns.record(request).await }) {
            Some(Ok(())) => Applied::Applied,
            Some(Err(_)) | None => Applied::Failed,
        }
    }

    fn apply_lock(&self, slave: &str, locked: bool) -> Applied {
        self.entries().entry(slave.to_owned()).or_default().locked = Some(locked);
        Applied::Applied
    }

    fn link_changed(&self, slave: &str, up: bool) {
        if up {
            self.entries().entry(slave.to_owned()).or_default();
        }
        self.hosts.set_linked(slave, up);
        self.jobs.slave_link(slave, up, epoch_ms());
    }
}

/// Runs `future` to its end from a sync call made on a worker of the multi-thread runtime.
/// `None` when no such runtime runs the caller.
fn block_on<T>(future: impl Future<Output = T>) -> Option<T> {
    let handle = tokio::runtime::Handle::try_current().ok()?;
    if handle.runtime_flavor() != tokio::runtime::RuntimeFlavor::MultiThread {
        warn!("slave turn not applied: the runtime has one thread");
        return None;
    }
    Some(tokio::task::block_in_place(|| handle.block_on(future)))
}

/// The master's request for a slave's turn, with the host pane id. `None` when the slave
/// name or the pane id has no valid form.
fn turn_request(slave: &str, turn: Turn) -> Option<TurnCompleteRequest> {
    if !valid_host_label(slave) || !valid_rex_pane_id(&turn.pane_id) {
        return None;
    }
    let block = turn.pane_id.strip_prefix(BLOCK_PREFIX)?;
    Some(TurnCompleteRequest {
        pane_id: host_pane_id(slave, block),
        pane_label: turn.pane_label,
        last_user_prompt: turn.last_user_prompt,
        assistant_message: turn.assistant_message,
        main_context: turn.main_context,
        session_id: turn.session_id,
        turn_started_at: turn.turn_started_at,
        // A path on the slave means nothing here.
        transcript_path: String::new(),
        input_tokens: turn.input_tokens,
        output_tokens: turn.output_tokens,
        cache_read_tokens: turn.cache_read_tokens,
        cache_write_tokens: turn.cache_write_tokens,
        model: turn.model,
    })
}

/// One pane of a slave's snapshot as an agent of the host `label`. `None` for a pane that
/// is not a Rex block, or has no agent.
fn host_agent(label: &str, pane: &AgentPaneState) -> Option<RexHostAgent> {
    if !valid_rex_pane_id(&pane.pane_id) || pane.agent_pid == 0 {
        return None;
    }
    let block = pane.pane_id.strip_prefix(BLOCK_PREFIX)?;
    let state = match AgentState::try_from(pane.state) {
        Ok(AgentState::Working) => HostAgentState::Observed(ObservedAgentState::Working),
        Ok(AgentState::Blocked) => HostAgentState::Observed(ObservedAgentState::Blocked),
        Ok(AgentState::Done) => HostAgentState::Observed(ObservedAgentState::Done),
        Ok(AgentState::Idle) => HostAgentState::Observed(ObservedAgentState::Idle),
        Ok(AgentState::Error) => HostAgentState::Observed(ObservedAgentState::Error),
        Ok(AgentState::Unspecified | AgentState::Unknown) | Err(_) => HostAgentState::Unknown,
    };
    Some(RexHostAgent {
        host: label.to_owned(),
        host_pane_id: host_pane_id(label, block),
        pid: pane.agent_pid,
        first_seen_ms: pane.agent_started_at_ms,
        state,
        current_step: pane.current_step.clone(),
        provider_id: pane.provider_id.clone(),
        provider_display_name: pane.provider_display_name.clone(),
        session_name: pane.session_name.clone(),
        window_index: pane.window_index,
        pane_index: pane.pane_index,
        window_name: pane.window_name.clone(),
        working_directory: String::new(),
    })
}

fn epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(i64::MAX)
}

static GLOBAL: OnceLock<Arc<SlaveRegistry>> = OnceLock::new();

/// Keeps the registry for the delivery code, which has no handle to it.
pub(crate) fn install(registry: Arc<SlaveRegistry>) {
    let _ = GLOBAL.set(registry);
}

/// The lock that decides for the agent at `pane_id`, when the pane is on a linked slave.
pub(crate) fn slave_lock_for_pane(pane_id: &str) -> Option<bool> {
    GLOBAL.get()?.pane_lock(pane_id)
}

#[cfg(test)]
#[path = "registry_tests.rs"]
mod registry_tests;
