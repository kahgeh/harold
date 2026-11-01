//! The pure parts of the Rex program status source.
//!
//! A Rex block keeps program status records. The record with no `id` is the one the program
//! writes about itself. This module parses that record and the `rex events` lines about it,
//! maps the record state to an agent state, and keeps each agent's current step in memory.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::sync::watch;
use tracing::{Dispatch, dispatcher, info};

use super::super::domain::ObservedAgentState;
use super::super::summary::normalize_work_summary;
use super::{RexApi, RexBlockSlot, RexError, parse_json};

/// The state word of a record. A word that is not in this list makes the record unusable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(
    dead_code,
    reason = "the observer and the streams use these in later tasks"
)]
pub(crate) enum RecordState {
    Working,
    Blocked,
    Done,
    Idle,
    Error,
}

impl RecordState {
    fn from_word(word: &str) -> Option<Self> {
        match word {
            "working" => Some(Self::Working),
            "blocked" => Some(Self::Blocked),
            "done" => Some(Self::Done),
            "idle" => Some(Self::Idle),
            "error" => Some(Self::Error),
            _ => None,
        }
    }

    /// Stage 2 has two agent states. A blocked agent counts as working.
    #[allow(dead_code, reason = "the observer uses this in a later task")]
    pub(crate) fn observed(self) -> ObservedAgentState {
        match self {
            Self::Working | Self::Blocked => ObservedAgentState::Working,
            Self::Done | Self::Idle | Self::Error => ObservedAgentState::Idle,
        }
    }
}

/// The root record of a block.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(
    dead_code,
    reason = "the observer and the streams use these in later tasks"
)]
pub(crate) struct StatusRecord {
    pub state: RecordState,
    pub pid: u32,
    /// The cleaned `msg`. Only a `working` or `blocked` record gives one.
    pub msg: Option<String>,
}

/// Reads one record object. The result is `None` when the state word is unknown or the
/// owner pid is missing.
fn record_from_value(value: &Value) -> Option<StatusRecord> {
    let state = RecordState::from_word(value.get("state")?.as_str()?)?;
    let pid = u32::try_from(value.get("owner")?.get("pid")?.as_u64()?).ok()?;
    let msg = match state {
        RecordState::Working | RecordState::Blocked => value
            .get("msg")
            .and_then(Value::as_str)
            .and_then(normalize_work_summary),
        RecordState::Done | RecordState::Idle | RecordState::Error => None,
    };
    Some(StatusRecord { state, pid, msg })
}

fn has_id(value: &Value) -> bool {
    value
        .get("id")
        .and_then(Value::as_str)
        .is_some_and(|id| !id.is_empty())
}

/// Parses the output of `program_status` and returns the root record.
#[allow(dead_code, reason = "the observer uses this in a later task")]
pub(crate) fn parse_program_status(text: &str) -> Result<Option<StatusRecord>, RexError> {
    let value = parse_json(text)?;
    let records = value
        .get("records")
        .and_then(Value::as_array)
        .ok_or(RexError::Malformed)?;
    Ok(records
        .iter()
        .find(|record| !has_id(record))
        .and_then(record_from_value))
}

/// An event about the program status of a block.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code, reason = "the event streams use these in a later task")]
pub(crate) enum StatusEvent {
    /// The root record changed. `record` is `None` when the new record is unusable.
    Changed {
        block_id: String,
        record: Option<StatusRecord>,
    },
    Removed {
        block_id: String,
    },
    BlockClosed {
        block_id: String,
    },
}

/// Parses one line of `rex events -s <session> --json`.
///
/// A live line (captured from Rex) is a `session_event` envelope. Its `payload.event` is
/// `{"type":"block_event","payload":{"block_id":..,"name":"program_status_changed",
/// "payload":{"id":"","record":{..},"reason":..}}}`. A block close is
/// `{"type":"block_closed","payload":{"block_id":..}}`. The parser also reads an event
/// object that sits at the top of the line, an event with no `block_event` wrapper, and
/// an event whose data sits on the event object itself.
#[allow(dead_code, reason = "the event streams use this in a later task")]
pub(crate) fn parse_event_line(line: &str) -> Option<StatusEvent> {
    let value: Value = serde_json::from_str(line.trim()).ok()?;
    let event = value
        .get("payload")
        .and_then(|payload| payload.get("event"))
        .unwrap_or(&value);
    let kind = event.get("type")?.as_str()?;
    let body = event.get("payload").unwrap_or(event);
    let block_id = body.get("block_id")?.as_str()?.to_string();
    let (name, data) = if kind == "block_event" {
        (
            body.get("name")?.as_str()?,
            body.get("payload").unwrap_or(body),
        )
    } else {
        (kind, body)
    };
    match name {
        "program_status_changed" => {
            let record = data.get("record").unwrap_or(data);
            if has_id(data) || has_id(record) {
                return None;
            }
            Some(StatusEvent::Changed {
                block_id,
                record: record_from_value(record),
            })
        }
        "program_status_removed" => {
            let record = data.get("record").unwrap_or(data);
            if has_id(data) || has_id(record) {
                return None;
            }
            Some(StatusEvent::Removed { block_id })
        }
        "block_closed" => Some(StatusEvent::BlockClosed { block_id }),
        _ => None,
    }
}

