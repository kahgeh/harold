//! Rex, the second multiplexer Harold can work with (`[agent_monitor] multiplexer = "rex"`).
//!
//! A Rex pane is a block with the id `block:<id>`. This module lists the blocks and finds the
//! agents in them, reads a block's screen with a tail read, and types into a block. Every
//! call goes through the `rex` program. The `RexApi` trait holds the calls, so the tests use
//! a fake.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::settings::AgentSettings;

use super::domain::{AgentIncarnation, AgentPaneObservation};
use super::inventory::{
    AgentInventoryPort, InventoryError, ProviderResolution, program_from_words, resolve_provider,
};
use super::screen::{StopSignal, current_stop, run_stoppable, stop_requested};

/// The start of a Rex pane id.
pub(crate) const BLOCK_PREFIX: &str = "block:";

/// A Rex call that takes longer than this fails.
const CALL_TIMEOUT: Duration = Duration::from_secs(3);
/// The most `process` calls that run side by side.
const PROCESS_WIDTH: usize = 8;
/// A scan that takes longer than this logs how long each part took.
const SLOW_SCAN: Duration = Duration::from_secs(1);
/// A screen read that takes longer than this logs its time.
const SLOW_READ: Duration = Duration::from_millis(500);
/// The fewest bytes a tail read asks for.
const MIN_TAIL_BYTES: usize = 64 * 1024;
/// The most bytes one `rex` call may print.
const MAX_REPLY_BYTES: usize = 16 * 1024 * 1024;
/// How long a block's size stays in the cache.
const SIZE_TTL: Duration = Duration::from_secs(30);
/// The most `blob_read` calls one tail read makes.
const MAX_BLOB_READS: usize = 64;

/// Whether `value` is `block:` and the characters of a block's id.
pub(crate) fn valid_rex_pane_id(value: &str) -> bool {
    value.strip_prefix(BLOCK_PREFIX).is_some_and(|id| {
        !id.is_empty()
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    })
}

/// Why a Rex call failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RexError {
    /// No `rex` program, or no Rex server runs.
    Absent,
    Timeout,
    Failed,
    Malformed,
}

impl From<RexError> for InventoryError {
    fn from(error: RexError) -> Self {
        match error {
            RexError::Absent => Self::RexAbsent,
            RexError::Timeout => Self::RexTimeout,
            RexError::Failed => Self::RexFailed,
            RexError::Malformed => Self::MalformedOutput,
        }
    }
}

/// One process of a block, as `process` gives it. The program's other arguments never
/// arrive from Rex.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RexProc {
    pub(crate) pid: u32,
    pub(crate) name: String,
    pub(crate) argv0: String,
    pub(crate) script: Option<String>,
    pub(crate) cwd: String,
}

impl RexProc {
    /// The program: the file name of `argv0`, or of `script` when `argv0` is an interpreter.
    /// With no `argv0`, the process's `name`.
    pub(crate) fn program(&self) -> String {
        let argv0 = self.argv0.trim_start_matches('-');
        let first = if argv0.is_empty() {
            self.name.as_str()
        } else {
            argv0
        };
        program_from_words(first, self.script.as_deref()).to_string()
    }
}

/// A block's `process` reply: the first process, the foreground program and its parents,
/// nearest first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RexProcess {
    pub(crate) child: RexProc,
    pub(crate) foreground: Option<RexProc>,
    pub(crate) ancestors: Vec<RexProc>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RexSession {
    pub(crate) id: String,
    pub(crate) label: String,
}

/// One terminal block with the numbers that make its label.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RexBlockSlot {
    pub(crate) session_id: String,
    pub(crate) session_label: String,
    /// From 1, the window's place in the session.
    pub(crate) window: u32,
    /// From 1, the block's place in its window.
    pub(crate) pane: u32,
    pub(crate) block_id: String,
}

