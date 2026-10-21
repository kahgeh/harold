//! A call with the chief, on fake ports and paused time: a fake microphone fed by the
//! test, a fake sink that records what it plays, a fake voice worker and a fake chief pane.

use super::phrases::{
    BYE, CHIEF_BUSY, NO_ANSWER, NO_ANSWER_IN_THREAD, NO_CHIEF, NOTHING_HEARD, VOICE_FAILED,
    VOICE_STOPPED,
};
use super::wav::tick_wav;
use super::worker::{
    Health, ProcessWorker, REQUEST_LIMIT, Transcript, VoiceWorker, WorkerCommand, WorkerError,
};
use super::*;
use crate::agent::domain::EffectiveAgentState;
use crate::outbound::fake::CapturedLogs;
use crate::telemetry::{self, StartedBy, test_support};
use crate::timed_prompts::tests::{CHIEF, Fake, Key};
use crate::timed_prompts::{AgentPane, AgentsView};
use std::collections::VecDeque;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;
use tokio::sync::{Notify, mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep};

// ---------------------------------------------------------------------------
// Samples
// ---------------------------------------------------------------------------

pub(crate) fn loud(ms: usize) -> Vec<i16> {
    (0..ms * 16)
        .map(|i| (8000.0 * (i as f64 * 0.3).sin()) as i16)
        .collect()
}

pub(crate) fn quiet(ms: usize) -> Vec<i16> {
    vec![0; ms * 16]
}

// ---------------------------------------------------------------------------
// The fake worker
// ---------------------------------------------------------------------------

/// What the fake worker answers to one transcribe request.
pub(crate) enum Heard {
    Text(String),
    /// Whisper's no-speech probability is 0.7.
    Unlikely(String),
    Fail,
    TimeOut,
    /// The worker exits: its health says so, and the request answers `Exited`.
    Exit,
    /// The same, with the request answering a second after the health.
    ExitLater,
    /// `Text`, answered a second after the request.
    Slow(String),
    FailToLoad,
}

pub(crate) fn text(heard: &str) -> Heard {
    Heard::Text(heard.to_string())
}

#[derive(Default)]
pub(crate) struct WorkerLog {
    pub(crate) script: VecDeque<Heard>,
    pub(crate) starts: usize,
    /// The samples in each turn sent to be transcribed.
    pub(crate) transcribed: Vec<usize>,
    /// The text of each say request.
    pub(crate) said: Vec<String>,
    /// What each say request answers in turn, with the health left as it is; once used
    /// up, each say request writes its file.
    pub(crate) say_script: VecDeque<WorkerError>,
    pub(crate) closes: usize,
    /// Whether each worker starts `Loading` rather than `Ready`.
    pub(crate) starts_loading: bool,
    /// The health of the worker started last.
    pub(crate) health: Option<Arc<watch::Sender<Health>>>,
}

pub(crate) struct FakeWorker(Arc<Mutex<WorkerLog>>, Arc<watch::Sender<Health>>);

impl VoiceWorker for FakeWorker {
    fn health(&self) -> watch::Receiver<Health> {
        self.1.subscribe()
    }

    async fn transcribe(&mut self, wav: &Path) -> Result<Transcript, WorkerError> {
        let heard = {
            let mut log = self.0.lock().unwrap();
            let bytes =
                std::fs::read(wav).expect("the turn's WAV is there while it is transcribed");
            log.transcribed.push((bytes.len() - 44) / 2);
            log.script
                .pop_front()
                .unwrap_or_else(|| text("hello there"))
        };
        let (text, no_speech_prob) = match heard {
            Heard::Text(text) => (text, 0.1),
            Heard::Unlikely(text) => (text, 0.7),
            Heard::Slow(text) => {
                sleep(Duration::from_secs(1)).await;
                (text, 0.1)
            }
            Heard::Fail => return Err(WorkerError::Failed),
            Heard::Exit => {
                self.1.send_replace(Health::Gone(WorkerError::Exited));
                return Err(WorkerError::Exited);
            }
            Heard::ExitLater => {
                self.1.send_replace(Health::Gone(WorkerError::Exited));
                sleep(Duration::from_secs(1)).await;
                return Err(WorkerError::Exited);
            }
            Heard::FailToLoad => return Err(WorkerError::FailedToLoad),
            Heard::TimeOut => {
                sleep(REQUEST_LIMIT).await;
                return Err(WorkerError::TimedOut);
            }
        };
        Ok(Transcript {
            text,
            no_speech_prob,
        })
    }

    async fn say(&mut self, text: &str, wav: &Path) -> Result<(), WorkerError> {
        let scripted = {
            let mut log = self.0.lock().unwrap();
            log.said.push(text.to_string());
            log.say_script.pop_front()
        };
        if let Some(error) = scripted {
            return Err(error);
        }
        std::fs::write(wav, text).map_err(|_| WorkerError::Failed)
    }

    async fn close(self) {
        self.0.lock().unwrap().closes += 1;
    }
}

// ---------------------------------------------------------------------------
// The fake sink and source
// ---------------------------------------------------------------------------

/// Records what each played file holds: the text the fake worker wrote, or `tick`.
#[derive(Default)]
pub(crate) struct FakeSink {
    pub(crate) played: Mutex<Vec<(Instant, String)>>,
    /// Whether each play stays open until `release` or `stop`.
    pub(crate) hold: AtomicBool,
    pub(crate) playing: AtomicBool,
    pub(crate) release: Notify,
    pub(crate) stops: AtomicUsize,
    /// Whether each play fails.
    pub(crate) fails: AtomicBool,
    /// What each played sound said, as the core told the sink.
    pub(crate) said: Mutex<Vec<Said>>,
}

impl FakeSink {
    pub(crate) fn release(&self) {
        if self.playing.swap(false, Ordering::SeqCst) {
            self.release.notify_one();
        }
    }
}

impl AudioSink for FakeSink {
    async fn play(&self, wav: &Path) -> Result<(), String> {
        if self.fails.load(Ordering::SeqCst) {
            return Err("afplay exited with 1".to_string());
        }
        let bytes = std::fs::read(wav).map_err(|error| error.to_string())?;
        let what = if bytes == tick_wav() {
            "tick".to_string()
        } else {
            String::from_utf8_lossy(&bytes).into_owned()
        };
        self.played.lock().unwrap().push((Instant::now(), what));
        if self.hold.load(Ordering::SeqCst) {
            self.playing.store(true, Ordering::SeqCst);
            self.release.notified().await;
        }
        Ok(())
    }

    async fn play_said(&self, wav: &Path, said: &Said) -> Result<(), String> {
        self.said.lock().unwrap().push(said.clone());
        self.play(wav).await
    }

    fn stop(&self) {
        self.stops.fetch_add(1, Ordering::SeqCst);
        self.release();
    }
}

/// A block of samples and when it was recorded; `None` is stamped when it is read.
type MicBlock = (Vec<i16>, Option<Instant>);

pub(crate) struct FakeSource(mpsc::UnboundedReceiver<MicBlock>);

impl AudioSource for FakeSource {
    async fn next_block(&mut self) -> Option<Vec<i16>> {
        self.0.recv().await.map(|(samples, _)| samples)
    }