struct StepsInner {
    steps: Mutex<HashMap<String, (u32, String)>>,
    version: watch::Sender<u64>,
}

impl Default for StepsInner {
    fn default() -> Self {
        Self {
            steps: Mutex::new(HashMap::new()),
            version: watch::channel(0).0,
        }
    }
}

/// The current step of each agent, in memory only. It is keyed by block id and holds the
/// pid of the agent that owns the step.
#[derive(Clone, Default)]
#[allow(
    dead_code,
    reason = "the streams and the watch stream use this in later tasks"
)]
pub(crate) struct CurrentSteps {
    inner: Arc<StepsInner>,
}

#[allow(
    dead_code,
    reason = "the streams and the watch stream use this in later tasks"
)]
impl CurrentSteps {
    /// Stores the step, or removes it when `step` is `None`. The version rises only when
    /// the stored value changes.
    pub(crate) fn set(&self, block_id: &str, pid: u32, step: Option<String>) {
        let changed = {
            let mut steps = self
                .inner
                .steps
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            match step {
                Some(step) => {
                    let new = (pid, step);
                    if steps.get(block_id) == Some(&new) {
                        false
                    } else {
                        steps.insert(block_id.to_string(), new);
                        true
                    }
                }
                None => steps.remove(block_id).is_some(),
            }
        };
        if changed {
            self.bump();
        }
    }

    pub(crate) fn clear_block(&self, block_id: &str) {
        let removed = self
            .inner
            .steps
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(block_id)
            .is_some();
        if removed {
            self.bump();
        }
    }

    /// The step of the block, only when the stored pid is `pid`.
    pub(crate) fn get(&self, block_id: &str, pid: u32) -> Option<String> {
        let steps = self
            .inner
            .steps
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        steps
            .get(block_id)
            .filter(|(stored, _)| *stored == pid)
            .map(|(_, step)| step.clone())
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<u64> {
        self.inner.version.subscribe()
    }

    fn bump(&self) {
        self.inner.version.send_modify(|version| *version += 1);
    }
}

/// The least time between two starts of the event stream of one session.
pub(crate) const RESTART_GAP: Duration = Duration::from_secs(5);

/// What the streams know about one block.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code, reason = "the observer uses this in a later task")]
pub(crate) enum Lookup {
    /// The block's session has a running stream. The record is the root record, if any.
    Stream(Option<StatusRecord>),
    /// The block's session has no running stream.
    NoStream,
}

pub(crate) type Program = Arc<dyn Fn() -> Result<PathBuf, RexError> + Send + Sync>;
pub(crate) type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

/// A record with the session that holds its block.
type Records = Mutex<HashMap<String, (String, StatusRecord)>>;

struct Shared {
    api: Arc<dyn RexApi>,
    steps: CurrentSteps,
    records: Records,
    /// The log subscriber of the thread that made the `RexStatus`. Reader threads use it.
    dispatch: Dispatch,
}

#[derive(Default)]
struct Flags {
    /// The first read is done.
    ready: AtomicBool,
    /// The reader thread has stopped reading.
    ended: AtomicBool,
    /// A caller asked the stream to stop.
    stop: AtomicBool,
}

struct Stream {
    started: Instant,
    flags: Arc<Flags>,
    child: Arc<Mutex<Option<Child>>>,
    thread: Option<JoinHandle<()>>,
}

impl Stream {
    /// A stream that never ran, such as one where the `rex` program is absent.
    fn failed(started: Instant) -> Self {
        let flags = Flags::default();
        flags.ended.store(true, Ordering::Release);
        Self {
            started,
            flags: Arc::new(flags),
            child: Arc::new(Mutex::new(None)),
            thread: None,
        }
    }