/// The calls Harold makes to Rex.
pub(crate) trait RexApi: Send + Sync {
    fn sessions(&self) -> Result<Vec<RexSession>, RexError>;
    /// The terminal blocks of the session, window by window: each inner list is one window.
    fn session_view(&self, session_id: &str) -> Result<Vec<Vec<String>>, RexError>;
    fn process(&self, session_id: &str, block_id: &str) -> Result<RexProcess, RexError>;
    /// The start time of the process, in milliseconds since the epoch.
    fn start_time_ms(&self, pid: u32) -> Option<i64>;
    /// One block method call, such as `size`, `format` or `blob_read`.
    fn block_call(
        &self,
        session_id: &str,
        block_id: &str,
        method: &str,
        args: &Value,
    ) -> Result<Value, RexError>;
    /// A paste event.
    fn paste(&self, session_id: &str, block_id: &str, text: &str) -> Result<(), RexError>;
    fn send_key(&self, session_id: &str, block_id: &str, key: &str) -> Result<(), RexError>;
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Escapes the control characters inside the strings of `rex`'s JSON output, which
/// `serde_json` refuses.
pub(crate) fn escape_controls(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let (mut in_string, mut escaped) = (false, false);
    for c in text.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            } else if (c as u32) < 0x20 {
                out.push_str(&format!("\\u{:04x}", c as u32));
                continue;
            }
        } else if c == '"' {
            in_string = true;
        }
        out.push(c);
    }
    out
}

pub(crate) fn parse_json(text: &str) -> Result<Value, RexError> {
    serde_json::from_str(&escape_controls(text)).map_err(|_| RexError::Malformed)
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value.get(key)?.as_str().map(str::to_string)
}

pub(crate) fn parse_sessions(text: &str) -> Result<Vec<RexSession>, RexError> {
    let value = parse_json(text)?;
    let sessions = value
        .get("sessions")
        .and_then(Value::as_array)
        .ok_or(RexError::Malformed)?;
    sessions
        .iter()
        .map(|session| {
            Ok(RexSession {
                id: string_field(session, "session_id").ok_or(RexError::Malformed)?,
                label: string_field(session, "label").unwrap_or_default(),
            })
        })
        .collect()
}

const TERMINAL_CREATOR: &str = "com.superlogical.terminal";

/// The terminal blocks of each window, in the order of `session.view`. A window's blocks are
/// in its layers, in the screen order of the layout.
pub(crate) fn parse_session_view(text: &str) -> Result<Vec<Vec<String>>, RexError> {
    let value = parse_json(text)?;
    let windows = value
        .get("windows")
        .and_then(Value::as_array)
        .ok_or(RexError::Malformed)?;
    let mut result = Vec::new();
    for window in windows {
        let mut blocks = Vec::new();
        let layers = window.get("layers").and_then(Value::as_array);
        for layer in layers.into_iter().flatten() {
            for block in layer
                .get("blocks")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if string_field(block, "creator_name").as_deref() != Some(TERMINAL_CREATOR) {
                    continue;
                }
                if let Some(id) = string_field(block, "block_id") {
                    blocks.push(id);
                }
            }
        }
        result.push(blocks);
    }
    Ok(result)
}

fn parse_proc(value: &Value) -> Result<RexProc, RexError> {
    Ok(RexProc {
        pid: value
            .get("pid")
            .and_then(Value::as_u64)
            .and_then(|pid| u32::try_from(pid).ok())
            .ok_or(RexError::Malformed)?,
        name: string_field(value, "name").unwrap_or_default(),
        argv0: string_field(value, "argv0").unwrap_or_default(),
        script: string_field(value, "script"),
        cwd: string_field(value, "cwd").unwrap_or_default(),
    })
}

pub(crate) fn parse_process(text: &str) -> Result<RexProcess, RexError> {
    let value = parse_json(text)?;
    let child = parse_proc(value.get("child").ok_or(RexError::Malformed)?)?;
    let foreground = match value.get("foreground") {
        Some(Value::Null) | None => None,
        Some(foreground) => Some(parse_proc(foreground)?),
    };
    let ancestors = value
        .get("ancestors")
        .and_then(Value::as_array)
        .map(|list| list.iter().map(parse_proc).collect::<Result<Vec<_>, _>>())
        .transpose()?
        .unwrap_or_default();
    Ok(RexProcess {
        child,
        foreground,
        ancestors,
    })
}

/// The slots of a session's blocks, numbered from 1 by window and by place in the window.
pub(crate) fn slots_of(session: &RexSession, windows: &[Vec<String>]) -> Vec<RexBlockSlot> {
    let mut slots = Vec::new();
    for (window, blocks) in (1_u32..).zip(windows) {
        for (pane, block_id) in (1_u32..).zip(blocks) {
            slots.push(RexBlockSlot {
                session_id: session.id.clone(),
                session_label: session.label.clone(),
                window,
                pane,
                block_id: block_id.clone(),
            });
        }
    }
    slots
}

