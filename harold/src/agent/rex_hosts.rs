//! The agents on another Rex host. Read only.
//!
//! `RexHostInventory` reads one host through `rex -S <label>`. It has read calls only: no
//! paste, no key and no screen read exists for a host. It keeps its result in memory.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::domain::{
    EffectiveAgentState, ObservedAgentState, UNKNOWN_PROVIDER_DISPLAY_NAME, UNKNOWN_PROVIDER_ID,
};
use super::inventory::{ProviderResolution, resolve_provider};
use super::pane_route::host_pane_id;
use super::rex::{
    BLOCK_PREFIX, RexBlockSlot, RexError, find_agent, parse_json, parse_process,
    parse_session_view, parse_sessions, run_program, slots_of,
};
use super::rex::{StatusRecord, parse_program_status};
use super::screen::stop_requested;
use crate::settings::AgentSettings;
use crate::telemetry::{MultiplexerOperation, MultiplexerOutcome, record_call};

/// A call to a host that takes longer than this fails.
const HOST_TIMEOUT: Duration = Duration::from_secs(2);
/// The same limit as the text that `rex --timeout` takes.
const HOST_TIMEOUT_ARG: &str = "2s";
/// The shortest time between two `whoami` calls to a host that does not answer them.
const WHOAMI_RETRY_MS: i64 = 30_000;
/// The shortest time between two icon reads.
const ICON_INTERVAL_MS: i64 = 60_000;

/// The state of an agent on a Rex host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HostAgentState {
    Observed(ObservedAgentState),
    Unknown,
}

/// The host a call records. A call with no server flag goes to this Mac: its host is empty.
pub(super) fn recorded_host(with_server: bool, label: &str) -> &str {
    if with_server { label } else { "" }
}

