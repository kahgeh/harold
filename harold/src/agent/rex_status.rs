//! The pure parts of the Rex program status source.
//!
//! A Rex block keeps program status records. The record with no `id` is the one the program
//! writes about itself. This module parses that record and the `rex events` lines about it,
//! maps the record state to an agent state, and keeps each agent's current step in memory.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
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

    /// Each record state is one observed state.
    pub(crate) fn observed(self) -> ObservedAgentState {
        match self {
            Self::Working => ObservedAgentState::Working,
            Self::Blocked => ObservedAgentState::Blocked,
            Self::Done => ObservedAgentState::Done,
            Self::Idle => ObservedAgentState::Idle,
            Self::Error => ObservedAgentState::Error,
        }
    }
}

/// The root record of a block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StatusRecord {
    pub state: RecordState,
    pub pid: u32,
    /// The cleaned `msg`. Only a `working` or `blocked` record gives one.
    pub msg: Option<String>,
}

impl StatusRecord {
    /// The current step: the `msg` of a `working` or `blocked` record.
    fn step(&self) -> Option<String> {
        match self.state {
            RecordState::Working | RecordState::Blocked => self.msg.clone(),
            RecordState::Done | RecordState::Idle | RecordState::Error => None,
        }
    }
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
pub(crate) enum StatusEvent {
    /// The root record changed. The event is only a trigger: it holds no record Harold uses.
    Changed {
        block_id: String,
    },
    /// A record was removed. The event is only a trigger for a new call.
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
            Some(StatusEvent::Changed { block_id })
        }
        "program_status_removed" => {
            let record = data.get("record").unwrap_or(data);
            if has_id(data) || has_id(record) {
                return None;
            }
            // A real event lists the removed ids: `"ids":[""]` is the root record. A list
            // without the root only removed child records.
            if let Some(ids) = data.get("ids").and_then(Value::as_array)
                && !ids.iter().any(|id| id.as_str() == Some(""))
            {
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
pub(crate) struct CurrentSteps {
    inner: Arc<StepsInner>,
}
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

/// The most blocks a stream remembers as closed.
const GONE_LIMIT: usize = 1024;

/// The least time between two starts of the event stream of one session.
pub(crate) const RESTART_GAP: Duration = Duration::from_secs(5);

/// What the streams know about one block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Lookup {
    /// The block's session has a running stream. The record is the root record, if any.
    Stream(Option<StatusRecord>),
    /// The block's session has no running stream.
    NoStream,
    /// The block's session has a running stream, and the last call for the block failed. The
    /// stream holds no record for it, so the observer makes one direct call.
    CallFailed,
}

pub(crate) type Program = Arc<dyn Fn() -> Result<PathBuf, RexError> + Send + Sync>;
pub(crate) type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

/// A record with the generation of the stream that wrote it.
type Records = Mutex<HashMap<String, (u64, StatusRecord)>>;

/// What the stream saw in one block since the observer last looked. A stream turn that ends
/// between two scans leaves `worked`. A block that closed or left the inventory leaves
/// `removed`, so the observer drops what it keeps for the block.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct BlockNote {
    worked: bool,
    removed: bool,
}

type Notes = Mutex<HashMap<String, BlockNote>>;

struct Shared {
    api: Arc<dyn RexApi>,
    steps: CurrentSteps,
    records: Records,
    /// The notes for the observer. Code that takes this lock holds no other lock after it.
    notes: Notes,
    /// The blocks whose last trigger call failed, with the generation of the stream that
    /// saw it. Code that takes this lock holds the records lock first.
    call_failed: Mutex<HashMap<String, u64>>,
    /// The source of stream generations. A stream tags every write with its generation, so
    /// a stale reader cannot change what a newer stream of the same session wrote.
    next_generation: AtomicU64,
    /// The log subscriber of the thread that made the `RexStatus`. Reader threads use it.
    dispatch: Dispatch,
}

impl Shared {
    /// Drops the records and steps that stream `generation` wrote. The result is the
    /// number of records.
    fn drop_stream(&self, generation: u64) -> usize {
        let mut records = lock(&self.records);
        let blocks: Vec<String> = records
            .iter()
            .filter(|(_, (owner, _))| *owner == generation)
            .map(|(block, _)| block.clone())
            .collect();
        for block in &blocks {
            records.remove(block);
            self.steps.clear_block(block);
        }
        lock(&self.call_failed).retain(|_, owner| *owner != generation);
        blocks.len()
    }