    async fn next_stamped(&mut self) -> Option<(Vec<i16>, Instant)> {
        let (samples, at) = self.0.recv().await?;
        Some((samples, at.unwrap_or_else(Instant::now)))
    }
}

/// Counts its drop in `dropped`.
pub(crate) struct FakeAwake(Arc<AtomicUsize>);

impl Drop for FakeAwake {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------------------
// The fake ports
// ---------------------------------------------------------------------------

pub(crate) const UPDATE: &str = "The chief has finished and is waiting.";

#[derive(Default)]
pub(crate) struct PortsState {
    mic: Option<mpsc::UnboundedSender<MicBlock>>,
    pub(crate) sources: usize,
    /// The next source ends at once.
    pub(crate) source_ends: bool,
    pub(crate) tts: Vec<String>,
    pub(crate) desk_stops: usize,
    pub(crate) update_prompts: Vec<String>,
    /// The trace of the span each turn-end update was asked with.
    pub(crate) update_traces: Vec<opentelemetry::trace::TraceId>,
    pub(crate) muted: bool,
    pub(crate) fail_awake: bool,
    pub(crate) awake_taken: usize,
    /// Whether each `[tts]` line goes on until `tts_release` is notified.
    pub(crate) hold_tts: bool,
    /// The reason of each `call_ended`.
    pub(crate) ended: Vec<&'static str>,
    /// What `no_answer_phrase` returns, when set.
    pub(crate) no_answer: Option<&'static str>,
}

struct PortsInner {
    panes: Fake,
    sink: Arc<FakeSink>,
    worker: Arc<Mutex<WorkerLog>>,
    state: Mutex<PortsState>,
    awake_dropped: Arc<AtomicUsize>,
    tts_release: Notify,
    root: PathBuf,
}

impl Drop for PortsInner {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[derive(Clone)]
pub(crate) struct FakePorts(Arc<PortsInner>);

impl FakePorts {
    pub(crate) fn new() -> Self {
        let root = std::env::temp_dir().join(format!("harold-call-{}", uuid::Uuid::new_v4()));
        Self(Arc::new(PortsInner {
            panes: Fake::at("2026-10-14", "09:00:00"),
            sink: Arc::default(),
            worker: Arc::default(),
            state: Mutex::default(),
            awake_dropped: Arc::default(),
            tts_release: Notify::new(),
            root,
        }))
    }

    pub(crate) fn state(&self) -> MutexGuard<'_, PortsState> {
        self.0.state.lock().unwrap()
    }

    /// The log of the fake workers' requests.
    pub(crate) fn worker_log(&self) -> MutexGuard<'_, WorkerLog> {
        self.0.worker.lock().unwrap()
    }

    /// The fake chief panes, and the keys typed into them.
    pub(crate) fn fake_panes(&self) -> &Fake {
        &self.0.panes
    }
}

impl CallPorts for FakePorts {
    type Source = FakeSource;
    type Sink = FakeSink;
    type Worker = FakeWorker;
    type Panes = Fake;
    type Awake = FakeAwake;

    fn open_source(&self) -> Result<FakeSource, String> {
        let mut state = self.state();
        state.sources += 1;
        let (tx, rx) = mpsc::unbounded_channel();
        if !state.source_ends {
            state.mic = Some(tx);
        }
        Ok(FakeSource(rx))
    }

    fn sink(&self) -> Arc<FakeSink> {
        Arc::clone(&self.0.sink)
    }

    fn start_worker(&self, _call_dir: &Path) -> Result<FakeWorker, WorkerError> {
        let mut log = self.0.worker.lock().unwrap();
        log.starts += 1;
        let first = if log.starts_loading {
            Health::Loading
        } else {
            Health::Ready
        };
        let health = Arc::new(watch::channel(first).0);
        log.health = Some(Arc::clone(&health));
        Ok(FakeWorker(Arc::clone(&self.0.worker), health))
    }

    fn panes(&self) -> &Fake {
        &self.0.panes
    }

    fn chief_folder(&self) -> Option<String> {
        Some(CHIEF.to_string())
    }

    fn chief_session(&self) -> Option<String> {
        None
    }

    async fn speak_with_tts(&self, line: &str) {
        let hold = {
            let mut state = self.state();
            state.tts.push(line.to_string());
            state.hold_tts
        };
        if hold {
            self.0.tts_release.notified().await;
        }
    }

    fn stop_desk_line(&self) {
        self.state().desk_stops += 1;
    }

    async fn turn_end_update(&self, prompt: &str, turn: &tracing::Span) -> String {
        self.state().update_prompts.push(prompt.to_string());
        let trace = {
            use opentelemetry::trace::TraceContextExt as _;
            use tracing_opentelemetry::OpenTelemetrySpanExt as _;
            turn.context().span().span_context().trace_id()
        };
        self.state().update_traces.push(trace);
        UPDATE.to_string()
    }

    fn calls_dir(&self) -> PathBuf {
        self.0.root.join("calls")
    }

    fn phrases_dir(&self) -> PathBuf {
        self.0.root.join("phrases")
    }

    fn voice_name(&self) -> String {
        "af_heart".to_string()
    }

    fn voice_muted(&self) -> bool {
        self.state().muted
    }

    fn call_ended(&self, reason: &'static str) {
        self.state().ended.push(reason);
    }

    fn no_answer_phrase(&self) -> &'static str {
        self.state().no_answer.unwrap_or(NO_ANSWER)
    }

    fn keep_awake(&self) -> Result<FakeAwake, String> {
        let mut state = self.state();
        if state.fail_awake {
            return Err("caffeinate not found".to_string());
        }
        state.awake_taken += 1;
        Ok(FakeAwake(Arc::clone(&self.0.awake_dropped)))
    }
}

// ---------------------------------------------------------------------------
// The harness
// ---------------------------------------------------------------------------

/// Lets every task that can run, run, without moving the paused clock.
pub(crate) async fn settle() {
    for _ in 0..200 {
        tokio::task::yield_now().await;
    }
}

/// Past the 0.3 s after a sound in which the microphone is not heard.
pub(crate) async fn past_the_tail() {
    sleep(Duration::from_millis(400)).await;
}

pub(crate) fn typed(pane: &str, transcript: &str) -> Vec<Key> {
    vec![
        Key::Text(pane.to_string(), format!("🎙 {transcript}")),
        Key::Enter(pane.to_string()),
    ]
}

fn chief_pane(pane_id: &str, state: EffectiveAgentState) -> AgentPane {
    AgentPane {
        pane_id: pane_id.to_string(),
        folder: CHIEF.to_string(),
        reads_as_claude: true,
        state,
        session: "pods".to_string(),
        window_index: 0,
        pane_index: 1,
        started_at_ms: 1_000,
    }
}

struct Harness {
    ports: FakePorts,
    calls: Arc<CallControl<FakePorts>>,
}

impl Harness {
    fn new() -> Self {
        let ports = FakePorts::new();
        let calls = Arc::new(CallControl::new(ports.clone(), true));
        Self { ports, calls }
    }

    fn script(&self, heard: impl IntoIterator<Item = Heard>) {
        self.worker().script.extend(heard);
    }