// ---------------------------------------------------------------------------
// Finding the agent
// ---------------------------------------------------------------------------

/// The agent in a block: look at the foreground program first, then at `ancestors` in the
/// order Rex gives them. Stop at the `child` process and do not look above it. The first
/// process whose program matches an agent command wins. Returns it with its program.
pub(crate) fn find_agent<'a>(
    process: &'a RexProcess,
    settings: &AgentSettings,
) -> Option<(&'a RexProc, String)> {
    for candidate in process.foreground.iter().chain(process.ancestors.iter()) {
        let program = candidate.program();
        if settings.matches_command(&program) {
            return Some((candidate, program));
        }
        if candidate.pid == process.child.pid {
            break;
        }
    }
    None
}

/// The observation for one block, or `None` when no agent runs in it. `start_time` reads
/// an agent pid's start time.
pub(crate) fn observe_block(
    slot: &RexBlockSlot,
    process: &RexProcess,
    settings: &AgentSettings,
    observed_at_ms: i64,
    mut start_time: impl FnMut(u32) -> Option<i64>,
) -> Result<Option<(AgentPaneObservation, String)>, InventoryError> {
    let Some((agent, program)) = find_agent(process, settings) else {
        return Ok(None);
    };
    let Some(agent_started_at_ms) = start_time(agent.pid) else {
        return Err(InventoryError::MissingProcessStartTime);
    };
    let (provider_id, provider_display_name) = match resolve_provider(settings, &program) {
        ProviderResolution::Matched {
            provider_id,
            provider_display_name,
        } => (provider_id, provider_display_name),
        ProviderResolution::Ambiguous { match_count } => {
            tracing::warn!(
                provider_match_count = match_count,
                "agent process matched multiple providers; using unknown provider"
            );
            unknown_provider()
        }
        ProviderResolution::Unknown => unknown_provider(),
    };
    let observation = AgentPaneObservation {
        incarnation: AgentIncarnation {
            pane_id: slot.block_id.clone(),
            pane_pid: process.child.pid,
            agent_pid: agent.pid,
            agent_started_at_ms,
            provider_id,
        },
        tmux_target: format!("{}:{}.{}", slot.session_label, slot.window, slot.pane),
        session_name: slot.session_label.clone(),
        window_index: slot.window,
        pane_index: slot.pane,
        working_directory: agent.cwd.clone(),
        provider_display_name,
        observed_at_ms,
    };
    Ok(Some((observation, program)))
}

fn unknown_provider() -> (String, String) {
    (
        super::domain::UNKNOWN_PROVIDER_ID.to_string(),
        super::domain::UNKNOWN_PROVIDER_DISPLAY_NAME.to_string(),
    )
}

// ---------------------------------------------------------------------------
// The tail read
// ---------------------------------------------------------------------------

/// The size of a block in rows and columns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BlockSize {
    pub(crate) rows: usize,
    pub(crate) cols: usize,
}

impl BlockSize {
    /// The bytes a tail read asks for: the screen at 4 bytes a character, and at least 64 KiB.
    pub(crate) fn tail_bytes(self) -> usize {
        (self.rows * self.cols * 4).max(MIN_TAIL_BYTES)
    }
}

pub(crate) fn parse_size(value: &Value) -> Result<BlockSize, RexError> {
    let field = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_u64)
            .and_then(|number| usize::try_from(number).ok())
            .filter(|number| *number > 0)
            .ok_or(RexError::Malformed)
    };
    Ok(BlockSize {
        rows: field("rows")?,
        cols: field("columns")?,
    })
}

/// The end of a block's output.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Tail {
    pub(crate) text: String,
    /// The length of the whole output in bytes.
    pub(crate) total: u64,
    /// Whether the read started inside the output, so its first line may be cut.
    pub(crate) cut: bool,
}