    /// Notes a failed call of stream `generation`: the block has no record and the mark is
    /// set. A stream that was asked to stop, or a closed block, sets nothing.
    fn fail(
        &self,
        generation: u64,
        stop: &AtomicBool,
        block_id: &str,
        closed: impl FnOnce() -> bool,
    ) {
        let mut records = lock(&self.records);
        if stop.load(Ordering::Acquire) || closed() {
            return;
        }
        if records
            .get(block_id)
            .is_none_or(|(owner, _)| *owner == generation)
        {
            records.remove(block_id);
            self.steps.clear_block(block_id);
        }
        lock(&self.call_failed).insert(block_id.to_string(), generation);
    }

    /// Clears the mark when stream `generation` set it.
    fn clear_failed(&self, generation: u64, block_id: &str) {
        let mut marks = lock(&self.call_failed);
        if marks.get(block_id) == Some(&generation) {
            marks.remove(block_id);
        }
    }

    fn count(&self, generation: u64) -> usize {
        lock(&self.records)
            .values()
            .filter(|(owner, _)| *owner == generation)
            .count()
    }

    /// Stores a record for stream `generation`, unless the stream was asked to stop. The
    /// check and the write happen under the records lock, which `drop_stream` also takes.
    ///
    /// `closed` runs under the lock too. The reader marks a block closed before it takes the
    /// lock to remove the record, so a store either comes before the removal or sees the mark.
    fn store(
        &self,
        generation: u64,
        stop: &AtomicBool,
        block_id: &str,
        record: StatusRecord,
        closed: impl FnOnce() -> bool,
    ) {
        let mut records = lock(&self.records);
        if stop.load(Ordering::Acquire) || closed() {
            return;
        }
        self.steps.set(block_id, record.pid, record.step());
        if matches!(record.state, RecordState::Working | RecordState::Blocked) {
            lock(&self.notes)
                .entry(block_id.to_string())
                .or_default()
                .worked = true;
        }
        records.insert(block_id.to_string(), (generation, record));
        self.clear_failed(generation, block_id);
    }

    /// Removes the record of the block, unless a newer stream wrote it or the stream was
    /// asked to stop.
    fn remove(&self, generation: u64, stop: &AtomicBool, block_id: &str) {
        let mut records = lock(&self.records);
        if stop.load(Ordering::Acquire) {
            return;
        }
        self.clear_failed(generation, block_id);
        if records
            .get(block_id)
            .is_some_and(|(owner, _)| *owner != generation)
        {
            return;
        }
        records.remove(block_id);
        self.steps.clear_block(block_id);
        self.note_removed(block_id);
    }

    /// The block has no record now. The observer drops what it keeps for the block.
    fn note_removed(&self, block_id: &str) {
        *lock(&self.notes).entry(block_id.to_string()).or_default() = BlockNote {
            worked: false,
            removed: true,
        };
    }
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
    generation: u64,
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
            generation: 0,
            started,
            flags: Arc::new(flags),
            child: Arc::new(Mutex::new(None)),
            thread: None,
        }
    }

    /// Asks the reader to stop and kills the child. The reader reaps the child. The result
    /// is the reader thread, so the caller can wait for it outside any lock.
    fn stop(mut self) -> Option<JoinHandle<()>> {
        self.flags.stop.store(true, Ordering::Release);
        if let Some(child) = lock(&self.child).as_mut() {
            let _ = child.kill();
        }
        self.thread.take()
    }
}