impl HostAgentState {
    /// The effective state of a host agent. A host agent with no record is `Unknown`.
    pub(crate) fn effective(self) -> EffectiveAgentState {
        match self {
            Self::Observed(state) => state.effective(),
            Self::Unknown => EffectiveAgentState::Unknown,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RexHostAgent {
    pub(crate) host: String,
    pub(crate) host_pane_id: String,
    pub(crate) pid: u32,
    pub(crate) first_seen_ms: i64,
    pub(crate) state: HostAgentState,
    pub(crate) current_step: Option<String>,
    pub(crate) provider_id: String,
    pub(crate) provider_display_name: String,
    pub(crate) session_name: String,
    pub(crate) window_index: u32,
    pub(crate) pane_index: u32,
    pub(crate) working_directory: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HostScan {
    pub(crate) agents: Vec<RexHostAgent>,
    pub(crate) host_id: Option<String>,
    pub(crate) outcome: Result<(), RexError>,
}

pub(crate) type HostClock = Arc<dyn Fn() -> i64 + Send + Sync>;
pub(crate) type HostProgram = Arc<dyn Fn() -> Result<PathBuf, RexError> + Send + Sync>;

/// What Harold last saw in one block: the agent pid and when Harold first saw it.
struct Seen {
    pid: u32,
    first_seen_ms: i64,
}

#[derive(Default)]
struct IconState {
    value: String,
    /// When the last read started. `None` before the first read.
    last_read_ms: Option<i64>,
    /// Whether the last host list that Harold could read names the label. `None` before
    /// the first read that works.
    listed: Option<bool>,
}

pub(crate) struct RexHostInventory {
    label: String,
    settings: AgentSettings,
    mark_override: Option<String>,
    clock: HostClock,
    program: HostProgram,
    seen: Mutex<HashMap<String, Seen>>,
    host_id: Mutex<Option<String>>,
    /// The time before which a failed `whoami` is not tried again.
    whoami_retry_at_ms: Mutex<Option<i64>>,
    icon: Mutex<IconState>,
}

impl RexHostInventory {
    /// An inventory that reads the host `label` with the daemon's `rex` program.
    pub(crate) fn new(
        label: String,
        settings: AgentSettings,
        mark_override: Option<String>,
        clock: HostClock,
    ) -> Self {
        let rex = super::rex::system();
        Self::with_program(
            label,
            settings,
            mark_override,
            clock,
            Arc::new(move || rex.program()),
        )
    }

    pub(crate) fn with_program(
        label: String,
        settings: AgentSettings,
        mark_override: Option<String>,
        clock: HostClock,
        program: HostProgram,
    ) -> Self {
        Self {
            label,
            settings,
            mark_override,
            clock,
            program,
            seen: Mutex::new(HashMap::new()),
            host_id: Mutex::new(None),
            whoami_retry_at_ms: Mutex::new(None),
            icon: Mutex::new(IconState::default()),
        }
    }

    pub(crate) fn label(&self) -> &str {
        &self.label
    }

    /// Runs `rex` with the label and the limit, and counts the call under `operation`.
    /// `with_server` is false for a call that the home of this Mac answers.
    fn call(
        &self,
        operation: MultiplexerOperation,
        with_server: bool,
        args: &[&str],
    ) -> Result<String, RexError> {
        let started = Instant::now();
        let result = (self.program)().and_then(|program| {
            let mut all = Vec::new();
            if with_server {
                all.extend(["-S", self.label.as_str()]);
            }
            all.extend(["--timeout", HOST_TIMEOUT_ARG]);
            all.extend_from_slice(args);
            run_program(&program, &all, HOST_TIMEOUT)
        });
        let outcome = match &result {
            Ok(_) => MultiplexerOutcome::Ok,
            Err(_) if stop_requested() => MultiplexerOutcome::Timeout,
            Err(RexError::Absent) => MultiplexerOutcome::Absent,
            Err(RexError::Timeout) => MultiplexerOutcome::Timeout,
            Err(RexError::Failed) => MultiplexerOutcome::Failed,
            Err(RexError::Malformed) => MultiplexerOutcome::Malformed,
        };
        record_call(
            "rex",
            recorded_host(with_server, &self.label),
            operation,
            outcome,
            started.elapsed(),
        );
        result
    }

    /// The `host_id` of the host's server. It is read until one read works. After a failed
    /// read, the next try waits 30 seconds: an older Rex has no `whoami --json`.
    fn read_host_id(&self) -> Option<String> {
        if let Some(id) = self
            .host_id
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
        {
            return Some(id);
        }
        let now = (self.clock)();
        let mut retry_at = self
            .whoami_retry_at_ms
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if retry_at.is_some_and(|at| now < at) {
            return None;
        }
        let id = self.whoami(true);
        match &id {
            Some(id) => {
                *self.host_id.lock().unwrap_or_else(PoisonError::into_inner) = Some(id.clone());
            }
            None => *retry_at = Some(now.saturating_add(WHOAMI_RETRY_MS)),
        }
        id
    }

    /// The `host_id` of the Rex server on this Mac, from `rex whoami --json` with no
    /// server. It is a read: it changes nothing.
    pub(crate) fn local_host_id(&self) -> Option<String> {
        self.whoami(false)
    }

    /// One `rex whoami --json` call, on the host's server or on this Mac's.
    fn whoami(&self, with_server: bool) -> Option<String> {
        let text = self
            .call(
                MultiplexerOperation::Other,
                with_server,
                &["whoami", "--json"],
            )
            .ok()?;
        parse_json(&text)
            .ok()?
            .get("host_id")?
            .as_str()
            .filter(|id| !id.is_empty())
            .map(str::to_string)
    }

    fn slots(&self) -> Result<Vec<RexBlockSlot>, RexError> {
        let sessions =
            parse_sessions(&self.call(MultiplexerOperation::List, true, &["ls", "--json"])?)?;
        let mut slots = Vec::new();
        for session in &sessions {
            let args = json!({"session_id": session.id}).to_string();
            let view = self.call(
                MultiplexerOperation::Layout,
                true,
                &["api", "call", "session.view", &args],
            )?;
            slots.extend(slots_of(session, &parse_session_view(&view)?));
        }
        Ok(slots)
    }

    /// The record of the program in a block, with the agent's state and step.
    fn record(&self, slot: &RexBlockSlot) -> Result<Option<StatusRecord>, RexError> {
        let text = self.call(
            MultiplexerOperation::Status,
            true,
            &[
                "block",
                "call",
                "-s",
                &slot.session_id,
                "-b",
                &slot.block_id,
                "program_status",
            ],
        )?;
        parse_program_status(&text)
    }

    fn scan_agents(&self) -> Result<Vec<RexHostAgent>, RexError> {
        let slots = self.slots()?;
        let now = (self.clock)();
        let mut found = Vec::new();
        for slot in &slots {
            let text = self.call(
                MultiplexerOperation::Process,
                true,
                &[
                    "block",
                    "call",
                    "-s",
                    &slot.session_id,
                    "-b",
                    &slot.block_id,
                    "process",
                ],
            )?;
            let process = parse_process(&text)?;
            let Some((agent, program)) = find_agent(&process, &self.settings) else {
                continue;
            };
            // A failed status call gives this agent no record, so its state is unknown.
            // The other agents keep theirs.
            let record = self
                .record(slot)
                .ok()
                .flatten()
                .filter(|record| record.pid == agent.pid);
            let (provider_id, provider_display_name) =
                match resolve_provider(&self.settings, &program) {
                    ProviderResolution::Matched {
                        provider_id,
                        provider_display_name,
                    } => (provider_id, provider_display_name),
                    _ => (
                        UNKNOWN_PROVIDER_ID.to_string(),
                        UNKNOWN_PROVIDER_DISPLAY_NAME.to_string(),
                    ),
                };
            let id = slot
                .block_id
                .strip_prefix(BLOCK_PREFIX)
                .unwrap_or(&slot.block_id);
            found.push((
                slot.block_id.clone(),
                RexHostAgent {
                    host: self.label.clone(),
                    host_pane_id: host_pane_id(&self.label, id),
                    pid: agent.pid,
                    first_seen_ms: now,
                    state: record.as_ref().map_or(HostAgentState::Unknown, |record| {
                        HostAgentState::Observed(record.state.observed())
                    }),
                    current_step: record.and_then(|record| record.msg),
                    provider_id,
                    provider_display_name,
                    session_name: slot.session_label.clone(),
                    window_index: slot.window,
                    pane_index: slot.pane,
                    working_directory: agent.cwd.clone(),
                },
            ));
        }
        // A new pid in a block is a new incarnation. A block with no agent forgets its pid.
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        let mut next = HashMap::new();
        let mut agents = Vec::new();
        for (block_id, mut agent) in found {
            let first_seen_ms = match seen.get(&block_id) {
                Some(old) if old.pid == agent.pid => old.first_seen_ms,
                _ => agent.first_seen_ms,
            };
            agent.first_seen_ms = first_seen_ms;
            next.insert(
                block_id,
                Seen {
                    pid: agent.pid,
                    first_seen_ms,
                },
            );
            agents.push(agent);
        }
        *seen = next;
        Ok(agents)
    }

    /// One read of the host: its agents and its `host_id`. A failed read gives no agents
    /// and keeps what Harold saw before.
    pub(crate) fn scan(&self) -> HostScan {
        let host_id = self.read_host_id();
        match self.scan_agents() {
            Ok(agents) => HostScan {
                agents,
                host_id,
                outcome: Ok(()),
            },
            Err(error) => HostScan {
                agents: Vec::new(),
                host_id,
                outcome: Err(error),
            },
        }
    }

    /// The icon name of the host: the mark from the settings, else the Rex icon. The Rex
    /// icon is read at most once a minute. A failed read keeps the last value.
    pub(crate) fn icon(&self) -> String {
        if let Some(mark) = &self.mark_override {
            return mark.clone();
        }
        let mut state = self.icon.lock().unwrap_or_else(PoisonError::into_inner);
        self.read_host_list(&mut state);
        state.value.clone()
    }

    /// Whether `rex hosts list` names the label: `Some(false)` when Rex does not know the
    /// host, `None` when no read of the list worked. It shares the icon's once-a-minute read.
    pub(crate) fn listed(&self) -> Option<bool> {
        let mut state = self.icon.lock().unwrap_or_else(PoisonError::into_inner);
        self.read_host_list(&mut state);
        state.listed
    }

    /// Reads the host list when the last read is a minute old or more.
    fn read_host_list(&self, state: &mut IconState) {
        let now = (self.clock)();
        let due = state
            .last_read_ms
            .is_none_or(|last| now.saturating_sub(last) >= ICON_INTERVAL_MS);
        if !due {
            return;
        }
        state.last_read_ms = Some(now);
        let read = self
            .call(
                MultiplexerOperation::Other,
                false,
                &["hosts", "list", "--json"],
            )
            .and_then(|text| parse_icon(&text, &self.label));
        if let Ok(entry) = read {
            state.listed = Some(entry.is_some());
            state.value = entry.unwrap_or_default();
        }
    }
}

/// The icon of the host `label` in the output of `rex hosts list --json`. A host with no
/// icon gives an empty icon, and a label with no entry gives `None`.
fn parse_icon(text: &str, label: &str) -> Result<Option<String>, RexError> {
    let value = parse_json(text)?;
    let hosts = value
        .get("hosts")
        .and_then(Value::as_array)
        .ok_or(RexError::Malformed)?;
    Ok(hosts
        .iter()
        .filter_map(|entry| entry.get("host"))
        .find(|host| host.get("label").and_then(Value::as_str) == Some(label))
        .map(|host| {
            host.get("icon")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        }))
}