/// Reads the last `bytes` bytes of the block's output in `format` (`text` or `vt`): one
/// `format` call for the length and an id, then `blob_read` calls with that id until `eof`.
pub(crate) fn tail_read(
    api: &dyn RexApi,
    session_id: &str,
    block_id: &str,
    format: &str,
    bytes: usize,
) -> Result<Tail, RexError> {
    let header = api.block_call(
        session_id,
        block_id,
        "format",
        &json!({"format": format, "trim": true, "max_bytes": 1}),
    )?;
    let total = header
        .get("total")
        .and_then(Value::as_u64)
        .ok_or(RexError::Malformed)?;
    let id = header
        .get("id")
        .and_then(Value::as_u64)
        .ok_or(RexError::Malformed)?;
    let wanted = u64::try_from(bytes).unwrap_or(u64::MAX);
    let start = total.saturating_sub(wanted);
    let mut offset = start;
    let mut text = String::new();
    for _ in 0..MAX_BLOB_READS {
        let reply = api.block_call(
            session_id,
            block_id,
            "blob_read",
            &json!({"id": id, "offset": offset, "max_bytes": bytes}),
        )?;
        let data = reply
            .get("data")
            .and_then(Value::as_str)
            .ok_or(RexError::Malformed)?;
        let eof = reply.get("eof").and_then(Value::as_bool).unwrap_or(false);
        offset += u64::try_from(data.len()).unwrap_or(u64::MAX);
        text.push_str(data);
        if eof || data.is_empty() {
            break;
        }
    }
    Ok(Tail {
        text,
        total,
        cut: start > 0,
    })
}

/// The last `keep` lines of a tail. A cut tail loses its first line, which can start in the
/// middle. Trailing line breaks go.
pub(crate) fn last_lines(tail: &Tail, keep: usize) -> String {
    let text = if tail.cut {
        tail.text.split_once('\n').map_or("", |(_, rest)| rest)
    } else {
        tail.text.as_str()
    };
    let text = text.trim_end_matches(['\n', '\r']);
    let lines: Vec<&str> = text.split('\n').collect();
    let from = lines.len().saturating_sub(keep);
    lines[from..].join("\n")
}

/// How much of the output a screen read keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Keep {
    /// The rows of the screen.
    Visible,
    /// This many lines.
    Lines(usize),
}

static SIZES: Mutex<Option<HashMap<String, (BlockSize, Instant)>>> = Mutex::new(None);

fn block_size(api: &dyn RexApi, session_id: &str, block_id: &str) -> Result<BlockSize, RexError> {
    let now = Instant::now();
    if let Some((size, at)) = SIZES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .and_then(|sizes| sizes.get(block_id))
        && now.duration_since(*at) < SIZE_TTL
    {
        return Ok(*size);
    }
    let size = parse_size(&api.block_call(session_id, block_id, "size", &json!({}))?)?;
    SIZES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get_or_insert_with(HashMap::new)
        .insert(block_id.to_string(), (size, now));
    Ok(size)
}

/// The text of a Rex block's screen. `styled` reads the escape sequences (`vt`) too. Logs
/// `agent screen read slow` when the read takes more than 500 ms.
pub(crate) fn read_screen_text(
    api: &dyn RexApi,
    block_id: &str,
    styled: bool,
    keep: Keep,
) -> Result<String, RexError> {
    let started = Instant::now();
    let session_id = session_of(api, block_id)?;
    let size = block_size(api, &session_id, block_id)?;
    let bytes = match keep {
        Keep::Visible => size.tail_bytes(),
        Keep::Lines(lines) => (lines * size.cols * 2).clamp(size.tail_bytes(), 1024 * 1024),
    };
    let tail = tail_read(
        api,
        &session_id,
        block_id,
        if styled { "vt" } else { "text" },
        bytes,
    )?;
    if started.elapsed() > SLOW_READ {
        tracing::warn!(
            pane_id = block_id,
            elapsed_ms = elapsed_ms(started),
            scrollback_bytes = tail.total,
            "agent screen read slow"
        );
    }
    let lines = match keep {
        Keep::Visible => size.rows,
        Keep::Lines(lines) => lines,
    };
    Ok(last_lines(&tail, lines))
}

// ---------------------------------------------------------------------------
// The block directory: which session holds a block
// ---------------------------------------------------------------------------

static DIRECTORY: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

fn directory_set(slots: &[RexBlockSlot]) {
    let map = slots
        .iter()
        .map(|slot| (slot.block_id.clone(), slot.session_id.clone()))
        .collect();
    *DIRECTORY.lock().unwrap_or_else(PoisonError::into_inner) = Some(map);
}