    /// Starts a call and gives it half a second of background.
    async fn start(&self) -> CallStatus {
        let status = self.calls.start().await;
        self.feed(quiet(500));
        settle().await;
        status
    }

    fn feed(&self, samples: Vec<i16>) {
        let mic = self.ports.state().mic.clone();
        let Some(mic) = mic else {
            return;
        };
        for block in samples.chunks(1600) {
            let _ = mic.send((block.to_vec(), None));
        }
    }

    /// Feeds `samples` as recorded from `at`: each block of 1,600 samples is stamped `at`
    /// plus its offset in the feed at 16 kHz.
    fn feed_captured(&self, samples: Vec<i16>, at: Instant) {
        let mic = self.ports.state().mic.clone();
        let Some(mic) = mic else {
            return;
        };
        for (n, block) in samples.chunks(1600).enumerate() {
            let offset = Duration::from_millis(n as u64 * 100);
            let _ = mic.send((block.to_vec(), Some(at + offset)));
        }
    }

    /// One second of speech and the 1.2 s of silence that ends it.
    async fn speak(&self) {
        self.feed(loud(1000));
        self.feed(quiet(1200));
        settle().await;
    }

    fn end_source(&self) {
        self.ports.state().mic = None;
    }

    fn say(&self, line: &str, by: Speaker) -> JoinHandle<Option<bool>> {
        let calls = Arc::clone(&self.calls);
        let line = line.to_string();
        tokio::spawn(async move { calls.say(&line, by).await })
    }

    fn status(&self) -> CallStatus {
        self.calls.status()
    }

    fn state(&self) -> CallState {
        self.status().state
    }

    fn keys(&self) -> Vec<Key> {
        self.ports.fake_panes().keys()
    }

    fn set_chief(&self, state: EffectiveAgentState) {
        self.ports.0.panes.set(|s| s.view.panes[0].state = state);
    }

    fn sink(&self) -> &FakeSink {
        &self.ports.0.sink
    }

    fn played(&self) -> Vec<String> {
        let played = self.sink().played.lock().unwrap();
        played.iter().map(|(_, what)| what.clone()).collect()
    }

    fn played_at(&self) -> Vec<(Instant, String)> {
        self.sink().played.lock().unwrap().clone()
    }

    fn tts(&self) -> Vec<String> {
        self.ports.state().tts.clone()
    }

    fn worker(&self) -> MutexGuard<'_, WorkerLog> {
        self.ports.worker_log()
    }

    fn call_dirs(&self) -> Vec<PathBuf> {
        match std::fs::read_dir(self.ports.calls_dir()) {
            Ok(entries) => entries.map(|entry| entry.unwrap().path()).collect(),
            Err(_) => Vec::new(),
        }
    }

    fn turn_dirs(&self) -> Vec<PathBuf> {
        self.call_dirs()
            .iter()
            .flat_map(|dir| std::fs::read_dir(dir).unwrap())
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("turn-")
            })
            .collect()
    }

    /// Sets the health of the worker started last.
    fn set_health(&self, health: Health) {
        let sender = self.worker().health.clone().expect("a worker was started");
        sender.send_replace(health);
    }

    fn awake_held(&self) -> usize {
        self.ports.state().awake_taken - self.ports.0.awake_dropped.load(Ordering::SeqCst)
    }

    /// Plays the chief's answer to the turn just typed, and waits until it has ended.
    async fn answer(&self, line: &str) {
        let said = self.say(line, Speaker::Chief);
        assert_eq!(said.await.unwrap(), Some(true));
        settle().await;
        past_the_tail().await;
    }
}

