//! Tests of the status streams that need the private parts of `rex_status.rs`: the shared
//! records, the pending calls and the registry. The fakes here are small, so the file does
//! not depend on the fakes of `rex_tests.rs`.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicUsize;

use super::*;

// -- fakes ---------------------------------------------------------------------------

/// The first `program_status` call for `block` waits until the gate opens. Later calls
/// answer `later` at once.
struct Gate {
    block: String,
    calls: AtomicUsize,
    open: Mutex<bool>,
    changed: Condvar,
    first: Result<Value, RexError>,
    later: Result<Value, RexError>,
}

impl Gate {
    fn new(
        block: &str,
        first: Result<Value, RexError>,
        later: Result<Value, RexError>,
    ) -> Arc<Self> {
        Arc::new(Self {
            block: block.to_string(),
            calls: AtomicUsize::new(0),
            open: Mutex::new(false),
            changed: Condvar::new(),
            first,
            later,
        })
    }

    fn call(&self) -> Result<Value, RexError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) > 0 {
            return self.later.clone();
        }
        let mut open = lock(&self.open);
        while !*open {
            open = self
                .changed
                .wait(open)
                .unwrap_or_else(PoisonError::into_inner);
        }
        self.first.clone()
    }

    fn release(&self) {
        *lock(&self.open) = true;
        self.changed.notify_all();
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// Opens the gate when it drops. Declare it after the `RexStatus`, so it drops first:
    /// the drop of `RexStatus` waits for the readers, and a call in a closed gate never ends.
    fn guard(self: &Arc<Self>) -> GateGuard {
        GateGuard(Arc::clone(self))
    }
}

struct GateGuard(Arc<Gate>);

impl Drop for GateGuard {
    fn drop(&mut self) {
        self.0.release();
    }
}

/// A `rex` that answers `program_status` from a table, or through a gate.
#[derive(Default)]
struct Api {
    replies: HashMap<String, Result<Value, RexError>>,
    gate: Option<Arc<Gate>>,
}

impl RexApi for Api {
    fn sessions(&self) -> Result<Vec<super::super::RexSession>, RexError> {
        Err(RexError::Failed)
    }
    fn session_view(&self, _: &str) -> Result<Vec<Vec<String>>, RexError> {
        Err(RexError::Failed)
    }
    fn process(&self, _: &str, _: &str) -> Result<super::super::RexProcess, RexError> {
        Err(RexError::Failed)
    }
    fn start_time_ms(&self, _: u32) -> Option<i64> {
        None
    }
    fn block_call(&self, _: &str, block: &str, method: &str, _: &Value) -> Result<Value, RexError> {
        assert_eq!(method, "program_status");
        if let Some(gate) = self.gate.as_ref().filter(|gate| gate.block == block) {
            return gate.call();
        }
        self.replies
            .get(block)
            .cloned()
            .unwrap_or(Err(RexError::Failed))
    }
    fn paste(&self, _: &str, _: &str, _: &str) -> Result<(), RexError> {
        Err(RexError::Failed)
    }
    fn send_key(&self, _: &str, _: &str, _: &str) -> Result<(), RexError> {
        Err(RexError::Failed)
    }
}

fn reply(state: &str, pid: u32, msg: &str) -> Value {
    json!({"records": [{"state": state, "owner": {"pid": pid}, "msg": msg}]})
}

fn no_root() -> Value {
    json!({"records": [{"id": "c", "state": "idle", "owner": {"pid": 1}}]})
}

fn slot(session: &str, block: &str) -> RexBlockSlot {
    RexBlockSlot {
        session_id: session.to_string(),
        session_label: session.to_string(),
        window: 1,
        pane: 1,
        block_id: block.to_string(),
    }
}

fn record(state: RecordState, pid: u32, msg: Option<&str>) -> StatusRecord {
    StatusRecord {
        state,
        pid,
        msg: msg.map(str::to_string),
    }
}

/// A fake `rex` program. `events` starts and, with `hold`, stays open until it is killed.
struct Script {
    dir: PathBuf,
}