fn directory_get(block_id: &str) -> Option<String> {
    DIRECTORY
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .and_then(|map| map.get(block_id).cloned())
}

/// All terminal blocks of all sessions, in the order Rex lists them.
fn list_slots(api: &dyn RexApi) -> Result<Vec<RexBlockSlot>, RexError> {
    let mut slots = Vec::new();
    for session in api.sessions()? {
        let windows = api.session_view(&session.id)?;
        slots.extend(slots_of(&session, &windows));
    }
    Ok(slots)
}

/// The session that holds the block: the last scan knows it, and a new listing finds it.
pub(crate) fn session_of(api: &dyn RexApi, block_id: &str) -> Result<String, RexError> {
    if let Some(session) = directory_get(block_id) {
        return Ok(session);
    }
    let slots = list_slots(api)?;
    directory_set(&slots);
    directory_get(block_id).ok_or(RexError::Failed)
}

/// Whether the block is gone: a fresh listing succeeds and does not hold it.
pub(crate) fn block_is_gone(api: &dyn RexApi, block_id: &str) -> bool {
    match list_slots(api) {
        Ok(slots) => {
            directory_set(&slots);
            !slots.iter().any(|slot| slot.block_id == block_id)
        }
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// Typing
// ---------------------------------------------------------------------------

/// The JSON of a paste event. `serde_json` escapes quotes and line breaks.
pub(crate) fn paste_event_json(session_id: &str, block_id: &str, text: &str) -> String {
    json!({
        "session_id": session_id,
        "block_id": block_id,
        "event": {"type": "paste", "payload": {"text": text}},
    })
    .to_string()
}

/// Rex's name for a key that tmux names differently.
pub(crate) fn rex_key_name(key: &str) -> String {
    match key {
        "BSpace" => "backspace".to_string(),
        "Enter" => "enter".to_string(),
        other => other.to_string(),
    }
}

fn typing_error(what: &str, block_id: &str, error: RexError) -> String {
    format!("rex {what} failed for {block_id}: {error:?}")
}

/// Pastes `text` into the block as one paste, without Enter.
pub(crate) fn type_text(block_id: &str, text: &str) -> Result<(), String> {
    let api = system();
    let session = session_of(&*api, block_id).map_err(|e| typing_error("paste", block_id, e))?;
    api.paste(&session, block_id, text)
        .map_err(|e| typing_error("paste", block_id, e))
}

/// Sends one key to the block by its tmux name, such as `i` or `BSpace`.
pub(crate) fn press_key(block_id: &str, key: &str) -> Result<(), String> {
    let api = system();
    let session = session_of(&*api, block_id).map_err(|e| typing_error("key", block_id, e))?;
    api.send_key(&session, block_id, &rex_key_name(key))
        .map_err(|e| typing_error("key", block_id, e))
}

/// The styled visible screen of a block, or `None` when the read fails.
pub(crate) fn read_screen_styled(block_id: &str) -> Option<String> {
    read_screen_text(&*system(), block_id, true, Keep::Visible).ok()
}

// ---------------------------------------------------------------------------
// The inventory
// ---------------------------------------------------------------------------

pub(crate) struct RexAgentInventory {
    api: Arc<dyn RexApi>,
    settings: AgentSettings,
    /// Each agent pid's start time, kept while the pid stays in the listing.
    start_times: Mutex<HashMap<u32, i64>>,
    /// Each observed agent process's program, from the last scan, by pane id and agent pid.
    programs: Mutex<HashMap<(String, u32), String>>,
}

impl RexAgentInventory {
    pub(crate) fn new(api: Arc<dyn RexApi>, settings: AgentSettings) -> Self {
        Self {
            api,
            settings,
            start_times: Mutex::new(HashMap::new()),
            programs: Mutex::new(HashMap::new()),
        }
    }

    /// An inventory that calls the `rex` program.
    pub(crate) fn system(settings: AgentSettings) -> Self {
        Self::new(system(), settings)
    }

    fn scan_inner(
        &self,
        rex_ms: &mut u64,
        ps_ms: &mut u64,
    ) -> Result<Vec<AgentPaneObservation>, InventoryError> {
        let started = Instant::now();
        let listed = list_slots(&*self.api);
        let slots = listed?;
        directory_set(&slots);
        let signal = current_stop();
        let processes = parallel_map(&slots, PROCESS_WIDTH, signal, |slot| {
            self.api.process(&slot.session_id, &slot.block_id)
        });
        *rex_ms = elapsed_ms(started);
        let started = Instant::now();
        let observed_at_ms = now_ms()?;
        let mut observed = Vec::new();
        let mut programs = HashMap::new();
        let mut seen = HashSet::new();
        for (slot, process) in slots.iter().zip(processes) {
            let process = process?;
            let result = observe_block(slot, &process, &self.settings, observed_at_ms, |pid| {
                seen.insert(pid);
                self.start_time(pid)
            })?;
            if let Some((observation, program)) = result {
                programs.insert(
                    (
                        observation.incarnation.pane_id.clone(),
                        observation.incarnation.agent_pid,
                    ),
                    program,
                );
                observed.push(observation);
            }
        }
        *ps_ms = elapsed_ms(started);
        self.start_times
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|pid, _| seen.contains(pid));
        *self.programs.lock().unwrap_or_else(PoisonError::into_inner) = programs;
        Ok(observed)
    }

    /// A start time from the cache, or one `ps` read for a new pid.
    fn start_time(&self, pid: u32) -> Option<i64> {
        if let Some(time) = self
            .start_times
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&pid)
        {
            return Some(*time);
        }
        let time = self.api.start_time_ms(pid)?;
        self.start_times
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(pid, time);
        Some(time)
    }
}