#[derive(Default)]
struct Registry {
    streams: HashMap<String, Stream>,
    /// The session of each block, from the last `sync`.
    owners: HashMap<String, String>,
    /// Set by `stop_all`. A later `sync` starts no stream.
    closed: bool,
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
pub(crate) struct RexStatus {
    shared: Arc<Shared>,
    program: Program,
    clock: Clock,
    registry: Mutex<Registry>,
}
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
                notes: Mutex::new(HashMap::new()),
                call_failed: Mutex::new(HashMap::new()),
                next_generation: AtomicU64::new(1),
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
        if registry.closed {
            return;
        }
        let mut sessions: Vec<(&str, Vec<String>)> = Vec::new();
        // A block that left the inventory gets a removed note.
        for block_id in registry.owners.keys() {
            if !slots.iter().any(|slot| &slot.block_id == block_id) {
                self.shared.note_removed(block_id);
            }
        }
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
                // The reader reaps its own child. `sync` does not wait for it.
                let generation = stream.generation;
                drop(stream.stop());
                self.shared.drop_stream(generation);
            }
        }
        for (id, blocks) in sessions {
            if let Some(stream) = registry.streams.get(id) {
                let ended = stream.flags.ended.load(Ordering::Acquire);
                if !ended || now.saturating_duration_since(stream.started) < RESTART_GAP {
                    continue;
                }
                if let Some(old) = registry.streams.remove(id) {
                    // The reader may still wait for a call thread that is in a `block_call`.
                    // `sync` does not wait for it: the thread ends and reaps its child alone.
                    drop(old.stop());
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
        let flags = Arc::new(Flags::default());
        let child = Arc::new(Mutex::new(Some(child)));
        let generation = self.shared.next_generation.fetch_add(1, Ordering::Relaxed);
        let reader = Reader {
            generation,
            shared: Arc::clone(&self.shared),
            session_id: session_id.to_string(),
            flags: Arc::clone(&flags),
            child: Arc::clone(&child),
        };
        let thread = std::thread::Builder::new()
            .name("rex-status".to_string())
            .spawn(move || reader.run(blocks, stdout))
            .ok();
        if thread.is_none() {
            log_stream(dispatch, session_id, "ended", Some("read_error"), 0);
            flags.ended.store(true, Ordering::Release);
            if let Some(mut child) = lock(&child).take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        Stream {
            generation,
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
        // One take of the records lock reads the record and the mark. `fail` and `store`
        // change both under this lock, so a half-set pair is never read.
        let records = lock(&self.shared.records);
        let record = records.get(block_id).map(|(_, record)| record.clone());
        if record.is_none() && lock(&self.shared.call_failed).contains_key(block_id) {
            return Lookup::CallFailed;
        }
        Lookup::Stream(record)
    }

    /// What the stream saw in the block since the last call, as `(worked, removed)`. The
    /// call clears the note.
    pub(crate) fn take_note(&self, block_id: &str) -> (bool, bool) {
        let note = lock(&self.shared.notes)
            .remove(block_id)
            .unwrap_or_default();
        (note.worked, note.removed)
    }

    /// The generation of each session's stream. Tests use it to see a restart.
    #[cfg(test)]
    pub(crate) fn generations(&self) -> HashMap<String, u64> {
        lock(&self.registry)
            .streams
            .iter()
            .map(|(session, stream)| (session.clone(), stream.generation))
            .collect()
    }

    /// Kills every child and waits for the readers, which reap them, for at most `STOP_WAIT`.
    /// A reader that is still in a call reaps its child later, on its own thread. A `sync`
    /// after this call starts no stream.
    pub(crate) fn stop_all(&self) {
        let mut registry = lock(&self.registry);
        registry.closed = true;
        let streams: Vec<Stream> = registry.streams.drain().map(|(_, stream)| stream).collect();
        drop(registry);
        // `stop` signals each reader and kills its child at once.
        let threads: Vec<_> = streams.into_iter().filter_map(Stream::stop).collect();
        if threads.is_empty() {
            return;
        }
        // The readers end on their own threads. The wait is off the async threads and takes
        // at most `STOP_WAIT` in all. A reader that is still in a call ends later alone.
        let (done, ended) = std::sync::mpsc::channel();
        let joiner = std::thread::Builder::new()
            .name("rex-status-stop".to_string())
            .spawn(move || {
                for thread in threads {
                    let _ = thread.join();
                }
                let _ = done.send(());
            });
        if joiner.is_ok() {
            off_async_threads(|| {
                let _ = ended.recv_timeout(STOP_WAIT);
            });
        }
    }

    /// Notes the current step of what a direct call read, for a block with no stream yet. A
    /// stream does the same: a record gives its step, and no record or a failed call clears it.
    pub(crate) fn note_direct(&self, block_id: &str, record: Option<&StatusRecord>) {
        // A stream that holds a record for the block is newer than the direct call, which
        // can have been in flight while the stream made its first read.
        let records = lock(&self.shared.records);
        if records.contains_key(block_id) {
            return;
        }
        match record {
            Some(record) => self.shared.steps.set(block_id, record.pid, record.step()),
            None => self.shared.steps.clear_block(block_id),
        }
    }
}

/// The most time `stop_all` waits for the reader threads.
const STOP_WAIT: Duration = Duration::from_secs(3);

/// Runs a wait so that it does not hold a tokio worker thread: the runtime moves the other
/// tasks of the worker to another thread. On a current-thread runtime or with no runtime,
/// the wait runs as it is.
fn off_async_threads<R>(wait: impl FnOnce() -> R) -> R {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(wait)
        }
        _ => wait(),
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

/// The most bytes of one event line. Harold discards the rest of a longer line.
const MAX_EVENT_LINE_BYTES: usize = 1024 * 1024;

enum Line {
    Text,
    TooLong,
    End,
}

/// Reads one line into `line`. A line over `MAX_EVENT_LINE_BYTES` is read to its end and
/// dropped.
fn read_event_line(
    reader: &mut BufReader<std::process::ChildStdout>,
    line: &mut Vec<u8>,
) -> std::io::Result<Line> {
    line.clear();
    let mut limited = reader.by_ref().take(MAX_EVENT_LINE_BYTES as u64);
    let read = limited.read_until(b'\n', line)?;
    if read == 0 {
        return Ok(Line::End);
    }
    if line.ends_with(b"\n") || read < MAX_EVENT_LINE_BYTES {
        return Ok(Line::Text);
    }
    loop {
        line.clear();
        let mut limited = reader.by_ref().take(MAX_EVENT_LINE_BYTES as u64);
        let read = limited.read_until(b'\n', line)?;
        if read == 0 || line.ends_with(b"\n") {
            line.clear();
            return Ok(Line::TooLong);
        }
    }
}

/// The blocks that wait for a `program_status` call, and the state of the blocks the call
/// thread works on.
#[derive(Default)]
struct Pending {
    state: Mutex<PendingState>,
    wake: Condvar,
}

#[derive(Default)]
struct PendingState {
    /// In arrival order. A block is here once, however many events came.
    queue: Vec<String>,
    /// The blocks the call thread has taken and not finished.
    in_hand: HashSet<String>,
    /// The in-hand blocks that closed. A mark lasts until the call thread finishes the block.
    closed: HashSet<String>,
    /// The blocks whose last call failed. A block logs `call_failed` once until a call works.
    failed: HashSet<String>,
    /// The blocks the reader saw closed. An event for such a block can come late, after the
    /// close. A failed call for it leaves no mark, no entry and no log. A call that works
    /// means the block is back, and takes it out. The set is cleared at `GONE_LIMIT`.
    /// An entry covers one failed late call. A second late event, after that call, can leave
    /// a mark and an entry again until the stream ends. A block that comes back and whose
    /// first call fails also shows no mark for that one call.
    gone: HashSet<String>,
    done: bool,
}

impl Pending {
    /// Puts the blocks of the first read in hand, so a close that comes first still counts.
    fn seed(&self, blocks: &[String]) {
        lock(&self.state).in_hand.extend(blocks.iter().cloned());
    }

    fn add(&self, block_id: &str) {
        // A block that closed while its call is in hand and then came back gets queued too.
        // The answer of the call in hand is dropped by the closed mark, so the next call
        // reads the block again.
        let mut state = lock(&self.state);
        if !state.queue.iter().any(|b| b == block_id) {
            state.queue.push(block_id.to_string());
            self.wake.notify_one();
        }
    }

    /// Cancels a waiting call. When the call thread holds the block, it marks the block
    /// closed, so the answer of a call in flight is not stored.
    fn close(&self, block_id: &str) {
        let mut state = lock(&self.state);
        state.queue.retain(|b| b != block_id);
        state.failed.remove(block_id);
        if state.gone.len() >= GONE_LIMIT {
            state.gone.clear();
        }
        state.gone.insert(block_id.to_string());
        if state.in_hand.contains(block_id) {
            state.closed.insert(block_id.to_string());
        }
    }

    fn is_closed(&self, block_id: &str) -> bool {
        lock(&self.state).closed.contains(block_id)
    }

    /// The call thread is done with the block, and the mark of a close can go.
    fn finish(&self, block_id: &str) {
        let mut state = lock(&self.state);
        state.in_hand.remove(block_id);
        state.closed.remove(block_id);
    }

    /// Notes a failed call. The result is true for the first failure since a call worked.
    /// A block that closed while the call was in hand leaves no entry.
    fn note_failed(&self, block_id: &str) -> bool {
        let mut state = lock(&self.state);
        if state.closed.contains(block_id) {
            return false;
        }
        state.failed.insert(block_id.to_string())
    }

    fn note_worked(&self, block_id: &str) {
        let mut state = lock(&self.state);
        state.failed.remove(block_id);
        // A close that came while the call was in hand keeps the block gone.
        if !state.closed.contains(block_id) {
            state.gone.remove(block_id);
        }
    }

    /// Whether the failed call was for a block the reader saw closed. The mark goes, so a
    /// late event costs one call and leaves nothing.
    fn take_gone(&self, block_id: &str) -> bool {
        lock(&self.state).gone.remove(block_id)
    }

    fn finish_all(&self) {
        lock(&self.state).done = true;
        self.wake.notify_all();
    }

    /// The next block that waits for a call. It waits until there is one, and it puts the
    /// block in hand. The result is `None` when the stream is done or stopped.
    fn next(&self, stop: &AtomicBool) -> Option<String> {
        let mut state = lock(&self.state);
        loop {
            if state.done || stop.load(Ordering::Acquire) {
                return None;
            }
            if !state.queue.is_empty() {
                let block = state.queue.remove(0);
                state.in_hand.insert(block.clone());
                return Some(block);
            }
            state = self
                .wake
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

/// The reader thread of one stream. It reads lines and never calls Rex.
struct Reader {
    generation: u64,
    shared: Arc<Shared>,
    session_id: String,
    flags: Arc<Flags>,
    child: Arc<Mutex<Option<Child>>>,
}

impl Reader {
    fn run(self, blocks: Vec<String>, stdout: std::process::ChildStdout) {
        let pending = Arc::new(Pending::default());
        let worker = Worker {
            generation: self.generation,
            shared: Arc::clone(&self.shared),
            session_id: self.session_id.clone(),
            flags: Arc::clone(&self.flags),
            pending: Arc::clone(&pending),
        };
        pending.seed(&blocks);
        // The child is already running, so events that come during the first calls wait in
        // the pipe or in `pending`.
        let worker = std::thread::Builder::new()
            .name("rex-status-calls".to_string())
            .spawn(move || worker.run(&blocks))
            .ok();
        let reason = if worker.is_some() {
            self.read_lines(&pending, stdout)
        } else {
            "read_error"
        };
        self.flags.ended.store(true, Ordering::Release);
        pending.finish_all();
        if let Some(worker) = worker {
            let _ = worker.join();
        }
        if let Some(mut child) = lock(&self.child).take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let dropped = self.shared.drop_stream(self.generation);
        log_stream(
            &self.shared.dispatch,
            &self.session_id,
            "ended",
            Some(reason),
            dropped,
        );
    }

    fn read_lines(&self, pending: &Pending, stdout: std::process::ChildStdout) -> &'static str {
        let mut reader = BufReader::new(stdout);
        let mut line = Vec::new();
        loop {
            if self.flags.stop.load(Ordering::Acquire) {
                return "exit";
            }
            match read_event_line(&mut reader, &mut line) {
                Ok(Line::End) => return "exit",
                Err(_) => return "read_error",
                Ok(Line::TooLong) => continue,
                Ok(Line::Text) => {}
            }
            match parse_event_line(&String::from_utf8_lossy(&line)) {
                Some(StatusEvent::Changed { block_id } | StatusEvent::Removed { block_id }) => {
                    pending.add(&block_id);
                }
                Some(StatusEvent::BlockClosed { block_id }) => {
                    // Mark first, then remove under the records lock.
                    pending.close(&block_id);
                    self.shared
                        .remove(self.generation, &self.flags.stop, &block_id);
                }
                None => {}
            }
        }
    }
}

/// The call thread of one stream. It makes one `program_status` call at a time, so a block
/// has at most one call running. Events that come during a call leave the block in `pending`
/// once, which gives one more call.
struct Worker {
    generation: u64,
    shared: Arc<Shared>,
    session_id: String,
    flags: Arc<Flags>,
    pending: Arc<Pending>,
}

impl Worker {
    fn run(self, blocks: &[String]) {
        for block in blocks {
            if self.flags.stop.load(Ordering::Acquire) {
                break;
            }
            self.call(block);
        }
        if !self.flags.stop.load(Ordering::Acquire) {
            // The first read is done, so the count is the number of records the stream found.
            // The line comes before `ready`, so a caller that sees the stream ready sees it.
            log_stream(
                &self.shared.dispatch,
                &self.session_id,
                "started",
                None,
                self.shared.count(self.generation),
            );
        }
        self.flags.ready.store(true, Ordering::Release);
        let pending = Arc::clone(&self.pending);
        while let Some(block) = pending.next(&self.flags.stop) {
            self.call(&block);
        }
    }

    /// Asks Rex for the root record of the block. The answer replaces the record whole.
    fn call(&self, block_id: &str) {
        if self.pending.is_closed(block_id) {
            self.pending.finish(block_id);
            return;
        }
        let answer = self
            .shared
            .api
            .block_call(&self.session_id, block_id, "program_status", &json!({}))
            .and_then(|value| parse_program_status(&value.to_string()));
        let stop = &self.flags.stop;
        if !self.pending.is_closed(block_id) {
            match answer {
                Ok(Some(record)) => {
                    self.pending.note_worked(block_id);
                    self.shared
                        .store(self.generation, stop, block_id, record, || {
                            self.pending.is_closed(block_id)
                        });
                }
                Ok(None) => {
                    self.pending.note_worked(block_id);
                    self.shared.remove(self.generation, stop, block_id);
                }
                Err(_) if self.pending.take_gone(block_id) => {}
                Err(error) => {
                    self.shared.fail(self.generation, stop, block_id, || {
                        self.pending.is_closed(block_id)
                    });
                    if self.pending.note_failed(block_id) && !stop.load(Ordering::Acquire) {
                        log_stream(
                            &self.shared.dispatch,
                            &self.session_id,
                            "call_failed",
                            Some(call_failure_reason(error)),
                            self.shared.count(self.generation),
                        );
                    }
                }
            }
        }
        self.pending.finish(block_id);
    }
}

fn call_failure_reason(error: RexError) -> &'static str {
    match error {
        RexError::Absent => "rex_absent",
        RexError::Timeout => "timeout",
        RexError::Failed => "failed",
        RexError::Malformed => "malformed",
    }
}

#[cfg(test)]
#[path = "rex_status_tests.rs"]
mod tests;