// ---------------------------------------------------------------------------
// Acceptance 1: a turn
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_turn_is_typed_with_the_mark_and_its_folder_deleted() {
    let h = Harness::new();
    h.script([text("hello there")]);
    assert_eq!(h.start().await.state, CallState::Listening);

    h.speak().await;

    assert_eq!(h.keys(), typed("%2", "hello there"));
    assert_eq!(h.worker().transcribed.len(), 1);
    assert!(h.turn_dirs().is_empty());
    let status = h.status();
    assert_eq!(status.last_result, Some(TurnResult::Typed));
    assert_eq!(status.last_transcript.as_deref(), Some("hello there"));
    assert_eq!(status.state, CallState::Waiting);
    assert_eq!(status.chief_pane_id.as_deref(), Some("%2"));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn one_second_of_silence_stays_one_turn() {
    let h = Harness::new();
    h.start().await;

    h.feed(loud(1000));
    h.feed(quiet(1000));
    h.speak().await;

    assert_eq!(h.worker().transcribed.len(), 1);
    assert_eq!(h.keys().len(), 2);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_call_folder_is_private_and_holds_the_tick() {
    let h = Harness::new();
    h.start().await;

    let dirs = h.call_dirs();
    assert_eq!(dirs.len(), 1);
    let name = dirs[0].file_name().unwrap().to_string_lossy().into_owned();
    assert!(name.starts_with("call-"), "{name}");
    let mode = std::fs::metadata(&dirs[0]).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o700);
    assert_eq!(std::fs::read(dirs[0].join("tick.wav")).unwrap(), tick_wav());
    assert_eq!(h.ports.state().desk_stops, 1);
}

// ---------------------------------------------------------------------------
// Acceptance 2: turn-taking
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn three_turns_in_a_row_need_no_key() {
    let h = Harness::new();
    h.script([text("one"), text("two"), text("three")]);
    h.start().await;
    h.sink().hold.store(true, Ordering::SeqCst);

    for n in 1..=3 {
        h.speak().await;
        assert_eq!(h.keys().len(), 2 * n);
        let said = h.say(&format!("answer {n}"), Speaker::Chief);
        settle().await;
        assert_eq!(h.played().last(), Some(&format!("answer {n}")));
        assert_eq!(h.state(), CallState::Speaking);
        h.sink().release();
        assert_eq!(said.await.unwrap(), Some(true));
        settle().await;
        assert_eq!(h.state(), CallState::Listening);
        past_the_tail().await;
    }

    let mut want = typed("%2", "one");
    want.extend(typed("%2", "two"));
    want.extend(typed("%2", "three"));
    assert_eq!(h.keys(), want);
}

// ---------------------------------------------------------------------------
// Acceptance 3: not hearing the chief
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn speech_while_the_sink_plays_or_0_3_s_after_starts_no_turn() {
    let h = Harness::new();
    h.start().await;
    h.speak().await;
    h.sink().hold.store(true, Ordering::SeqCst);
    let said = h.say("answer", Speaker::Chief);
    settle().await;

    // While the answer plays.
    h.speak().await;
    assert_eq!(h.worker().transcribed.len(), 1);

    // 0.2 s after it ends.
    h.sink().release();
    said.await.unwrap();
    sleep(Duration::from_millis(200)).await;
    h.speak().await;
    assert_eq!(h.worker().transcribed.len(), 1);

    // Past the 0.3 s.
    past_the_tail().await;
    h.speak().await;
    assert_eq!(h.worker().transcribed.len(), 2);
}

/// A typed turn, then the chief's answer held in the sink and released: the instant the
/// answer ended, with the call listening again.
async fn after_an_answer(h: &Harness) -> Instant {
    h.start().await;
    h.speak().await;
    h.sink().hold.store(true, Ordering::SeqCst);
    let said = h.say("answer", Speaker::Chief);
    settle().await;
    h.sink().release();
    said.await.unwrap();
    settle().await;
    Instant::now()
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn speech_captured_during_a_sound_but_read_after_it_starts_no_turn() {
    let h = Harness::new();
    h.script([text("first"), text("second")]);
    let ended = after_an_answer(&h).await;

    sleep(Duration::from_secs(1)).await;
    // The speech was recorded from 0.5 s before the answer ended to 0.2 s after, all inside
    // the muted time; only the silence after it was recorded in the clear.
    let mut samples = loud(700);
    samples.extend(quiet(1200));
    h.feed_captured(samples, ended - Duration::from_millis(500));
    settle().await;

    assert_eq!(h.worker().transcribed.len(), 1);
    assert_eq!(h.keys(), typed("%2", "first"));
    assert_eq!(h.state(), CallState::Listening);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn speech_captured_after_the_tail_but_read_late_is_a_turn() {
    let h = Harness::new();
    h.script([text("first"), text("second")]);
    let ended = after_an_answer(&h).await;

    sleep(Duration::from_secs(2)).await;
    let mut samples = loud(1000);
    samples.extend(quiet(1200));
    h.feed_captured(samples, ended + Duration::from_millis(400));
    settle().await;

    let mut want = typed("%2", "first");
    want.extend(typed("%2", "second"));
    assert_eq!(h.keys(), want);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn the_sink_is_told_what_each_sound_says() {
    let h = Harness::new();
    h.script([text("hello there"), text("")]);
    h.start().await;
    h.speak().await;

    sleep(Duration::from_secs(3)).await;
    settle().await;
    h.answer("All green.").await;
    h.speak().await;

    let said = h.sink().said.lock().unwrap().clone();
    assert_eq!(
        said,
        [
            Said::Tick,
            Said::Line {
                text: "All green.".into(),
                by: Speaker::Chief
            },
            Said::Phrase(NOTHING_HEARD.into()),
        ]
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn the_ports_hear_why_each_call_ended() {
    let h = Harness::new();
    h.script([text("Bye.")]);
    h.start().await;
    h.speak().await;
    h.ports.fake_panes().set(|s| s.view.panes.clear());

    h.calls.start().await;
    settle().await;

    assert_eq!(h.ports.state().ended, ["bye", "no_chief"]);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn the_ports_choose_the_no_answer_phrase() {
    let h = Harness::new();
    h.ports.state().no_answer = Some(NO_ANSWER_IN_THREAD);
    h.start().await;
    h.speak().await;

    sleep(Duration::from_secs(95)).await;

    assert_eq!(h.played().last().unwrap(), NO_ANSWER_IN_THREAD);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn talk_now_stops_the_sink_and_listens_at_once() {
    let h = Harness::new();
    h.start().await;
    h.speak().await;
    h.sink().hold.store(true, Ordering::SeqCst);
    let said = h.say("a long answer", Speaker::Chief);
    settle().await;
    assert_eq!(h.state(), CallState::Speaking);

    // Another agent's update waits behind the answer.
    let update = h.say("update", Speaker::OtherAgent);
    settle().await;

    h.calls.talk_now();
    settle().await;

    assert_eq!(h.state(), CallState::Listening);
    assert_eq!(said.await.unwrap(), Some(true));
    assert_eq!(h.sink().stops.load(Ordering::SeqCst), 1);
    // The update waits until the user's next turn is cut.
    h.sink().hold.store(false, Ordering::SeqCst);
    sleep(Duration::from_secs(5)).await;
    assert_eq!(h.played(), ["a long answer"]);
    assert!(!update.is_finished());
    h.feed(loud(1000));
    settle().await;
    assert_eq!(h.played(), ["a long answer"]);
    h.feed(quiet(1200));
    settle().await;
    assert_eq!(update.await.unwrap(), Some(true));
    assert_eq!(h.played(), ["a long answer", "update"]);
    assert_eq!(h.worker().transcribed.len(), 2);
}

// ---------------------------------------------------------------------------
// Acceptance 4: the writer
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn the_turn_is_typed_as_voice_and_starts_a_voice_turn() {
    // A pane id no other test types into: the typed prompts are shared by every test.
    let pane = "%voice-a4";
    let h = Harness::new();
    h.ports.0.panes.set(|s| {
        s.view = AgentsView {
            listing_failed: false,
            panes: vec![chief_pane(pane, EffectiveAgentState::Idle)],
        }
    });
    h.start().await;

    h.speak().await;

    assert_eq!(h.keys(), typed(pane, "hello there"));
    let (_, started_by, cx) =
        telemetry::take_prompt(pane, time::OffsetDateTime::now_utc()).expect("a prompt");
    assert_eq!(started_by, StartedBy::Voice);
    assert!(cx.is_some(), "the turn joins the call turn's trace");
}

// ---------------------------------------------------------------------------
// Acceptance 5: the cases
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_busy_chief_hears_the_phrase_and_gets_the_turn_once_free() {
    let h = Harness::new();
    h.script([text("approve q19")]);
    h.start().await;
    h.set_chief(EffectiveAgentState::Busy);

    h.speak().await;

    assert!(h.keys().is_empty());
    assert_eq!(h.played(), [CHIEF_BUSY]);
    assert_eq!(h.state(), CallState::Listening);
    sleep(Duration::from_secs(30)).await;
    assert!(h.keys().is_empty());

    h.set_chief(EffectiveAgentState::Idle);
    sleep(Duration::from_millis(1100)).await;
    assert_eq!(h.keys(), typed("%2", "approve q19"));
    assert_eq!(h.status().last_result, Some(TurnResult::Typed));
    assert_eq!(h.state(), CallState::Waiting);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_chief_busy_past_2_minutes_drops_the_turn() {
    let logs = CapturedLogs::start();
    let h = Harness::new();
    h.start().await;
    h.set_chief(EffectiveAgentState::Busy);
    h.speak().await;

    sleep(Duration::from_secs(121)).await;
    h.set_chief(EffectiveAgentState::Idle);
    sleep(Duration::from_secs(5)).await;

    assert!(h.keys().is_empty());
    assert_eq!(h.status().last_result, Some(TurnResult::ChiefBusy));
    assert_eq!(h.played(), [CHIEF_BUSY]);
    let text = logs.text();
    assert!(
        text.contains("spoken prompt dropped reason=\"chief_busy\""),
        "{text}"
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_slow_chief_gets_a_tick_every_3_s_and_no_answer_yet_at_90_s() {
    let h = Harness::new();
    h.start().await;
    h.speak().await;
    let typed_at = Instant::now();
    assert_eq!(h.keys().len(), 2);

    sleep(Duration::from_secs(95)).await;

    let played = h.played_at();
    let ticks: Vec<Duration> = played
        .iter()
        .filter(|(_, what)| what == "tick")
        .map(|(at, _)| *at - typed_at)
        .collect();
    let want: Vec<Duration> = (1..30).map(|n| Duration::from_secs(3 * n)).collect();
    assert_eq!(ticks, want);
    let (at, last) = played.last().unwrap();
    assert_eq!(last, NO_ANSWER);
    assert_eq!(*at - typed_at, Duration::from_secs(90));
    assert_eq!(h.state(), CallState::Listening);
    past_the_tail().await;
    h.speak().await;
    assert_eq!(h.keys().len(), 4);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_turn_that_ends_without_harold_speak_gets_the_turn_end_update() {
    let h = Harness::new();
    h.start().await;
    h.speak().await;

    h.set_chief(EffectiveAgentState::Busy);
    sleep(Duration::from_millis(1000)).await;
    h.set_chief(EffectiveAgentState::Idle);
    sleep(Duration::from_millis(1900)).await;
    assert!(!h.played().contains(&UPDATE.to_string()));

    sleep(Duration::from_millis(700)).await;
    assert_eq!(h.played().last().map(String::as_str), Some(UPDATE));
    assert_eq!(h.ports.state().update_prompts, ["🎙 hello there"]);
    settle().await;
    assert_eq!(h.state(), CallState::Listening);
}

/// Makes the chief's pane one no other test uses, so its exported spans are this call's.
fn own_chief_pane(h: &Harness) -> String {
    let pane = format!("%call-turn-{}", uuid::Uuid::new_v4());
    let id = pane.clone();
    h.ports
        .0
        .panes
        .set(move |s| s.view.panes[0].pane_id = id.clone());
    pane
}

fn call_turns_on(pane: &str) -> Vec<opentelemetry_sdk::trace::SpanData> {
    test_support::spans()
        .into_iter()
        .filter(|span| {
            span.name == "harold.call_turn"
                && test_support::span_attr(span, "pane_id").as_deref() == Some(pane)
        })
        .collect()
}

#[test]
fn typing_a_spoken_prompt_records_it_with_a_trace_context() {
    test_support::install();
    let pane = format!("%call-turn-{}", uuid::Uuid::new_v4());
    let turn = call::log_typed(&pane, "hello");
    let (_, started_by, cx) =
        telemetry::take_prompt(&pane, time::OffsetDateTime::now_utc()).expect("prompt kept");
    assert_eq!(started_by, StartedBy::Voice);
    let cx = cx.expect("a spoken prompt joins a trace");
    use opentelemetry::trace::TraceContextExt as _;
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;
    assert_eq!(
        cx.span().span_context().trace_id(),
        turn.context().span().span_context().trace_id()
    );
    drop(turn);
    let spans = call_turns_on(&pane);
    assert_eq!(spans.len(), 1);
    assert_eq!(
        spans[0].parent_span_id,
        opentelemetry::trace::SpanId::INVALID
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn the_update_is_asked_with_the_turns_span_and_the_span_ends_when_the_line_arrives() {
    test_support::install();
    let h = Harness::new();
    let pane = own_chief_pane(&h);
    h.start().await;
    h.speak().await;
    assert!(call_turns_on(&pane).is_empty(), "the turn is still open");

    h.set_chief(EffectiveAgentState::Busy);
    sleep(Duration::from_millis(1000)).await;
    h.set_chief(EffectiveAgentState::Idle);
    sleep(Duration::from_millis(2600)).await;
    settle().await;

    let spans = call_turns_on(&pane);
    assert_eq!(spans.len(), 1);
    assert_eq!(
        h.ports.state().update_traces,
        [spans[0].span_context.trace_id()]
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn the_span_ends_when_the_wait_gives_up() {
    test_support::install();
    let h = Harness::new();
    let pane = own_chief_pane(&h);
    h.start().await;
    h.speak().await;
    assert!(call_turns_on(&pane).is_empty());

    sleep(Duration::from_secs(95)).await;
    settle().await;
    assert_eq!(call_turns_on(&pane).len(), 1);
    assert!(h.ports.state().update_traces.is_empty());
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn nothing_heard_plays_sorry_and_listens_again() {
    let logs = CapturedLogs::start();
    let h = Harness::new();
    h.script([text(""), Heard::Unlikely("hello".to_string())]);
    h.start().await;

    h.speak().await;
    assert_eq!(h.played(), [NOTHING_HEARD]);
    assert_eq!(h.status().last_result, Some(TurnResult::NothingHeard));
    past_the_tail().await;
    h.speak().await;

    assert_eq!(h.played(), [NOTHING_HEARD, NOTHING_HEARD]);
    assert!(h.keys().is_empty());
    assert_eq!(h.status().last_result, Some(TurnResult::NothingHeard));
    assert_eq!(h.state(), CallState::Listening);
    assert!(
        logs.text()
            .contains("spoken prompt dropped reason=\"nothing_heard\"")
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn start_with_no_chief_says_so_and_starts_nothing() {
    let logs = CapturedLogs::start();
    let h = Harness::new();
    h.ports.0.panes.set(|s| s.view.panes.clear());

    h.calls.start().await;
    settle().await;

    assert_eq!(h.tts(), [NO_CHIEF]);
    let status = h.status();
    assert_eq!(status.state, CallState::Off);
    assert_eq!(status.last_result, Some(TurnResult::NoChief));
    assert_eq!(status.chief_pane_id, None);
    assert_eq!(h.ports.state().sources, 0);
    assert_eq!(h.worker().starts, 0);
    assert!(h.call_dirs().is_empty());
    assert!(logs.text().contains("call ended reason=\"no_chief\""));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_chief_gone_mid_call_ends_the_call() {
    let h = Harness::new();
    h.start().await;
    h.ports.0.panes.set(|s| s.view.panes.clear());

    h.speak().await;

    assert!(h.keys().is_empty());
    assert_eq!(h.played(), [NO_CHIEF]);
    assert_eq!(h.status().last_result, Some(TurnResult::NoChief));
    assert_eq!(h.state(), CallState::Off);
}

// ---------------------------------------------------------------------------
// Decisions 16 and 17: the display and the muted voice
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn start_while_the_voice_is_muted_starts_nothing() {
    let logs = CapturedLogs::start();
    let h = Harness::new();
    h.ports.state().muted = true;

    let status = h.calls.start().await;

    assert_eq!(status.state, CallState::Off);
    assert_eq!(status.last_result, Some(TurnResult::VoiceMuted));
    assert_eq!(h.ports.state().sources, 0);
    assert_eq!(h.worker().starts, 0);
    assert_eq!(h.ports.state().awake_taken, 0);
    assert!(
        logs.text()
            .contains("call not started reason=\"voice_muted\"")
    );

    h.ports.state().muted = false;
    assert_eq!(h.start().await.state, CallState::Listening);
    assert_eq!(h.worker().starts, 1);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn the_display_is_kept_awake_for_the_call_and_released_at_its_end() {
    let logs = CapturedLogs::start();
    let h = Harness::new();

    // Ended by stop().
    h.start().await;
    assert_eq!(h.awake_held(), 1);
    h.calls.stop().await;
    assert_eq!(h.awake_held(), 0);

    // Ended by "bye".
    h.script([text("Bye.")]);
    h.start().await;
    assert_eq!(h.awake_held(), 1);
    h.speak().await;
    assert_eq!(h.state(), CallState::Off);
    assert_eq!(h.awake_held(), 0);

    // Ended by the source.
    h.start().await;
    assert_eq!(h.awake_held(), 1);
    h.end_source();
    settle().await;
    assert_eq!(h.state(), CallState::Off);
    assert_eq!(h.awake_held(), 0);
    assert_eq!(h.ports.state().awake_taken, 3);

    // Not kept awake: the call goes on.
    h.ports.state().fail_awake = true;
    assert_eq!(h.start().await.state, CallState::Listening);
    h.speak().await;
    assert_eq!(h.keys().len(), 2);
    assert!(logs.text().contains("display not kept awake"));
}

// ---------------------------------------------------------------------------
// Acceptance 6: hanging up by voice
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_bye_alone_types_nothing_and_ends_the_call() {
    let logs = CapturedLogs::start();
    let h = Harness::new();
    h.script([text("Bye.")]);
    h.start().await;

    h.speak().await;

    assert!(h.keys().is_empty());
    assert_eq!(h.played(), [BYE]);
    assert_eq!(h.state(), CallState::Off);
    assert!(h.call_dirs().is_empty());
    assert!(logs.text().contains("call ended reason=\"bye\""));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_turn_then_bye_types_the_turn_and_ends() {
    let h = Harness::new();
    h.script([text("approve q19, bye")]);
    h.start().await;

    h.speak().await;

    assert_eq!(h.keys(), typed("%2", "approve q19"));
    assert_eq!(h.played(), [BYE]);
    assert_eq!(h.state(), CallState::Off);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn bye_inside_a_sentence_is_typed_whole() {
    let h = Harness::new();
    h.script([text("say bye to the old store first")]);
    h.start().await;

    h.speak().await;

    assert_eq!(h.keys(), typed("%2", "say bye to the old store first"));
    assert!(h.played().is_empty());
    assert_eq!(h.state(), CallState::Waiting);
}

// ---------------------------------------------------------------------------
// Acceptance 8: failure
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_failed_transcription_drops_the_turn_and_keeps_listening() {
    let logs = CapturedLogs::start();
    let h = Harness::new();
    h.script([Heard::Fail]);
    h.start().await;

    h.speak().await;

    assert!(h.keys().is_empty());
    assert!(h.turn_dirs().is_empty());
    assert_eq!(h.status().last_result, Some(TurnResult::Failed));
    assert_eq!(h.state(), CallState::Listening);
    assert_eq!(h.worker().starts, 1);
    assert!(
        logs.text()
            .contains("spoken prompt dropped reason=\"failed\"")
    );
    h.speak().await;
    assert_eq!(h.keys().len(), 2);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_worker_that_does_not_answer_in_30_s_is_restarted() {
    let logs = CapturedLogs::start();
    let h = Harness::new();
    h.script([Heard::TimeOut]);
    h.start().await;

    h.speak().await;
    assert_eq!(h.state(), CallState::Transcribing);
    sleep(REQUEST_LIMIT + Duration::from_millis(100)).await;

    assert!(h.turn_dirs().is_empty());
    assert_eq!(h.status().last_result, Some(TurnResult::Failed));
    assert_eq!(h.worker().starts, 2);
    assert_eq!(h.tts(), [VOICE_STOPPED]);
    assert_eq!(h.state(), CallState::Listening);
    assert!(
        logs.text()
            .contains("voice worker exited reason=\"timed_out\"")
    );
    past_the_tail().await;
    h.speak().await;
    assert_eq!(h.keys().len(), 2);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_worker_that_exits_is_started_once_more() {
    let logs = CapturedLogs::start();
    let h = Harness::new();
    h.script([Heard::Exit]);
    h.start().await;

    h.speak().await;

    assert_eq!(h.worker().starts, 2);
    assert_eq!(h.tts(), [VOICE_STOPPED]);
    assert_eq!(h.status().last_result, Some(TurnResult::Failed));
    assert_eq!(h.state(), CallState::Listening);
    assert_eq!(
        logs.text()
            .matches("voice worker exited reason=\"exited\"")
            .count(),
        1
    );
    past_the_tail().await;
    h.speak().await;
    assert_eq!(h.keys(), typed("%2", "hello there"));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_second_exit_ends_the_call() {
    let logs = CapturedLogs::start();
    let h = Harness::new();
    h.script([Heard::Exit, Heard::Exit]);
    h.start().await;

    h.speak().await;
    past_the_tail().await;
    h.speak().await;

    assert_eq!(h.worker().starts, 2);
    assert_eq!(h.tts(), [VOICE_STOPPED, VOICE_FAILED]);
    assert_eq!(h.state(), CallState::Off);
    assert!(h.call_dirs().is_empty());
    let text = logs.text();
    assert_eq!(
        text.matches("voice worker exited reason=\"exited\"")
            .count(),
        2,
        "{text}"
    );
    assert!(
        text.contains("call ended reason=\"voice_failed\""),
        "{text}"
    );
    assert_eq!(h.awake_held(), 0);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_worker_that_fails_to_load_ends_the_call_at_once() {
    let logs = CapturedLogs::start();
    let h = Harness::new();
    h.worker().starts_loading = true;
    h.start().await;

    h.set_health(Health::Gone(WorkerError::FailedToLoad));
    settle().await;

    assert_eq!(h.worker().starts, 1);
    assert_eq!(h.tts(), [VOICE_FAILED]);
    assert_eq!(h.state(), CallState::Off);
    assert_eq!(h.awake_held(), 0);
    assert!(h.call_dirs().is_empty());
    let text = logs.text();
    assert!(
        text.contains("voice worker exited reason=\"failed_to_load\""),
        "{text}"
    );
    assert!(
        text.contains("call ended reason=\"voice_failed\""),
        "{text}"
    );
}

// ---------------------------------------------------------------------------
// Acceptance 9: loaded once
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn five_turns_start_the_worker_once() {
    let h = Harness::new();
    h.start().await;

    for n in 1..=5 {
        h.speak().await;
        h.answer(&format!("answer {n}")).await;
    }
    assert_eq!(h.keys().len(), 10);
    h.calls.stop().await;

    let worker = h.worker();
    assert_eq!(worker.starts, 1);
    assert_eq!(worker.transcribed.len(), 5);
    assert_eq!(
        worker.said,
        (1..=5).map(|n| format!("answer {n}")).collect::<Vec<_>>()
    );
    assert_eq!(worker.closes, 1);
    drop(worker);
    assert!(h.call_dirs().is_empty());
    assert_eq!(h.state(), CallState::Off);
}

// ---------------------------------------------------------------------------
// Acceptance 10: no text in logs
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn the_transcript_reaches_no_log_and_no_export() {
    let logs = CapturedLogs::start();
    test_support::install();
    let h = Harness::new();
    h.script([text("VOICE-CANARY-4412")]);
    h.start().await;

    h.speak().await;
    h.answer("VOICE-CANARY-4412 heard").await;
    h.calls.stop().await;

    assert_eq!(h.keys(), typed("%2", "VOICE-CANARY-4412"));
    let text = logs.text();
    assert!(!text.contains("VOICE-CANARY-4412"), "{text}");
    assert!(
        text.contains("spoken prompt typed pane_id=%2 words=1"),
        "{text}"
    );
    assert!(!test_support::exported_text().contains("VOICE-CANARY-4412"));
    let exported: Vec<_> = test_support::logs()
        .into_iter()
        .filter(|log| format!("{:?}", log.record.body()).contains("spoken prompt typed"))
        .collect();
    assert!(!exported.is_empty());
    for log in exported {
        let keys: Vec<String> = log
            .record
            .attributes_iter()
            .map(|(key, _)| key.as_str().to_string())
            .collect();
        assert!(keys.contains(&"pane_id".to_string()), "{keys:?}");
        assert!(!keys.contains(&"words".to_string()), "{keys:?}");
    }
}

// ---------------------------------------------------------------------------
// Review focus
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_source_that_ends_ends_the_call() {
    let logs = CapturedLogs::start();
    let h = Harness::new();
    h.ports.state().source_ends = true;

    h.calls.start().await;
    settle().await;

    assert_eq!(h.tts(), [VOICE_FAILED]);
    assert_eq!(h.state(), CallState::Off);
    assert!(h.call_dirs().is_empty());
    assert!(
        logs.text()
            .contains("call ended reason=\"microphone_failed\"")
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_line_waits_while_the_user_is_mid_turn() {
    let h = Harness::new();
    h.start().await;

    h.feed(loud(1000));
    settle().await;
    let said = h.say("update", Speaker::OtherAgent);
    settle().await;
    h.feed(loud(1000));
    settle().await;
    assert!(!said.is_finished());
    assert!(h.played().is_empty());

    h.feed(quiet(1200));
    settle().await;

    assert_eq!(said.await.unwrap(), Some(true));
    assert_eq!(h.played(), ["update"]);
    assert_eq!(h.worker().transcribed, [2 * SAMPLE_RATE as usize]);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_transcript_with_line_breaks_is_typed_as_one_line() {
    let h = Harness::new();
    h.script([text("a\nb\u{7}")]);
    h.start().await;

    h.speak().await;

    assert_eq!(h.keys(), typed("%2", "a b"));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_second_turn_while_the_first_waits_is_joined() {
    let h = Harness::new();
    h.script([text("first"), text("second")]);
    h.start().await;
    h.set_chief(EffectiveAgentState::Busy);

    h.speak().await;
    past_the_tail().await;
    h.speak().await;
    assert!(h.keys().is_empty());

    h.set_chief(EffectiveAgentState::Idle);
    sleep(Duration::from_millis(1100)).await;

    assert_eq!(h.keys(), typed("%2", "first second"));
    assert_eq!(h.played(), [CHIEF_BUSY]);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn stop_passes_on_a_turn_held_for_a_busy_chief() {
    let logs = CapturedLogs::start();
    let h = Harness::new();
    h.script([text("approve q19")]);
    h.start().await;
    h.set_chief(EffectiveAgentState::Busy);
    h.speak().await;
    assert_eq!(h.played(), [CHIEF_BUSY]);

    assert_eq!(h.calls.stop().await.state, CallState::Off);
    assert!(h.call_dirs().is_empty());
    sleep(Duration::from_secs(60)).await;
    assert!(h.keys().is_empty());
    h.set_chief(EffectiveAgentState::Idle);
    sleep(Duration::from_secs(10)).await;

    assert_eq!(h.keys(), typed("%2", "approve q19"));
    let text = logs.text();
    assert!(
        text.contains("spoken prompt typed pane_id=%2 words=2"),
        "{text}"
    );
    assert!(!text.contains("spoken prompt dropped"), "{text}");
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_bye_passes_on_a_turn_held_for_a_busy_chief() {
    let h = Harness::new();
    h.script([text("approve q19"), text("Bye.")]);
    h.start().await;
    h.set_chief(EffectiveAgentState::Busy);
    h.speak().await;
    past_the_tail().await;

    h.speak().await;

    assert_eq!(h.played(), [CHIEF_BUSY, BYE]);
    assert_eq!(h.state(), CallState::Off);
    assert!(h.keys().is_empty());
    h.set_chief(EffectiveAgentState::Idle);
    sleep(Duration::from_secs(10)).await;
    assert_eq!(h.keys(), typed("%2", "approve q19"));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_held_turn_passed_on_is_dropped_2_minutes_after_it_was_first_held() {
    let logs = CapturedLogs::start();
    let h = Harness::new();
    h.start().await;
    h.set_chief(EffectiveAgentState::Busy);
    h.speak().await;
    sleep(Duration::from_secs(60)).await;
    h.calls.stop().await;

    // Two minutes from the hold, not from the stop.
    sleep(Duration::from_secs(65)).await;
    h.set_chief(EffectiveAgentState::Idle);
    sleep(Duration::from_secs(10)).await;

    assert!(h.keys().is_empty());
    let text = logs.text();
    assert_eq!(text.matches("spoken prompt dropped").count(), 1, "{text}");
    assert!(
        text.contains("spoken prompt dropped reason=\"chief_busy\""),
        "{text}"
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn shutdown_drops_a_turn_held_for_a_busy_chief() {
    let logs = CapturedLogs::start();
    let h = Harness::new();
    h.start().await;
    h.set_chief(EffectiveAgentState::Busy);
    h.speak().await;

    h.calls.shutdown().await;
    h.set_chief(EffectiveAgentState::Idle);
    sleep(Duration::from_secs(10)).await;

    assert!(h.keys().is_empty());
    assert_eq!(h.status().last_result, Some(TurnResult::ChiefBusy));
    let text = logs.text();
    assert!(
        text.contains("spoken prompt dropped reason=\"chief_busy\""),
        "{text}"
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_phrase_is_made_once_and_kept_for_later_calls() {
    let h = Harness::new();
    h.script([text(""), text("")]);

    for _ in 0..2 {
        h.start().await;
        h.speak().await;
        h.calls.stop().await;
    }

    assert_eq!(h.played(), [NOTHING_HEARD, NOTHING_HEARD]);
    let made = h.worker().said.clone();
    assert_eq!(made, [NOTHING_HEARD]);
    assert_eq!(h.worker().starts, 2);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn lines_after_talk_now_are_not_played_until_the_next_turn() {
    let h = Harness::new();
    h.start().await;
    h.speak().await;
    h.sink().hold.store(true, Ordering::SeqCst);
    let said = h.say("first part", Speaker::Chief);
    settle().await;
    h.calls.talk_now();
    said.await.unwrap();
    h.sink().hold.store(false, Ordering::SeqCst);

    assert_eq!(
        h.say("second part", Speaker::Chief).await.unwrap(),
        Some(true)
    );
    settle().await;
    assert_eq!(h.played(), ["first part"]);
    assert_eq!(h.worker().said, ["first part"]);

    past_the_tail().await;
    h.speak().await;
    assert_eq!(h.keys().len(), 4);
    h.answer("next answer").await;
    assert_eq!(h.played(), ["first part", "next answer"]);
}

// ---------------------------------------------------------------------------
// Lines with no call
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_line_with_no_call_open_is_not_taken() {
    let h = Harness::new();
    assert_eq!(h.calls.say("hello", Speaker::Chief).await, None);
    assert_eq!(h.state(), CallState::Off);
    let calls = Arc::clone(&h.calls);
    let blocking = tokio::task::spawn_blocking(move || calls.say_blocking("hi", Speaker::Chief));
    assert_eq!(blocking.await.unwrap(), None);
}

/// A call that answers a fixed result, or `None` for no call open, and counts the lines it got.
struct FakeLineTaker {
    answer: Option<bool>,
    taken: AtomicUsize,
}

impl FakeLineTaker {
    fn new(answer: Option<bool>) -> Self {
        Self {
            answer,
            taken: AtomicUsize::new(0),
        }
    }

    fn taken(&self) -> usize {
        self.taken.load(Ordering::SeqCst)
    }
}

impl LineTaker for FakeLineTaker {
    fn say_blocking(&self, _line: &str, _by: Speaker) -> Option<bool> {
        self.taken.fetch_add(1, Ordering::SeqCst);
        self.answer
    }
}

#[test]
fn a_line_goes_to_the_first_open_call() {
    let closed = FakeLineTaker::new(None);
    let open = FakeLineTaker::new(Some(true));
    let ended_first = FakeLineTaker::new(Some(false));

    assert_eq!(say_in_first_open(&[], "hi", Speaker::Chief), None);

    assert_eq!(
        say_in_first_open(&[&closed, &open], "hi", Speaker::Chief),
        Some(true)
    );
    assert_eq!((closed.taken(), open.taken()), (1, 1));

    let open = FakeLineTaker::new(Some(true));
    assert_eq!(
        say_in_first_open(&[&ended_first, &open], "hi", Speaker::Chief),
        Some(false)
    );
    assert_eq!(open.taken(), 0);
}

/// The projector asks only the phone's calls for the chief's line: an open desk call is
/// never asked, so the line then goes by the screen lock's rules, as before the phone's calls.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn the_chiefs_line_for_the_phone_never_reaches_an_open_desk_call() {
    let desk = Harness::new();
    desk.start().await;
    assert_eq!(desk.state(), CallState::Listening);
    let no_phone_call = FakeLineTaker::new(None);

    assert_eq!(say_in_phone_call_over(None, "hi", Speaker::Chief), None);
    assert_eq!(
        say_in_phone_call_over(Some(&no_phone_call), "hi", Speaker::Chief),
        None
    );
    settle().await;

    assert_eq!(no_phone_call.taken(), 1);
    assert!(desk.played().is_empty());
    assert_eq!(desk.state(), CallState::Listening);
    let phone_call = FakeLineTaker::new(Some(true));
    assert_eq!(
        say_in_phone_call_over(Some(&phone_call), "hi", Speaker::Chief),
        Some(true)
    );
    assert_eq!(phone_call.taken(), 1);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_line_said_into_a_call_that_ends_first_is_not_played() {
    let h = Harness::new();
    h.start().await;
    h.feed(loud(1000));
    settle().await;
    let said = h.say("update", Speaker::OtherAgent);
    settle().await;

    h.calls.stop().await;

    assert_eq!(said.await.unwrap(), Some(false));
    assert!(h.played().is_empty());
    assert_eq!(h.calls.say("later", Speaker::Chief).await, None);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn shutdown_ends_the_call_with_reason_shutdown() {
    let logs = CapturedLogs::start();
    let h = Harness::new();
    h.start().await;

    h.calls.shutdown().await;

    assert_eq!(h.state(), CallState::Off);
    assert_eq!(h.worker().closes, 1);
    assert!(h.call_dirs().is_empty());
    assert!(logs.text().contains("call ended reason=\"shutdown\""));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_call_reads_as_speaking_while_it_says_why_it_ended() {
    let h = Harness::new();
    h.script([Heard::FailToLoad]);
    h.start().await;
    h.ports.state().hold_tts = true;

    h.speak().await;

    assert_eq!(h.tts(), [VOICE_FAILED]);
    assert_eq!(h.state(), CallState::Speaking);
    h.ports.0.tts_release.notify_one();
    settle().await;
    assert_eq!(h.state(), CallState::Off);
}

// ---------------------------------------------------------------------------
// The daemon's control
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_control_that_is_not_set_up_starts_nothing() {
    let ports = FakePorts::new();
    let calls = CallControl::new(ports.clone(), false);

    let status = calls.start().await;
    settle().await;

    assert_eq!(status.state, CallState::NotSetUp);
    assert_eq!(calls.status().state, CallState::NotSetUp);
    assert_eq!(ports.state().sources, 0);
    assert_eq!(ports.0.worker.lock().unwrap().starts, 0);
    assert_eq!(ports.state().awake_taken, 0);
    assert!(!ports.calls_dir().exists());
    assert_eq!(calls.say("hello", Speaker::Chief).await, None);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn start_during_a_call_starts_nothing_new() {
    let h = Harness::new();
    assert_eq!(h.start().await.state, CallState::Listening);

    assert_eq!(h.calls.start().await.state, CallState::Listening);
    settle().await;

    assert_eq!(h.ports.state().sources, 1);
    assert_eq!(h.worker().starts, 1);
    assert_eq!(h.call_dirs().len(), 1);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn stop_with_no_call_answers_off() {
    let h = Harness::new();

    assert_eq!(h.calls.stop().await.state, CallState::Off);
    assert_eq!(h.ports.state().sources, 0);
    assert_eq!(h.worker().starts, 0);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn shutdown_ends_the_call_and_cleans_up() {
    let logs = CapturedLogs::start();
    let h = Harness::new();
    h.start().await;
    assert_eq!(h.call_dirs().len(), 1);

    h.calls.shutdown().await;
    settle().await;

    assert_eq!(h.state(), CallState::Off);
    assert_eq!(h.worker().closes, 1);
    let mic = h
        .ports
        .state()
        .mic
        .clone()
        .expect("the test's end of the microphone");
    assert!(mic.is_closed(), "the source was dropped");
    assert_eq!(h.awake_held(), 0);
    assert!(h.call_dirs().is_empty());
    assert!(logs.text().contains("call ended reason=\"shutdown\""));
    assert_eq!(h.calls.say("later", Speaker::Chief).await, None);
}

// The worker's health, sounds that cannot play, the turn before a bye, and a second START.
#[path = "voice_input_call_tests.rs"]
mod call_tests;

#[test]
fn two_agents_in_the_chiefs_folder_leave_the_first_as_the_chief() {
    let h = Harness::new();
    let logs = crate::outbound::fake::CapturedLogs::start();
    assert_eq!(super::call::chief_pane(&h.ports).as_deref(), Some("%2"));
    h.ports.0.panes.set(|s| {
        let mut second = s.view.panes[0].clone();
        second.pane_id = "%9".into();
        second.pane_index += 1;
        s.view.panes.push(second);
    });

    assert_eq!(super::call::chief_pane(&h.ports).as_deref(), Some("%2"));
    // The chief did not move, so only the first lookup was logged.
    let text = logs.text();
    assert_eq!(text.matches("chief pane changed").count(), 1, "{text}");
    assert!(text.contains("state=\"found\""), "{text}");
}