impl AgentInventoryPort for RexAgentInventory {
    /// Logs one `warn` line when the scan takes more than a second or does not succeed.
    fn scan(&self) -> Result<Vec<AgentPaneObservation>, InventoryError> {
        let started = Instant::now();
        let (mut rex_ms, mut ps_ms) = (0, 0);
        let result = self.scan_inner(&mut rex_ms, &mut ps_ms);
        let outcome = match &result {
            Ok(_) => "ok",
            Err(_) if stop_requested() => "stopped",
            Err(_) => "failed",
        };
        if started.elapsed() > SLOW_SCAN || outcome != "ok" {
            tracing::warn!(ps_ms, rex_ms, outcome, "agent inventory scan slow");
        }
        result
    }

    fn program(&self, incarnation: &AgentIncarnation) -> Option<String> {
        self.programs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&(incarnation.pane_id.clone(), incarnation.agent_pid))
            .cloned()
    }

    fn resolve(&self, pane_id: &str) -> Result<Option<AgentPaneObservation>, InventoryError> {
        Ok(self
            .scan()?
            .into_iter()
            .find(|pane| pane.incarnation.pane_id == pane_id))
    }

    /// Runs `process` again for the block and searches it as the scan does. It passes when
    /// the same agent pid with the same start time is found there.
    fn is_current(&self, incarnation: &AgentIncarnation) -> Result<bool, InventoryError> {
        let block_id = incarnation.pane_id.as_str();
        let session_id = session_of(&*self.api, block_id)?;
        let process = self.api.process(&session_id, block_id)?;
        let Some((agent, _)) = find_agent(&process, &self.settings) else {
            return Ok(false);
        };
        if agent.pid != incarnation.agent_pid || process.child.pid != incarnation.pane_pid {
            return Ok(false);
        }
        Ok(self.api.start_time_ms(agent.pid) == Some(incarnation.agent_started_at_ms))
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn now_ms() -> Result<i64, InventoryError> {
    let milliseconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| InventoryError::RexFailed)?
        .as_millis();
    i64::try_from(milliseconds).map_err(|_| InventoryError::RexFailed)
}

/// Runs `call` for each item, at most `width` at a time, and returns the results in order.
/// Each worker thread gets the caller's stop signal.
fn parallel_map<T: Sync, R: Send>(
    items: &[T],
    width: usize,
    signal: Option<StopSignal>,
    call: impl Fn(&T) -> R + Sync,
) -> Vec<R> {
    let mut results = Vec::with_capacity(items.len());
    for chunk in items.chunks(width.max(1)) {
        std::thread::scope(|scope| {
            let handles: Vec<_> = chunk
                .iter()
                .map(|item| {
                    let (call, signal) = (&call, signal.clone());
                    scope.spawn(move || match signal {
                        Some(signal) => run_stoppable(signal, || call(item)),
                        None => call(item),
                    })
                })
                .collect();
            for handle in handles {
                results.push(handle.join().unwrap_or_else(|_| panic_result()));
            }
        });
    }
    results
}