impl Script {
    fn new(name: &str, hold: bool) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "harold-rex-status-private-{}-{name}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let ending = if hold { "exec sleep 30\n" } else { "exit 0\n" };
        install_script(
            &dir.join("rex"),
            &format!("#!/bin/sh\n[ \"$1\" = --probe ] && exit 0\n{ending}"),
        );
        Self { dir }
    }

    fn program(&self) -> Program {
        let path = self.dir.join("rex");
        Arc::new(move || Ok(path.clone()))
    }
}

impl Drop for Script {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Writes an executable script and waits until the system lets it start. Another test
/// thread can fork while the file is open for writing, and until that child runs `exec` a
/// start of the script fails with "Text file busy". The probe run succeeds only when no
/// such child is left.
fn install_script(path: &Path, script: &str) {
    std::fs::write(path, script).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match Command::new(path).arg("--probe").status() {
            Ok(_) => return,
            Err(error) if error.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                assert!(Instant::now() < deadline, "the script stays busy: {error}");
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("the probe run of the script failed: {error}"),
        }
    }
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done() {
        assert!(Instant::now() < deadline, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Runs a test body on a named thread. A body that does not end in 30 s fails with the
/// test's name.
fn within_30s(name: &'static str, body: impl FnOnce() + Send + 'static) {
    let (done, ended) = std::sync::mpsc::channel();
    let handle = std::thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));
            let _ = done.send(());
            if let Err(panic) = result {
                std::panic::resume_unwind(panic);
            }
        })
        .unwrap();
    if ended.recv_timeout(Duration::from_secs(30)).is_err() {
        panic!("{name} did not end in 30 s");
    }
    if let Err(panic) = handle.join() {
        std::panic::resume_unwind(panic);
    }
}

#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Logs {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        lock(&self.0).extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Logs {
    fn with(&self, message: &str) -> Vec<String> {
        String::from_utf8_lossy(&lock(&self.0))
            .lines()
            .filter(|line| line.contains(message))
            .map(str::to_string)
            .collect()
    }
}

fn capture_logs() -> (Logs, tracing::subscriber::DefaultGuard) {
    crate::test_logs::keep_second_dispatcher();
    let logs = Logs::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::INFO)
        .finish();
    (logs, tracing::subscriber::set_default(subscriber))
}

fn status_of(api: Api, script: &Script) -> (RexStatus, CurrentSteps) {
    let steps = CurrentSteps::default();
    (
        RexStatus::new(Arc::new(api), script.program(), steps.clone()),
        steps,
    )
}

// -- the call-failed mark ------------------------------------------------------------

#[test]
fn a_stopped_stream_cannot_set_the_call_failed_mark() {
    let script = Script::new("stalemark", true);
    let (status, _) = status_of(Api::default(), &script);
    // A stream that was asked to stop: its late failed call sets no mark.
    let stopped = AtomicBool::new(true);
    status.shared.fail(7, &stopped, "block:a", || false);
    assert!(lock(&status.shared.call_failed).is_empty());
    // The same call from a stream that runs sets the mark. The check above is the stop check.
    let running = AtomicBool::new(false);
    status.shared.fail(7, &running, "block:a", || false);
    assert_eq!(lock(&status.shared.call_failed).get("block:a"), Some(&7));
}

#[test]
fn a_status_call_that_times_out_sets_the_mark_and_logs_timeout() {
    let (logs, _guard) = capture_logs();
    let script = Script::new("timeout", true);
    let api = Api {
        replies: [("block:a".to_string(), Err(RexError::Timeout))].into(),
        ..Api::default()
    };
    let (status, _) = status_of(api, &script);
    status.sync(&[slot("session:1", "block:a")]);
    wait_until("the mark is set", || {
        status.lookup("block:a") == Lookup::CallFailed
    });
    let failed: Vec<String> = logs
        .with("agent status stream")
        .into_iter()
        .filter(|line| line.contains("event=\"call_failed\""))
        .collect();
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert!(failed[0].contains("reason=\"timeout\""), "{}", failed[0]);
}