    /// Kills the child and waits for the reader thread, which reaps it.
    fn stop(mut self) {
        self.flags.stop.store(true, Ordering::Release);
        if let Some(child) = lock(&self.child).as_mut() {
            let _ = child.kill();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[derive(Default)]
struct Registry {
    streams: HashMap<String, Stream>,
    /// The session of each block, from the last `sync`.
    owners: HashMap<String, String>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn log_stream(
    dispatch: &Dispatch,
    session: &str,
    event: &str,
    reason: Option<&str>,
    records: usize,
) {
    dispatcher::with_default(dispatch, || match reason {
        Some(reason) => info!(
            session.id = session,
            event, reason, records, "agent status stream"
        ),
        None => info!(session.id = session, event, records, "agent status stream"),
    });
}

/// One `rex events` child for each Rex session, and the root record of each block.
#[allow(dead_code, reason = "the observer uses this in a later task")]
pub(crate) struct RexStatus {
    shared: Arc<Shared>,
    program: Program,
    clock: Clock,
    registry: Mutex<Registry>,
}

#[allow(dead_code, reason = "the observer uses this in a later task")]
impl RexStatus {
    pub(crate) fn new(api: Arc<dyn RexApi>, program: Program, steps: CurrentSteps) -> Self {
        Self::with_clock(api, program, steps, Arc::new(Instant::now))
    }

    /// Like `new`, with a clock for the restart gap.
    pub(crate) fn with_clock(
        api: Arc<dyn RexApi>,
        program: Program,
        steps: CurrentSteps,
        clock: Clock,
    ) -> Self {
        Self {
            shared: Arc::new(Shared {
                api,
                steps,
                records: Mutex::new(HashMap::new()),
                dispatch: dispatcher::get_default(Dispatch::clone),
            }),
            program,
            clock,
            registry: Mutex::new(Registry::default()),
        }
    }

    pub(crate) fn steps(&self) -> CurrentSteps {
        self.shared.steps.clone()
    }

    /// Starts a stream for each session in `slots` that has none, and stops the stream of a
    /// session that left `slots`. A stream that ended restarts after `RESTART_GAP`. The
    /// call does not wait for a child.
    pub(crate) fn sync(&self, slots: &[RexBlockSlot]) {
        let now = (self.clock)();
        let mut registry = lock(&self.registry);
        let mut sessions: Vec<(&str, Vec<String>)> = Vec::new();
        registry.owners.clear();
        for slot in slots {
            registry
                .owners
                .insert(slot.block_id.clone(), slot.session_id.clone());
            match sessions.iter_mut().find(|(id, _)| *id == slot.session_id) {
                Some((_, blocks)) => blocks.push(slot.block_id.clone()),
                None => sessions.push((&slot.session_id, vec![slot.block_id.clone()])),
            }
        }
        let gone: Vec<String> = registry
            .streams
            .keys()
            .filter(|id| !sessions.iter().any(|(session, _)| session == id))
            .cloned()
            .collect();
        for id in gone {
            if let Some(stream) = registry.streams.remove(&id) {
                stream.stop();
            }
        }
        for (id, blocks) in sessions {
            if let Some(stream) = registry.streams.get(id) {
                let ended = stream.flags.ended.load(Ordering::Acquire);
                if !ended || now.saturating_duration_since(stream.started) < RESTART_GAP {
                    continue;
                }
                if let Some(old) = registry.streams.remove(id) {
                    old.stop();
                }
            }
            let stream = self.start(id, blocks, now);
            registry.streams.insert(id.to_string(), stream);
        }
    }

    fn start(&self, session_id: &str, blocks: Vec<String>, now: Instant) -> Stream {
        let dispatch = &self.shared.dispatch;
        let program = match (self.program)() {
            Ok(program) => program,
            Err(error) => {
                log_stream(dispatch, session_id, "ended", Some(spawn_reason(error)), 0);
                return Stream::failed(now);
            }
        };
        let spawned = Command::new(program)
            .arg("--autostart=false")
            .args(["events", "-s", session_id, "--json"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(error) => {
                let error = if error.kind() == std::io::ErrorKind::NotFound {
                    RexError::Absent
                } else {
                    RexError::Failed
                };
                log_stream(dispatch, session_id, "ended", Some(spawn_reason(error)), 0);
                return Stream::failed(now);
            }
        };
        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill();
            let _ = child.wait();
            log_stream(dispatch, session_id, "ended", Some("read_error"), 0);
            return Stream::failed(now);
        };
        log_stream(dispatch, session_id, "started", None, 0);
        let flags = Arc::new(Flags::default());
        let child = Arc::new(Mutex::new(Some(child)));
        let reader = Reader {
            shared: Arc::clone(&self.shared),
            session_id: session_id.to_string(),
            flags: Arc::clone(&flags),
            child: Arc::clone(&child),
        };
        let thread = std::thread::Builder::new()
            .name("rex-status".to_string())
            .spawn(move || reader.run(&blocks, stdout))
            .ok();
        if thread.is_none() {
            flags.ended.store(true, Ordering::Release);
            if let Some(mut child) = lock(&child).take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        Stream {
            started: now,
            flags,
            child,
            thread,
        }
    }

    /// The root record of the block, when the block's session has a running stream that
    /// has made its first read.
    pub(crate) fn lookup(&self, block_id: &str) -> Lookup {
        let registry = lock(&self.registry);
        let running = registry
            .owners
            .get(block_id)
            .and_then(|session| registry.streams.get(session))
            .is_some_and(|stream| {
                stream.flags.ready.load(Ordering::Acquire)
                    && !stream.flags.ended.load(Ordering::Acquire)
            });
        if !running {
            return Lookup::NoStream;
        }
        let record = lock(&self.shared.records)
            .get(block_id)
            .map(|(_, record)| record.clone());
        Lookup::Stream(record)
    }

    /// Kills and reaps every child.
    pub(crate) fn stop_all(&self) {
        let streams: Vec<Stream> = lock(&self.registry)
            .streams
            .drain()
            .map(|(_, stream)| stream)
            .collect();
        for stream in streams {
            stream.stop();
        }
    }
}

impl Drop for RexStatus {
    fn drop(&mut self) {
        self.stop_all();
    }
}

fn spawn_reason(error: RexError) -> &'static str {
    match error {
        RexError::Absent => "rex_absent",
        _ => "read_error",
    }
}

/// The reader thread of one stream.
struct Reader {
    shared: Arc<Shared>,
    session_id: String,
    flags: Arc<Flags>,
    child: Arc<Mutex<Option<Child>>>,
}

impl Reader {
    fn run(self, blocks: &[String], stdout: std::process::ChildStdout) {
        // The child is already running, so events that come during the first read wait in
        // the pipe. The events that follow replace what the first read stored.
        for block in blocks {
            if self.flags.stop.load(Ordering::Acquire) {
                break;
            }
            self.refresh(block);
        }
        self.flags.ready.store(true, Ordering::Release);
        let mut reader = BufReader::new(stdout);
        let mut line = Vec::new();
        let reason = loop {
            line.clear();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) => break "exit",
                Err(_) => break "read_error",
                Ok(_) => {}
            }
            if let Some(event) = parse_event_line(&String::from_utf8_lossy(&line)) {
                self.apply(event);
            }
        };
        self.flags.ended.store(true, Ordering::Release);
        if let Some(mut child) = lock(&self.child).take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let dropped = self.drop_records();
        log_stream(
            &self.shared.dispatch,
            &self.session_id,
            "ended",
            Some(reason),
            dropped,
        );
    }

    fn apply(&self, event: StatusEvent) {
        match event {
            StatusEvent::Changed {
                block_id,
                record: Some(record),
            } => self.store(&block_id, record),
            // An event record may carry no owner pid. A fresh call gives the whole record.
            StatusEvent::Changed {
                block_id,
                record: None,
            } => self.refresh(&block_id),
            StatusEvent::Removed { block_id } | StatusEvent::BlockClosed { block_id } => {
                self.remove(&block_id);
            }
        }
    }

    /// Reads the root record of the block with `program_status` and stores it.
    fn refresh(&self, block_id: &str) {
        let record = self
            .shared
            .api
            .block_call(&self.session_id, block_id, "program_status", &json!({}))
            .ok()
            .and_then(|value| parse_program_status(&value.to_string()).ok())
            .flatten();
        match record {
            Some(record) => self.store(block_id, record),
            None => self.remove(block_id),
        }
    }

    fn store(&self, block_id: &str, record: StatusRecord) {
        let step = match record.state {
            RecordState::Working | RecordState::Blocked => record.msg.clone(),
            RecordState::Done | RecordState::Idle | RecordState::Error => None,
        };
        self.shared.steps.set(block_id, record.pid, step);
        lock(&self.shared.records).insert(block_id.to_string(), (self.session_id.clone(), record));
    }

    fn remove(&self, block_id: &str) {
        lock(&self.shared.records).remove(block_id);
        self.shared.steps.clear_block(block_id);
    }

    /// Drops the records and steps of the session. The result is the number of records.
    fn drop_records(&self) -> usize {
        let blocks: Vec<String> = {
            let mut records = lock(&self.shared.records);
            let blocks: Vec<String> = records
                .iter()
                .filter(|(_, (session, _))| *session == self.session_id)
                .map(|(block, _)| block.clone())
                .collect();
            for block in &blocks {
                records.remove(block);
            }
            blocks
        };
        for block in &blocks {
            self.shared.steps.clear_block(block);
        }
        blocks.len()
    }
}