fn panic_result<R>() -> R {
    // A panic in a worker is a bug in a `rex` call: do not hide it.
    panic!("a Rex worker thread panicked")
}

// ---------------------------------------------------------------------------
// The `rex` program
// ---------------------------------------------------------------------------

/// Where `rex` can be, in the order Harold tries: the setting, the running server's own
/// copy, `rex` on the `PATH`, and the app bundle in `/Applications`.
pub(crate) fn rex_candidates(
    setting: Option<&str>,
    home: Option<&Path>,
    path_env: Option<&str>,
    applications: &[PathBuf],
) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(setting) = setting.filter(|value| !value.trim().is_empty()) {
        candidates.push(PathBuf::from(setting));
    }
    if let Some(home) = home {
        candidates.push(home.join(
            "Library/Application Support/com.superlogical.rex/RexServer/Versions/Current/Rex Server.app/Contents/MacOS/rex",
        ));
    }
    for dir in path_env.unwrap_or_default().split(':') {
        if !dir.is_empty() {
            candidates.push(Path::new(dir).join("rex"));
        }
    }
    for app in applications {
        candidates.push(app.join("Contents/Helpers/Rex Server.rex/Contents/MacOS/rex"));
    }
    candidates
}

fn find_rex(setting: Option<&str>) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let path_env = std::env::var("PATH").ok();
    let applications: Vec<PathBuf> = std::fs::read_dir("/Applications")
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("Rex") && name.ends_with(".app"))
        })
        .collect();
    rex_candidates(setting, home.as_deref(), path_env.as_deref(), &applications)
        .into_iter()
        .find(|path| path.is_file())
}

/// The output of `program` with `args`, or why it failed. A call that takes more than
/// `timeout` is killed. The call never starts a Rex server.
fn run_program(program: &Path, args: &[&str], timeout: Duration) -> Result<String, RexError> {
    let mut command = Command::new(program);
    command
        .arg("--autostart=false")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command.spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            RexError::Absent
        } else {
            RexError::Failed
        }
    })?;
    let Some(mut stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(RexError::Failed);
    };
    // Drained on its own thread, so a long reply never blocks the child.
    let reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        stdout
            .by_ref()
            .take(MAX_REPLY_BYTES as u64 + 1)
            .read_to_end(&mut buffer)
            .map(|_| buffer)
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(RexError::Failed);
            }
        }
        if started.elapsed() >= timeout || stop_requested() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(if stop_requested() {
                RexError::Failed
            } else {
                RexError::Timeout
            });
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    let output = reader
        .join()
        .map_err(|_| RexError::Failed)?
        .map_err(|_| RexError::Failed)?;
    if !status.success() {
        return Err(RexError::Failed);
    }
    if output.len() > MAX_REPLY_BYTES {
        return Err(RexError::Malformed);
    }
    String::from_utf8(output).map_err(|_| RexError::Malformed)
}

/// The `rex` program, found on the first call that needs it.
pub(crate) struct SystemRex {
    setting: Option<String>,
    found: Mutex<Option<PathBuf>>,
}

impl SystemRex {
    pub(crate) fn new(setting: Option<String>) -> Self {
        Self {
            setting,
            found: Mutex::new(None),
        }
    }

    /// The path of the `rex` program, or `Absent`.
    pub(crate) fn program(&self) -> Result<PathBuf, RexError> {
        let mut found = self.found.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(path) = found.as_ref().filter(|path| path.is_file()) {
            return Ok(path.clone());
        }
        let path = find_rex(self.setting.as_deref()).ok_or(RexError::Absent)?;
        *found = Some(path.clone());
        Ok(path)
    }

    fn run(&self, args: &[&str]) -> Result<String, RexError> {
        run_program(&self.program()?, args, CALL_TIMEOUT)
    }

    /// The output of `rex version`, for the start line.
    pub(crate) fn version(&self) -> Result<String, RexError> {
        Ok(self
            .run(&["version"])?
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .to_string())
    }
}