#[test]
fn a_closed_block_leaves_no_failed_entry_and_an_open_block_keeps_it_until_a_call_works() {
    let pending = Pending::default();
    // A block that is waiting: the entry goes with the close.
    assert!(pending.note_failed("block:w"));
    pending.close("block:w");
    assert!(!lock(&pending.state).failed.contains("block:w"));
    // A block whose call is in hand closes, and then the call reports its failure.
    pending.seed(&["block:h".to_string()]);
    pending.close("block:h");
    assert!(!pending.note_failed("block:h"));
    assert!(lock(&pending.state).failed.is_empty());
    // A block that stays open keeps its entry until a call works.
    assert!(pending.note_failed("block:o"));
    assert!(!pending.note_failed("block:o"));
    assert!(lock(&pending.state).failed.contains("block:o"));
    pending.note_worked("block:o");
    assert!(lock(&pending.state).failed.is_empty());
}

// -- the first event of a returning block -----------------------------------------------

#[test]
fn a_block_that_closes_while_its_call_is_in_hand_and_comes_back_keeps_its_first_event() {
    let pending = Pending::default();
    pending.seed(&["block:a".to_string()]);
    pending.close("block:a");
    assert!(pending.is_closed("block:a"));
    // The block comes back, and its first event arrives while the old call is in hand.
    pending.add("block:a");
    assert_eq!(lock(&pending.state).queue, vec!["block:a".to_string()]);
    // The call thread finishes the old call. The mark of the close goes with it.
    pending.finish("block:a");
    assert!(!pending.is_closed("block:a"));
    let stop = AtomicBool::new(false);
    assert_eq!(pending.next(&stop).as_deref(), Some("block:a"));
}

// -- lookup reads the record and the mark together ---------------------------------------

#[test]
fn a_lookup_never_reads_a_record_and_a_mark_half_set() {
    let script = Script::new("halfway", true);
    let (status, _) = status_of(Api::default(), &script);
    let status = Arc::new(status);
    {
        let flags = Flags::default();
        flags.ready.store(true, Ordering::Release);
        let mut registry = lock(&status.registry);
        registry.owners.insert("block:a".into(), "session:1".into());
        registry.streams.insert(
            "session:1".into(),
            Stream {
                generation: 1,
                started: Instant::now(),
                flags: Arc::new(flags),
                child: Arc::new(Mutex::new(None)),
                thread: None,
            },
        );
    }
    let running = Arc::new(AtomicBool::new(false));
    let good = record(RecordState::Working, 5, Some("x"));
    // The pair is either (a record, no mark) or (no record, a mark).
    status
        .shared
        .store(1, &running, "block:a", good.clone(), || false);
    let writer = {
        let status = Arc::clone(&status);
        let running = Arc::clone(&running);
        std::thread::spawn(move || {
            for _ in 0..20_000 {
                status.shared.fail(1, &running, "block:a", || false);
                status
                    .shared
                    .store(1, &running, "block:a", good.clone(), || false);
            }
        })
    };
    let mut reads = 0;
    while !writer.is_finished() || reads < 1_000 {
        assert_ne!(status.lookup("block:a"), Lookup::Stream(None));
        reads += 1;
    }
    writer.join().unwrap();
}

// -- stop_all and sync do not wait on the wrong thread -------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn stop_all_blocks_no_tokio_worker_and_ends_within_three_seconds() {
    let script = Script::new("stopasync", true);
    let gate = Gate::new("block:a", Ok(no_root()), Ok(no_root()));
    let api = Api {
        gate: Some(Arc::clone(&gate)),
        ..Api::default()
    };
    let (status, _) = status_of(api, &script);
    let status = Arc::new(status);
    let _open = gate.guard();
    status.sync(&[slot("session:1", "block:a")]);
    while gate.calls() != 1 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // The reader cannot end: its call thread waits in the gate. This watchdog opens the gate
    // late, so a stop that waits for the reader without a limit ends late and fails below.
    let watchdog = Arc::clone(&gate);
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(8));
        watchdog.release();
    });
    let ticks = Arc::new(AtomicUsize::new(0));
    let ticker = {
        let ticks = Arc::clone(&ticks);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(10)).await;
                ticks.fetch_add(1, Ordering::SeqCst);
            }
        })
    };
    let started = Instant::now();
    let stopper = Arc::clone(&status);
    tokio::spawn(async move { stopper.stop_all() })
        .await
        .unwrap();
    let took = started.elapsed();
    ticker.abort();
    // The only worker thread is free during the stop, so the ticker runs.
    assert!(ticks.load(Ordering::SeqCst) >= 100, "{ticks:?}");
    assert!(took >= Duration::from_millis(2500), "{took:?}");
    assert!(took < Duration::from_secs(5), "{took:?}");
}