impl RexApi for SystemRex {
    fn sessions(&self) -> Result<Vec<RexSession>, RexError> {
        // No answer from the first call means no server runs.
        match self.run(&["ls", "--json"]) {
            Err(RexError::Failed) => Err(RexError::Absent),
            other => parse_sessions(&other?),
        }
    }

    fn session_view(&self, session_id: &str) -> Result<Vec<Vec<String>>, RexError> {
        let args = json!({"session_id": session_id}).to_string();
        parse_session_view(&self.run(&["api", "call", "session.view", &args])?)
    }

    fn process(&self, session_id: &str, block_id: &str) -> Result<RexProcess, RexError> {
        parse_process(&self.run(&["block", "call", "-s", session_id, "-b", block_id, "process"])?)
    }

    fn start_time_ms(&self, pid: u32) -> Option<i64> {
        let output = Command::new("ps")
            .env("LC_ALL", "C")
            .env("TZ", "UTC")
            .args(["-o", "lstart=", "-p", &pid.to_string()])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        parse_lstart(std::str::from_utf8(&output.stdout).ok()?)
    }

    fn block_call(
        &self,
        session_id: &str,
        block_id: &str,
        method: &str,
        args: &Value,
    ) -> Result<Value, RexError> {
        let args = args.to_string();
        parse_json(&self.run(&[
            "block", "call", "-s", session_id, "-b", block_id, method, &args,
        ])?)
    }

    fn paste(&self, session_id: &str, block_id: &str, text: &str) -> Result<(), RexError> {
        let event = paste_event_json(session_id, block_id, text);
        self.run(&["api", "call", "block.event", &event])
            .map(|_| ())
    }

    fn send_key(&self, session_id: &str, block_id: &str, key: &str) -> Result<(), RexError> {
        self.run(&["send-key", "-s", session_id, "-b", block_id, key])
            .map(|_| ())
    }
}

/// The start time `ps -o lstart=` prints, such as `Sat Oct 31 22:25:16 2026`, read as UTC.
pub(crate) fn parse_lstart(text: &str) -> Option<i64> {
    let fields: Vec<&str> = text.split_whitespace().collect();
    let [_, month, day, time, year] = fields.as_slice() else {
        return None;
    };
    super::inventory::parse_start_time(month, day, time, year)
}

/// The inventory of the multiplexer that `[agent_monitor] multiplexer` names.
pub(crate) fn inventory_for(settings: &crate::settings::Settings) -> Arc<dyn AgentInventoryPort> {
    match settings.agent_monitor.multiplexer() {
        crate::settings::Multiplexer::Rex => {
            Arc::new(RexAgentInventory::system(settings.agents.clone()))
        }
        crate::settings::Multiplexer::Tmux => Arc::new(super::inventory::TmuxAgentInventory::new(
            settings.agents.clone(),
        )),
    }
}

/// The screen reader of the multiplexer that `[agent_monitor] multiplexer` names.
pub(crate) fn screen_for(
    settings: &crate::settings::Settings,
) -> Arc<dyn super::screen::VisibleScreenPort> {
    match settings.agent_monitor.multiplexer() {
        crate::settings::Multiplexer::Rex => Arc::new(super::screen::RexVisibleScreen::new()),
        crate::settings::Multiplexer::Tmux => Arc::new(super::screen::TmuxVisibleScreen::new()),
    }
}

/// Logs the `agent monitor multiplexer` line once, at start.
pub(crate) fn log_multiplexer(settings: &crate::settings::Settings) {
    match settings.agent_monitor.multiplexer() {
        crate::settings::Multiplexer::Rex => {
            let version = system().version().unwrap_or_default();
            tracing::info!(multiplexer = "rex", rex_version = %version, "agent monitor multiplexer");
        }
        crate::settings::Multiplexer::Tmux => {
            tracing::info!(multiplexer = "tmux", "agent monitor multiplexer");
        }
    }
}

static SYSTEM: OnceLock<Arc<SystemRex>> = OnceLock::new();

/// The shared `rex` program handle of the daemon. Its path setting is read once.
pub(crate) fn system() -> Arc<SystemRex> {
    Arc::clone(SYSTEM.get_or_init(|| {
        let setting = crate::settings::try_get_settings()
            .and_then(|settings| settings.agent_monitor.rex.clone());
        Arc::new(SystemRex::new(setting))
    }))
}

#[cfg(test)]
#[path = "rex_tests.rs"]
mod tests;