#[test]
fn a_restart_of_an_ended_stream_does_not_wait_for_the_old_reader() {
    within_30s(
        "a_restart_of_an_ended_stream_does_not_wait_for_the_old_reader",
        || {
            // The program ends at once, but its call thread waits in the gate. So the
            // stream is ended and its reader is not.
            let script = Script::new("restart", false);
            let gate = Gate::new("block:a", Ok(no_root()), Ok(no_root()));
            let api = Api {
                gate: Some(Arc::clone(&gate)),
                ..Api::default()
            };
            let offset = Arc::new(Mutex::new(Duration::ZERO));
            let base = Instant::now();
            let clock: Clock = {
                let offset = Arc::clone(&offset);
                Arc::new(move || base + *lock(&offset))
            };
            let status = RexStatus::with_clock(
                Arc::new(api),
                script.program(),
                CurrentSteps::default(),
                clock,
            );
            let _open = gate.guard();
            let slots = [slot("session:1", "block:a")];
            status.sync(&slots);
            wait_until("the call is in the gate", || gate.calls() == 1);
            wait_until("the stream ended", || {
                lock(&status.registry)
                    .streams
                    .values()
                    .all(|stream| stream.flags.ended.load(Ordering::Acquire))
            });
            let before = status.generations();
            *lock(&offset) = RESTART_GAP;
            // The watchdog opens the gate late, so a `sync` that waits ends late and fails.
            let watchdog = Arc::clone(&gate);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(8));
                watchdog.release();
            });
            let started = Instant::now();
            status.sync(&slots);
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "{:?}",
                started.elapsed()
            );
            assert_ne!(status.generations(), before);
        },
    );
}

// -- the started line and the direct call ------------------------------------------------

#[test]
fn the_started_line_holds_the_number_of_records_the_first_read_found() {
    let (logs, _guard) = capture_logs();
    let script = Script::new("started", true);
    let api = Api {
        replies: [
            ("block:a".to_string(), Ok(reply("working", 5, "step"))),
            ("block:n".to_string(), Ok(no_root())),
        ]
        .into(),
        ..Api::default()
    };
    let (status, _) = status_of(api, &script);
    status.sync(&[slot("session:1", "block:a"), slot("session:1", "block:n")]);
    wait_until("the stream is ready", || {
        status.lookup("block:a") != Lookup::NoStream
    });
    let started: Vec<String> = logs
        .with("agent status stream")
        .into_iter()
        .filter(|line| line.contains("event=\"started\""))
        .collect();
    assert_eq!(started.len(), 1, "{started:?}");
    assert!(started[0].contains("records=1"), "{}", started[0]);
}

#[test]
fn a_direct_call_gives_the_current_step_as_a_stream_does() {
    let script = Script::new("direct", true);
    let (status, steps) = status_of(Api::default(), &script);
    status.note_direct("block:a", &record(RecordState::Working, 7, Some("step")));
    assert_eq!(steps.get("block:a", 7).as_deref(), Some("step"));
    // A record that is not working or blocked gives no step.
    status.note_direct("block:a", &record(RecordState::Done, 7, None));
    assert_eq!(steps.get("block:a", 7), None);
}

#[test]
fn a_direct_call_does_not_replace_the_step_of_a_stream_record() {
    let script = Script::new("directnewer", true);
    let (status, steps) = status_of(Api::default(), &script);
    let running = AtomicBool::new(false);
    let newer = record(RecordState::Working, 7, Some("newer"));
    status.shared.store(1, &running, "block:a", newer, || false);
    // A direct call that was in flight during the first read brings an older answer.
    status.note_direct("block:a", &record(RecordState::Working, 7, Some("older")));
    assert_eq!(steps.get("block:a", 7).as_deref(), Some("newer"));
}
