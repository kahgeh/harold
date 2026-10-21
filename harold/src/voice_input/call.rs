//! One call with the chief at the desk. A call is one task that owns the worker, the
//! detector and the state, and takes its events from one channel: sample blocks, control
//! commands, lines to say, timers, and the ends of the work it spawned (a request to the
//! worker, a sound, a turn-end update). `CallControl` starts it, talks to it and reads its
//! status.

use super::hang_up::{Spoken, hang_up};
use super::phrases::{
    BYE, CHIEF_BUSY, NO_CHIEF, NOTHING_HEARD, VOICE_FAILED, VOICE_STOPPED, phrase_file,
};
use super::speech::SpeechDetector;
use super::wav::{tick_wav, wav_bytes};
use super::worker::{Health, Transcript, VoiceWorker, WorkerError};
use super::{
    AudioSink, AudioSource, CallPorts, CallState, CallStatus, SAMPLE_RATE, Said, Speaker,
    TurnResult,
};
use crate::agent::domain::EffectiveAgentState;
use crate::away_message::one_line;
use crate::outbound::left_out::{ChiefLookup, resolve_chief, same_folder};
use crate::telemetry::{self, StartedBy};
use crate::timed_prompts::{
    AgentPane, AgentsView, Attempt, Ports, Reason, Writer, type_when_ready,
};
use std::collections::VecDeque;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until};
use tracing::{info, warn};
use tracing_opentelemetry::OpenTelemetrySpanExt as _;
use uuid::Uuid;

/// How long after a sound ends the microphone is still not heard.
const MUTE_TAIL: Duration = Duration::from_millis(300);
/// The wait for an answer before the first tick, and between ticks.
const TICK_EVERY: Duration = Duration::from_secs(3);
/// How long the chief has to answer before Harold says `No answer yet.`.
const ANSWER_LIMIT: Duration = Duration::from_secs(90);
/// How often a turn for a busy chief is tried again (Decision 9).
const BUSY_RETRY: Duration = Duration::from_secs(1);
/// How long a turn for a busy chief is kept.
const BUSY_LIMIT: Duration = Duration::from_secs(120);
/// How often the chief's pane is read while an answer is awaited (Decision 11).
const PANE_READ_EVERY: Duration = Duration::from_millis(500);
/// How long after the chief's turn ends its line may still arrive.
const LINE_GRACE: Duration = Duration::from_secs(2);
/// How long a call goes with no turn cut and no sound played before it ends by itself.
const IDLE_LIMIT: Duration = Duration::from_secs(10 * 60);
/// How long, after talk now, other agents' lines and the restart phrase wait for a turn.
const HOLD_LIMIT: Duration = Duration::from_secs(10);
/// Whisper's no-speech probability above which a turn counts as nothing heard.
const NO_SPEECH_LIMIT: f32 = 0.6;
/// The mark in front of each turn typed into the chief's pane.
const MARK: &str = "🎙";
const TICK_WAV: &str = "tick.wav";
const TURN_WAV: &str = "turn.wav";

/// Why a call ended: the `reason` of `call ended` (Decision 15).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Ended {
    Stopped,
    Bye,
    NoChief,
    VoiceFailed,
    MicrophoneFailed,
    Shutdown,
    /// No turn was cut and no sound played for `IDLE_LIMIT`.
    Idle,
    /// Harold's voice was muted during the call.
    VoiceMuted,
}

impl Ended {
    fn as_str(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Bye => "bye",
            Self::NoChief => "no_chief",
            Self::VoiceFailed => "voice_failed",
            Self::MicrophoneFailed => "microphone_failed",
            Self::Shutdown => "shutdown",
            Self::Idle => "idle",
            Self::VoiceMuted => "voice_muted",
        }
    }
}

/// What a call's task hears about.
pub(super) enum Event<W> {
    /// Samples, and when they were recorded.
    Block {
        samples: Vec<i16>,
        captured_at: Instant,
    },
    SourceEnded,
    Stop(Ended),
    TalkNow,
    /// Forget the turn being spoken: a newer phone screen has taken the call over.
    ForgetTurn,
    Say {
        line: String,
        by: Speaker,
        reply: oneshot::Sender<bool>,
    },
    /// A transcribe request ended; the worker of `generation` comes back with its answer.
    Transcribed {
        generation: u64,
        dir: PathBuf,
        result: Result<Transcript, WorkerError>,
        worker: W,
    },
    /// A say request for the sound `id` ended.
    Made {
        generation: u64,
        id: u64,
        wav: PathBuf,
        result: Result<(), WorkerError>,
        worker: W,
    },
    /// The sound `id` ended; `played` is false when the sink could not play it.
    Played {
        id: u64,
        played: bool,
    },
    TurnEndUpdate(String),
}

pub(super) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The chief's pane: the first agent pane of the chief's tmux session in the chief's
/// folder.
pub(super) fn chief_pane<P: CallPorts>(ports: &P) -> Option<String> {
    let folder = ports.chief_folder()?;
    let view = ports.panes().agents();
    let session = ports.chief_session();
    let found = resolve_chief(
        view.panes.iter().map(AgentPane::position),
        Some(&folder),
        session.as_deref(),
    );
    match found {
        ChiefLookup::Found(pane_id) => Some(pane_id),
        ChiefLookup::Ambiguous { .. } | ChiefLookup::None => None,
    }
}

/// Whether an agent runs in `folder`. A pane list that failed to read counts as yes, as it
/// counts as busy, not as no chief, when a turn is typed (Decision 10).
fn chief_running(view: &AgentsView, folder: &str) -> bool {
    view.listing_failed
        || view
            .panes
            .iter()
            .any(|pane| same_folder(&pane.folder, folder))
}

/// One try at typing `text` into the chief's pane, as the timed prompts type (Decision 10).
async fn try_type<P: CallPorts>(ports: &P, text: &str) -> Typing {
    let Some(folder) = ports.chief_folder() else {
        return Typing::NoChief;
    };
    let panes = ports.panes();
    let view = panes.agents();
    let line = format!("{MARK} {text}");
    match type_when_ready(panes, Writer::Voice, &folder, &line, &view, &[]).await {
        Attempt::Typed { pane } => Typing::Typed(pane),
        Attempt::Wait(Reason::NoAgentInFolder) => Typing::NoChief,
        Attempt::Wait(_) => Typing::Busy,
        Attempt::EnterFailed { .. } => Typing::Failed,
    }
}

/// Logs a typed turn and opens its `harold.call_turn` span, the root of the turn's trace.
/// The stop hook's `harold.turn` joins it. The span ends when its last holder lets go.
pub(super) fn log_typed(pane: &str, text: &str) -> tracing::Span {
    let words = text.split_whitespace().count();
    let turn = tracing::info_span!(
        target: telemetry::TRACE_TARGET,
        parent: None,
        "harold.call_turn",
        pane_id = %pane
    );
    // Fails only when no export layer is installed, and then there is no trace to join.
    let cx = turn.context();
    turn.in_scope(|| info!(pane_id = %pane, words, "spoken prompt typed"));
    telemetry::prompt_typed(
        pane,
        StartedBy::Voice,
        Some(cx),
        time::OffsetDateTime::now_utc(),
    );
    turn
}

fn log_dropped(result: TurnResult) {
    info!(reason = result.reason(), "spoken prompt dropped");
}

/// Types a turn into a busy chief's pane once it is free, after the call has ended: tried
/// first at `next_try` and then every second until `gives_up_at`, as during a call
/// (Decision 9).
async fn pass_on_when_free<P: CallPorts>(
    ports: Arc<P>,
    text: String,
    mut next_try: Instant,
    gives_up_at: Instant,
) {
    loop {
        sleep_until(next_try).await;
        let result = match try_type(&*ports, &text).await {
            Typing::Typed(pane) => {
                // The call is over: nothing waits on the turn, so its span ends here.
                drop(log_typed(&pane, &text));
                return;
            }
            Typing::Busy if Instant::now() < gives_up_at => {
                next_try = Instant::now() + BUSY_RETRY;
                continue;
            }
            Typing::Busy => TurnResult::ChiefBusy,
            Typing::NoChief => TurnResult::NoChief,
            Typing::Failed => TurnResult::Failed,
        };
        log_dropped(result);
        return;
    }
}

fn private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::DirBuilder::new().mode(0o700).create(dir)
}

/// Makes `<calls>/call-<uuid>/` for the user only, and writes the tick into it.
fn make_call_dir(calls: &Path, dir: &Path) -> std::io::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(calls)?;
    private_dir(dir)?;
    std::fs::write(dir.join(TICK_WAV), tick_wav())
}

/// Removes the `call-*` folders under `calls` that a Harold which did not end its calls
/// left behind. Nothing else in `calls` is touched.
pub(super) fn sweep_call_folders(calls: &Path) {
    let Ok(entries) = std::fs::read_dir(calls) else {
        return;
    };
    for entry in entries.flatten() {
        let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
        if !is_dir || !entry.file_name().to_string_lossy().starts_with("call-") {
            continue;
        }
        if let Err(error) = std::fs::remove_dir_all(entry.path()) {
            warn!(error = %error, "call folder not removed");
        }
    }
}

// ---------------------------------------------------------------------------
// The call's task
// ---------------------------------------------------------------------------

pub(super) async fn run_call<P: CallPorts>(
    ports: Arc<P>,
    status: Arc<Mutex<CallStatus>>,
    events: mpsc::UnboundedSender<Event<P::Worker>>,
    mut inbox: mpsc::UnboundedReceiver<Event<P::Worker>>,
    opened: watch::Sender<bool>,
) {
    let Some(mut call) = Call::open(ports, status, events, opened).await else {
        return;
    };
    call.run(&mut inbox).await;
}

/// What the call does with the samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Listening,
    Transcribing,
    /// A turn was typed; the chief's answer is awaited.
    Waiting,
    /// The call says its last phrase and ends.
    Ending,
}

/// One piece of work for the worker.
enum Job {
    Transcribe { dir: PathBuf },
    Make { id: u64, text: String, wav: PathBuf },
}

/// Where a sound's audio comes from.
enum Audio {
    /// A file, which the worker is still making while `ready` is false. A phrase made in the
    /// call's folder moves to `keep_as` in the phrase folder once made.
    Wav {
        path: PathBuf,
        ready: bool,
        keep_as: Option<PathBuf>,
        remove: bool,
    },
    /// The line said through the `[tts]` command.
    Tts,
}

/// What happens once a sound has played.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum After {
    Nothing,
    End(Ended),
}

/// A line or a phrase to play, in turn.
struct Sound {
    id: u64,
    text: String,
    audio: Audio,
    /// Who sent the line; `None` for Harold's own phrases.
    by: Option<Speaker>,
    /// Whether this is the chief's answer to the turn typed last.
    answers: bool,
    /// Whether it waits, after talk now, until the next turn is cut: another agent's line
    /// or the restart phrase, but not the chief's lines or Harold's reply to a turn.
    waits_after_talk_now: bool,
    after: After,
    reply: Option<oneshot::Sender<bool>>,
}

impl Sound {
    fn ready(&self) -> bool {
        match self.audio {
            Audio::Wav { ready, .. } => ready,
            Audio::Tts => true,
        }
    }

    /// What it says, as the sink is told.
    fn said(&self) -> Said {
        match self.by {
            Some(by) => Said::Line {
                text: self.text.clone(),
                by,
            },
            None => Said::Phrase(self.text.clone()),
        }
    }

    /// Removes its file when it was made for this call, answers its sender and gives back
    /// what happens next.
    fn finish(self, said: bool) -> After {
        if let Audio::Wav {
            path, remove: true, ..
        } = &self.audio
        {
            let _ = std::fs::remove_file(path);
        }
        if let Some(reply) = self.reply {
            let _ = reply.send(said);
        }
        self.after
    }
}

/// The sound playing now; `sound` is `None` for the tick.
struct Playing {
    id: u64,
    task: JoinHandle<()>,
    by_tts: bool,
    /// Talk now stopped it: the call listens while it winds down.
    stopped: bool,
    sound: Option<Sound>,
}

/// A typed turn whose answer has not come yet.
struct Awaiting {
    pane: String,
    prompt: String,
    next_tick: Instant,
    gives_up_at: Instant,
    next_read: Instant,
    seen_busy: bool,
    idle_since: Option<Instant>,
    /// A line answering the turn has arrived, or is coming from the turn-end update.
    answered: bool,
    update_asked: bool,
    /// The turn's `harold.call_turn` span; it ends when the update line arrives or this is
    /// dropped.
    turn: Option<tracing::Span>,
}

/// A turn that waits for a busy chief.
struct HeldTurn {
    text: String,
    next_try: Instant,
    gives_up_at: Instant,
}

/// How one try at typing a turn ended.
enum Typing {
    Typed(String),
    NoChief,
    Busy,
    Failed,
}

struct Call<P: CallPorts> {
    ports: Arc<P>,
    sink: Arc<P::Sink>,
    status: Arc<Mutex<CallStatus>>,
    events: mpsc::UnboundedSender<Event<P::Worker>>,
    dir: PathBuf,
    detector: SpeechDetector,
    mode: Mode,
    /// The worker while it answers nothing; a request's task owns it until it reports back.
    worker: Option<P::Worker>,
    /// Counts the workers started; a request's answer carries the count of its worker.
    generation: u64,
    /// The health of the call's worker, while it is followed.
    health: Option<watch::Receiver<Health>>,
    worker_ready: bool,
    request: Option<JoinHandle<()>>,
    jobs: VecDeque<Job>,
    worker_exits: u32,
    sounds: VecDeque<Sound>,
    playing: Option<Playing>,
    /// When the microphone is heard again after the last sound.
    unmuted_at: Instant,
    awaiting: Option<Awaiting>,
    held: Option<HeldTurn>,
    /// Set by talk now: the chief's lines are taken as said until the next turn is typed.
    skip_chief_lines: bool,
    /// Set by talk now: sounds that wait after talk now do so until the next turn is cut
    /// or typed, or until this time, when no turn has started by then.
    hold_others_until: Option<Instant>,
    /// When the last turn was cut or the last sound played; the call ends `IDLE_LIMIT` after.
    active_at: Instant,
    done: bool,
    reader: Option<JoinHandle<()>>,
    updates: Vec<JoinHandle<()>>,
    awake: Option<P::Awake>,
    next_id: u64,
}

impl<P: CallPorts> Call<P> {
    /// Steps 1 to 4 of a call's start. `None` when the call ended before it listened.
    async fn open(
        ports: Arc<P>,
        status: Arc<Mutex<CallStatus>>,
        events: mpsc::UnboundedSender<Event<P::Worker>>,
        started: watch::Sender<bool>,
    ) -> Option<Self> {
        {
            let mut status = lock(&status);
            status.last_result = None;
            status.last_transcript = None;
        }
        let calls = ports.calls_dir();
        let dir = calls.join(format!("call-{}", Uuid::new_v4()));
        let not_opened = NotOpened {
            ports: &*ports,
            status: &status,
            dir: &dir,
        };
        if let Err(error) = make_call_dir(&calls, &dir) {
            warn!(error = %error, "call folder not made");
            not_opened.close(started, Ended::VoiceFailed).await;
            return None;
        }
        ports.stop_desk_line();
        let chief = ports.chief_folder();
        if !chief.is_some_and(|folder| chief_running(&ports.panes().agents(), &folder)) {
            lock(&status).last_result = Some(TurnResult::NoChief);
            not_opened.close(started, Ended::NoChief).await;
            return None;
        }
        let awake = match ports.keep_awake() {
            Ok(awake) => Some(awake),
            Err(error) => {
                warn!(error = %error, "display not kept awake");
                None
            }
        };
        let mut source = match ports.open_source() {
            Ok(source) => source,
            Err(error) => {
                warn!(error = %error, "microphone not opened");
                not_opened.close(started, Ended::MicrophoneFailed).await;
                return None;
            }
        };
        let worker = match ports.start_worker(&dir) {
            Ok(worker) => worker,
            Err(error) => {
                let reason = error.exit_reason().unwrap_or("failed_to_load");
                warn!(reason, "voice worker exited");
                not_opened.close(started, Ended::VoiceFailed).await;
                return None;
            }
        };
        let blocks = events.clone();
        let reader = tokio::spawn(async move {
            while let Some((samples, captured_at)) = source.next_stamped().await {
                let block = Event::Block {
                    samples,
                    captured_at,
                };
                if blocks.send(block).is_err() {
                    return;
                }
            }
            let _ = blocks.send(Event::SourceEnded);
        });
        info!("call started");
        let mut call = Self {
            sink: ports.sink(),
            ports,
            status,
            events,
            dir,
            detector: SpeechDetector::new(),
            mode: Mode::Listening,
            worker: None,
            generation: 0,
            health: None,
            worker_ready: false,
            request: None,
            jobs: VecDeque::new(),
            worker_exits: 0,
            sounds: VecDeque::new(),
            playing: None,
            unmuted_at: Instant::now(),
            awaiting: None,
            held: None,
            skip_chief_lines: false,
            hold_others_until: None,
            active_at: Instant::now(),
            done: false,
            reader: Some(reader),
            updates: Vec::new(),
            awake,
            next_id: 0,
        };
        call.watch_worker(&worker);
        call.worker = Some(worker);
        call.publish();
        started.send_replace(true);
        Some(call)
    }

    async fn run(&mut self, inbox: &mut mpsc::UnboundedReceiver<Event<P::Worker>>) {
        while !self.done {
            self.next_request();
            self.next_sound();
            self.publish();
            let wake = self.next_wake();
            let far = Instant::now() + Duration::from_secs(86_400);
            tokio::select! {
                event = inbox.recv() => match event {
                    Some(event) => self.on_event(event).await,
                    None => self.end(Ended::Stopped, None).await,
                },
                () = sleep_until(wake.unwrap_or(far)), if wake.is_some() => self.on_timer().await,
                health = health_change(&mut self.health), if self.health.is_some() => {
                    self.health_changed(health).await;
                }
            }
        }
    }

    async fn on_event(&mut self, event: Event<P::Worker>) {
        match event {
            Event::Block { .. } if self.ports.voice_muted() => self.voice_muted().await,
            Event::Block {
                samples,
                captured_at,
            } => self.heard_block(&samples, captured_at),
            Event::SourceEnded => {
                self.end(Ended::MicrophoneFailed, Some(VOICE_FAILED)).await;
            }
            Event::Stop(why) => self.end(why, None).await,
            Event::TalkNow => self.talk_now(),
            Event::ForgetTurn => self.forget_turn(),
            Event::Say { line, by, reply } => self.line(line, by, Some(reply)),
            Event::Transcribed {
                generation,
                dir,
                result,
                worker,
            } => {
                let _ = std::fs::remove_dir_all(&dir);
                self.transcribed(generation, result, worker).await;
            }
            Event::Made {
                generation,
                id,
                wav,
                result,
                worker,
            } => self.made(generation, id, wav, result, worker).await,
            Event::Played { id, played } => self.played(id, played).await,
            Event::TurnEndUpdate(line) => self.turn_end_update(line),
        }
    }

    // --- the samples ---

    /// A block recorded while a sound played, or within the tail after, is not heard, even
    /// when it is read later.
    fn heard_block(&mut self, block: &[i16], captured_at: Instant) {
        if self.mode != Mode::Listening {
            self.detector.reset();
            return;
        }
        let muted = self.playing.is_some() || captured_at < self.unmuted_at;
        let Some(turn) = self.detector.push(block, muted).into_iter().next() else {
            return;
        };
        self.turn_ended(&turn);
    }

    /// Forgets the speech of a turn not yet ended, keeping the background level. A newer
    /// phone screen that takes the call over means the user is starting again: joining the
    /// older screen's half of a sentence to the newer's could type a garbled line, with Enter,
    /// into the chief's pane. Only a turn being spoken while listening has anything to forget.
    fn forget_turn(&mut self) {
        if self.mode == Mode::Listening {
            self.detector.reset();
        }
    }

    /// Writes the turn to `<call dir>/turn-<uuid>/turn.wav` for the worker to transcribe.
    fn turn_ended(&mut self, samples: &[i16]) {
        self.mode = Mode::Transcribing;
        self.detector.reset();
        self.hold_others_until = None;
        self.active_at = Instant::now();
        let dir = self.dir.join(format!("turn-{}", Uuid::new_v4()));
        let written = private_dir(&dir)
            .and_then(|()| std::fs::write(dir.join(TURN_WAV), wav_bytes(samples, SAMPLE_RATE)));
        if let Err(error) = written {
            warn!(error = %error, "turn not written");
            let _ = std::fs::remove_dir_all(&dir);
            self.dropped(TurnResult::Failed);
            self.mode = Mode::Listening;
            return;
        }
        self.jobs.push_back(Job::Transcribe { dir });
    }

    // --- the worker ---

    /// Hands the next job to the worker when it is free. The request runs in its own task,
    /// which owns the worker until it reports back: a request is not cancel-safe.
    fn next_request(&mut self) {
        if self.done || self.request.is_some() || self.jobs.is_empty() {
            return;
        }
        let Some(mut worker) = self.worker.take() else {
            return;
        };
        let Some(job) = self.jobs.pop_front() else {
            self.worker = Some(worker);
            return;
        };
        let events = self.events.clone();
        let generation = self.generation;
        self.request = Some(tokio::spawn(async move {
            let event = match job {
                Job::Transcribe { dir } => {
                    let result = worker.transcribe(&dir.join(TURN_WAV)).await;
                    Event::Transcribed {
                        generation,
                        dir,
                        result,
                        worker,
                    }
                }
                Job::Make { id, text, wav } => {
                    let result = worker.say(&text, &wav).await;
                    Event::Made {
                        generation,
                        id,
                        wav,
                        result,
                        worker,
                    }
                }
            };
            let _ = events.send(event);
        }));
    }

    async fn transcribed(
        &mut self,
        generation: u64,
        result: Result<Transcript, WorkerError>,
        worker: P::Worker,
    ) {
        self.request = None;
        let error = match result {
            Ok(transcript) => {
                self.worker_back(generation, worker).await;
                self.heard(transcript).await;
                return;
            }
            Err(error) => error,
        };
        self.dropped(TurnResult::Failed);
        self.mode = Mode::Listening;
        if error.exit_reason().is_some() {
            worker.close().await;
            self.worker_died(generation, error).await;
        } else {
            self.worker_back(generation, worker).await;
        }
    }

    async fn made(
        &mut self,
        generation: u64,
        id: u64,
        wav: PathBuf,
        result: Result<(), WorkerError>,
        worker: P::Worker,
    ) {
        self.request = None;
        let at = self.sounds.iter().position(|sound| sound.id == id);
        match result {
            Ok(()) => {
                self.worker_back(generation, worker).await;
                match at {
                    Some(at) => self.keep_made(at),
                    None => {
                        let _ = std::fs::remove_file(&wav);
                    }
                }
            }
            // The worker could not make it: the `[tts]` command says it instead.
            Err(WorkerError::Failed) => {
                self.worker_back(generation, worker).await;
                let _ = std::fs::remove_file(&wav);
                if let Some(at) = at {
                    self.sounds[at].audio = Audio::Tts;
                }
            }
            // The worker went: the line is made again by the next one. Its text is taken
            // first, because a restart puts the restart phrase in front of it.
            Err(error) => {
                let text = at.map(|at| self.sounds[at].text.clone());
                worker.close().await;
                self.worker_died(generation, error).await;
                if let (false, Some(text)) = (self.done, text) {
                    self.jobs.push_front(Job::Make { id, text, wav });
                }
            }
        }
    }

    /// Takes back the worker a request owned. One whose death was already handled is closed.
    async fn worker_back(&mut self, generation: u64, worker: P::Worker) {
        if generation == self.generation {
            self.worker = Some(worker);
        } else {
            worker.close().await;
        }
    }

    /// Marks the sound at `at` ready, moving a new phrase into the phrase folder.
    fn keep_made(&mut self, at: usize) {
        let phrases = self.ports.phrases_dir();
        let Audio::Wav {
            path,
            ready,
            keep_as,
            remove,
        } = &mut self.sounds[at].audio
        else {
            return;
        };
        *ready = true;
        let Some(keep_as) = keep_as.take() else {
            return;
        };
        let kept = std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&phrases)
            .and_then(|()| std::fs::rename(&*path, &keep_as));
        match kept {
            Ok(()) => {
                *path = keep_as;
                *remove = false;
            }
            Err(error) => warn!(error = %error, "phrase not kept"),
        }
    }

    /// Follows the health of `worker`, the call's worker from now on.
    fn watch_worker(&mut self, worker: &P::Worker) {
        let health = worker.health();
        self.worker_ready = *health.borrow() == Health::Ready;
        self.health = Some(health);
    }

    async fn health_changed(&mut self, health: Health) {
        match health {
            Health::Loading => {}
            Health::Ready => self.worker_ready = true,
            Health::Gone(error) => self.worker_died(self.generation, error).await,
        }
    }

    /// The worker of `generation` has gone, seen through its health or a request; each
    /// death counts once. A worker that never loaded, a second death, or a restart that
    /// fails ends the call. Otherwise the worker is started once more, and requests go to
    /// it from now on.
    async fn worker_died(&mut self, generation: u64, error: WorkerError) {
        if self.done || generation != self.generation {
            return;
        }
        let reason = error.exit_reason().unwrap_or("exited");
        warn!(reason, "voice worker exited");
        self.generation += 1;
        self.health = None;
        if let Some(worker) = self.worker.take() {
            worker.close().await;
        }
        self.worker_exits += 1;
        let restarted =
            if error == WorkerError::FailedToLoad || !self.worker_ready || self.worker_exits > 1 {
                None
            } else {
                match self.ports.start_worker(&self.dir) {
                    Ok(worker) => Some(worker),
                    Err(error) => {
                        let reason = error.exit_reason().unwrap_or("failed_to_load");
                        warn!(reason, "voice worker exited");
                        None
                    }
                }
            };
        let Some(worker) = restarted else {
            if self.mode == Mode::Transcribing {
                self.dropped(TurnResult::Failed);
            }
            self.end(Ended::VoiceFailed, Some(VOICE_FAILED)).await;
            return;
        };
        self.watch_worker(&worker);
        self.worker = Some(worker);
        let id = self.new_id();
        self.sounds.push_front(Sound {
            id,
            text: VOICE_STOPPED.to_string(),
            audio: Audio::Tts,
            by: None,
            answers: false,
            waits_after_talk_now: true,
            after: After::Nothing,
            reply: None,
        });
    }

    // --- a turn ---

    async fn heard(&mut self, transcript: Transcript) {
        let text = one_line(&transcript.text);
        if text.is_empty() || transcript.no_speech_prob > NO_SPEECH_LIMIT {
            self.dropped(TurnResult::NothingHeard);
            self.mode = Mode::Listening;
            self.phrase(NOTHING_HEARD, After::Nothing);
            return;
        }
        match hang_up(&text) {
            Spoken::Turn(turn) => self.turn(turn.to_string()).await,
            Spoken::TurnThenBye(turn) => {
                let (turn, gives_up_at) = match self.held.take() {
                    Some(held) => (format!("{} {turn}", held.text), held.gives_up_at),
                    None => (turn.to_string(), Instant::now() + BUSY_LIMIT),
                };
                self.last_turn(turn, gives_up_at).await;
            }
            Spoken::ByeOnly => self.say_bye(),
        }
    }

    /// Types the turn, or joins it to the turn waiting for a busy chief (Decision 9).
    async fn turn(&mut self, text: String) {
        self.mode = Mode::Listening;
        if let Some(held) = self.held.as_mut() {
            held.text.push(' ');
            held.text.push_str(&text);
            return;
        }
        match try_type(&*self.ports, &text).await {
            Typing::Typed(pane) => self.typed(pane, text),
            Typing::NoChief => self.no_chief(),
            Typing::Busy => {
                let now = Instant::now();
                self.held = Some(HeldTurn {
                    text,
                    next_try: now + BUSY_RETRY,
                    gives_up_at: now + BUSY_LIMIT,
                });
                self.phrase(CHIEF_BUSY, After::Nothing);
            }
            Typing::Failed => self.dropped(TurnResult::Failed),
        }
    }

    /// The turn said before a bye, and then the call ends. A busy chief gets the turn as
    /// it would during the call, until `gives_up_at`, from a task that outlives the call and
    /// holds no audio.
    async fn last_turn(&mut self, text: String, gives_up_at: Instant) {
        match try_type(&*self.ports, &text).await {
            Typing::Typed(pane) => self.typed(pane, text),
            Typing::NoChief => self.dropped(TurnResult::NoChief),
            Typing::Busy => {
                let next_try = Instant::now() + BUSY_RETRY;
                tokio::spawn(pass_on_when_free(
                    Arc::clone(&self.ports),
                    text,
                    next_try,
                    gives_up_at,
                ));
            }
            Typing::Failed => self.dropped(TurnResult::Failed),
        }
        self.say_bye();
    }

    fn typed(&mut self, pane: String, text: String) {
        let turn = log_typed(&pane, &text);
        self.hold_others_until = None;
        let prompt = format!("{MARK} {text}");
        {
            let mut status = lock(&self.status);
            status.last_result = Some(TurnResult::Typed);
            status.last_transcript = Some(text);
        }
        self.skip_chief_lines = false;
        self.mode = Mode::Waiting;
        let now = Instant::now();
        self.awaiting = Some(Awaiting {
            pane,
            prompt,
            next_tick: now + TICK_EVERY,
            gives_up_at: now + ANSWER_LIMIT,
            next_read: now + PANE_READ_EVERY,
            seen_busy: false,
            idle_since: None,
            answered: false,
            update_asked: false,
            turn: Some(turn),
        });
    }

    fn dropped(&self, result: TurnResult) {
        log_dropped(result);
        lock(&self.status).last_result = Some(result);
    }

    /// Drops a turn still waiting for a busy chief.
    fn drop_held(&mut self) {
        if self.held.take().is_some() {
            self.dropped(TurnResult::ChiefBusy);
        }
    }

    /// Hands a turn still waiting for a busy chief to a task that outlives the call: Harold
    /// promised to pass it on, so it keeps its retry and its two minutes from when it was
    /// first held.
    fn pass_on_held(&mut self) {
        if let Some(held) = self.held.take() {
            tokio::spawn(pass_on_when_free(
                Arc::clone(&self.ports),
                held.text,
                held.next_try,
                held.gives_up_at,
            ));
        }
    }

    fn say_bye(&mut self) {
        self.ending_phrase(BYE, Ended::Bye);
    }

    fn no_chief(&mut self) {
        self.drop_held();
        self.dropped(TurnResult::NoChief);
        self.ending_phrase(NO_CHIEF, Ended::NoChief);
    }

    fn ending_phrase(&mut self, text: &'static str, why: Ended) {
        self.mode = Mode::Ending;
        self.awaiting = None;
        self.phrase(text, After::End(why));
        if let Some(sound) = self.sounds.pop_back() {
            self.sounds.push_front(sound);
        }
    }

    // --- the timers ---

    fn next_wake(&self) -> Option<Instant> {
        let held = self
            .held
            .iter()
            .flat_map(|held| [held.next_try, held.gives_up_at]);
        let awaiting = self
            .awaiting
            .iter()
            .filter(|awaiting| !awaiting.answered)
            .flat_map(|awaiting| {
                let mut times = vec![awaiting.next_tick, awaiting.gives_up_at];
                if !awaiting.update_asked {
                    times.push(awaiting.next_read);
                    times.extend(awaiting.idle_since.map(|at| at + LINE_GRACE));
                }
                times
            });
        let idle = std::iter::once(self.active_at + IDLE_LIMIT);
        held.chain(awaiting)
            .chain(self.hold_others_until)
            .chain(idle)
            .min()
    }

    async fn on_timer(&mut self) {
        let now = Instant::now();
        if let Some(held) = &self.held {
            if now >= held.gives_up_at {
                self.held = None;
                self.dropped(TurnResult::ChiefBusy);
            } else if now >= held.next_try {
                self.try_held(now).await;
            }
        }
        self.await_answer(now);
        if self.hold_others_until.is_some_and(|until| now >= until) {
            self.hold_others_until = None;
        }
        if now >= self.active_at + IDLE_LIMIT {
            self.idle(now);
        }
    }

    /// Ends a call that nothing happened in for `IDLE_LIMIT`, so a call left open does not
    /// keep the display awake for good. A call doing something then starts its clock again.
    fn idle(&mut self, now: Instant) {
        let quiet = self.mode == Mode::Listening
            && !self.detector.in_turn()
            && self.playing.is_none()
            && self.sounds.is_empty()
            && self.held.is_none();
        if !quiet {
            self.active_at = now;
            return;
        }
        self.ending_phrase(BYE, Ended::Idle);
    }

    /// Harold's voice was muted during the call: it ends at once, with no last words.
    async fn voice_muted(&mut self) {
        self.end(Ended::VoiceMuted, None).await;
        lock(&self.status).last_result = Some(TurnResult::VoiceMuted);
    }

    /// Tries the turn waiting for a busy chief again, unless the user is speaking or a
    /// turn is being transcribed, which may join it.
    async fn try_held(&mut self, now: Instant) {
        let Some(held) = self.held.as_mut() else {
            return;
        };
        if self.mode != Mode::Listening || self.detector.in_turn() {
            held.next_try = now + BUSY_RETRY;
            return;
        }
        let text = held.text.clone();
        match try_type(&*self.ports, &text).await {
            Typing::Typed(pane) => {
                self.held = None;
                self.typed(pane, text);
            }
            Typing::NoChief => {
                self.held = None;
                self.no_chief();
            }
            Typing::Busy => {
                if let Some(held) = self.held.as_mut() {
                    held.next_try = Instant::now() + BUSY_RETRY;
                }
            }
            Typing::Failed => {
                self.held = None;
                self.dropped(TurnResult::Failed);
            }
        }
    }

    /// The tick, `No answer yet.`, and the chief's turn ending without a line.
    fn await_answer(&mut self, now: Instant) {
        let Some(awaiting) = self.awaiting.as_mut() else {
            return;
        };
        if awaiting.answered {
            return;
        }
        if now >= awaiting.gives_up_at {
            self.awaiting = None;
            self.mode = Mode::Listening;
            let phrase = self.ports.no_answer_phrase();
            self.phrase(phrase, After::Nothing);
            return;
        }
        let tick = now >= awaiting.next_tick;
        if tick {
            awaiting.next_tick += TICK_EVERY;
        }
        if !awaiting.update_asked && now >= awaiting.next_read {
            awaiting.next_read += PANE_READ_EVERY;
            let view = self.ports.panes().agents();
            let state = view
                .panes
                .iter()
                .find(|pane| pane.pane_id == awaiting.pane)
                .map(|pane| pane.state);
            match state {
                Some(EffectiveAgentState::Busy) => {
                    awaiting.seen_busy = true;
                    awaiting.idle_since = None;
                }
                Some(EffectiveAgentState::Idle) if awaiting.seen_busy => {
                    awaiting.idle_since.get_or_insert(now);
                }
                _ => {}
            }
        }
        if !awaiting.update_asked
            && awaiting
                .idle_since
                .is_some_and(|idle| now >= idle + LINE_GRACE)
        {
            awaiting.update_asked = true;
            let ports = Arc::clone(&self.ports);
            let events = self.events.clone();
            let prompt = awaiting.prompt.clone();
            let turn = awaiting.turn.clone().unwrap_or_else(tracing::Span::none);
            self.updates.push(tokio::spawn(async move {
                let line = ports.turn_end_update(&prompt, &turn).await;
                drop(turn);
                let _ = events.send(Event::TurnEndUpdate(line));
            }));
        }
        if tick {
            self.play_tick();
        }
    }

    // --- lines and phrases ---

    fn new_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// A line said into the call: made into audio by the worker, then played in turn.
    fn line(&mut self, line: String, by: Speaker, reply: Option<oneshot::Sender<bool>>) {
        if self.mode == Mode::Ending {
            if let Some(reply) = reply {
                let _ = reply.send(false);
            }
            return;
        }
        if by == Speaker::Chief && self.skip_chief_lines {
            if let Some(reply) = reply {
                let _ = reply.send(true);
            }
            return;
        }
        let answers = by == Speaker::Chief
            && self.awaiting.as_mut().is_some_and(|awaiting| {
                awaiting.answered = true;
                true
            });
        let id = self.new_id();
        let wav = self.dir.join(format!("answer-{}.wav", Uuid::new_v4()));
        self.jobs.push_back(Job::Make {
            id,
            text: line.clone(),
            wav: wav.clone(),
        });
        self.sounds.push_back(Sound {
            id,
            text: line,
            audio: Audio::Wav {
                path: wav,
                ready: false,
                keep_as: None,
                remove: true,
            },
            by: Some(by),
            answers,
            waits_after_talk_now: by == Speaker::OtherAgent,
            after: After::Nothing,
            reply,
        });
    }

    /// The chief's turn ended with no line: the desk's turn-end update answers it.
    fn turn_end_update(&mut self, line: String) {
        let Some(awaiting) = self.awaiting.as_mut() else {
            return;
        };
        if !awaiting.update_asked || awaiting.answered {
            return;
        }
        // The line has arrived: the call turn ends (the update task let go of its copy).
        awaiting.turn = None;
        self.line(line, Speaker::Chief, None);
    }

    /// One of Harold's phrases, from the phrase folder, or made by the worker and kept
    /// there for later calls (Decision 8).
    fn phrase(&mut self, text: &'static str, after: After) {
        let kept = phrase_file(&self.ports.phrases_dir(), &self.ports.voice_name(), text);
        let id = self.new_id();
        let audio = if kept.is_file() {
            Audio::Wav {
                path: kept,
                ready: true,
                keep_as: None,
                remove: false,
            }
        } else {
            let wav = self.dir.join(format!("phrase-{}.wav", Uuid::new_v4()));
            let job = Job::Make {
                id,
                text: text.to_string(),
                wav: wav.clone(),
            };
            if after == After::Nothing {
                self.jobs.push_back(job);
            } else {
                self.jobs.push_front(job);
            }
            Audio::Wav {
                path: wav,
                ready: false,
                keep_as: Some(kept),
                remove: true,
            }
        };
        self.sounds.push_back(Sound {
            id,
            text: text.to_string(),
            audio,
            by: None,
            answers: false,
            waits_after_talk_now: false,
            after,
            reply: None,
        });
    }

    // --- playing ---

    /// Plays the first sound in line when it is ready, nothing plays, and the user is not
    /// mid-turn (Decision 5).
    fn next_sound(&mut self) {
        if self.done || self.playing.is_some() {
            return;
        }
        let Some(first) = self.sounds.front() else {
            return;
        };
        if !first.ready() || (self.mode == Mode::Listening && self.detector.in_turn()) {
            return;
        }
        if self.hold_others_until.is_some() && first.waits_after_talk_now {
            return;
        }
        let Some(sound) = self.sounds.pop_front() else {
            return;
        };
        if sound.answers && self.awaiting.take().is_some() && self.mode == Mode::Waiting {
            self.mode = Mode::Listening;
        }
        let by_tts = matches!(sound.audio, Audio::Tts);
        let task = match &sound.audio {
            Audio::Wav { path, .. } => self.spawn_play(sound.id, path.clone(), sound.said()),
            Audio::Tts => self.spawn_tts(sound.id, sound.text.clone()),
        };
        self.playing = Some(Playing {
            id: sound.id,
            task,
            by_tts,
            stopped: false,
            sound: Some(sound),
        });
    }

    fn play_tick(&mut self) {
        if self.playing.is_some() || !self.sounds.is_empty() {
            return;
        }
        let id = self.new_id();
        let task = self.spawn_play(id, self.dir.join(TICK_WAV), Said::Tick);
        self.playing = Some(Playing {
            id,
            task,
            by_tts: false,
            stopped: false,
            sound: None,
        });
    }

    fn spawn_play(&self, id: u64, wav: PathBuf, said: Said) -> JoinHandle<()> {
        let sink = Arc::clone(&self.sink);
        let events = self.events.clone();
        tokio::spawn(async move {
            let played = match sink.play_said(&wav, &said).await {
                Ok(()) => true,
                Err(error) => {
                    warn!(error = %error, "call sound not played");
                    false
                }
            };
            let _ = events.send(Event::Played { id, played });
        })
    }

    fn spawn_tts(&self, id: u64, text: String) -> JoinHandle<()> {
        let ports = Arc::clone(&self.ports);
        let events = self.events.clone();
        tokio::spawn(async move {
            ports.speak_with_tts(&text).await;
            let _ = events.send(Event::Played { id, played: true });
        })
    }

    /// A sound ended. One the sink could not play is said through the `[tts]` command
    /// next, and only then counts as said.
    async fn played(&mut self, id: u64, played: bool) {
        if self.playing.as_ref().is_none_or(|playing| playing.id != id) {
            return;
        }
        let Some(playing) = self.playing.take() else {
            return;
        };
        self.unmuted_at = Instant::now() + MUTE_TAIL;
        self.active_at = Instant::now();
        let Some(mut sound) = playing.sound else {
            return;
        };
        if !played && !playing.stopped && matches!(sound.audio, Audio::Wav { .. }) {
            if let Audio::Wav {
                path, remove: true, ..
            } = &sound.audio
            {
                let _ = std::fs::remove_file(path);
            }
            sound.audio = Audio::Tts;
            self.sounds.push_front(sound);
            return;
        }
        if let After::End(why) = sound.finish(true) {
            self.end(why, None).await;
        }
    }

    /// Stops what plays and listens at once. The chief's lines count as said until the next
    /// turn is typed (Decision 12), and other agents' lines and the restart phrase wait until
    /// the next turn is cut or typed, or for `HOLD_LIMIT` when no turn starts, so a delivery
    /// waiting on them is never held for long.
    fn talk_now(&mut self) {
        if let Some(playing) = self.playing.as_mut() {
            self.sink.stop();
            if playing.by_tts {
                self.ports.stop_desk_line();
            }
            playing.stopped = true;
        }
        self.skip_chief_lines = true;
        self.hold_others_until = Some(Instant::now() + HOLD_LIMIT);
        let (chief, others): (VecDeque<Sound>, VecDeque<Sound>) = std::mem::take(&mut self.sounds)
            .into_iter()
            .partition(|sound| sound.by == Some(Speaker::Chief));
        self.sounds = others;
        for sound in chief {
            let id = sound.id;
            self.jobs
                .retain(|job| !matches!(job, Job::Make { id: made, .. } if *made == id));
            sound.finish(true);
        }
        self.awaiting = None;
        if self.mode == Mode::Waiting {
            self.mode = Mode::Listening;
        }
    }

    // --- the status and the end ---

    fn publish(&self) {
        if self.done {
            return;
        }
        let speaking = self
            .playing
            .as_ref()
            .is_some_and(|playing| playing.sound.is_some() && !playing.stopped);
        let state = match self.mode {
            _ if speaking => CallState::Speaking,
            Mode::Listening => CallState::Listening,
            Mode::Transcribing => CallState::Transcribing,
            Mode::Waiting => CallState::Waiting,
            Mode::Ending => CallState::Speaking,
        };
        lock(&self.status).state = state;
    }

    /// Ends the call: a turn waiting for a busy chief is passed on once the chief is free,
    /// or dropped when Harold shuts down, lines not yet played go back to their senders, `last_words` are said through the `[tts]` command, the source, the
    /// worker, the call's folder and the display's wake are let go, and the transcript is
    /// forgotten.
    async fn end(&mut self, why: Ended, last_words: Option<&'static str>) {
        if self.done {
            return;
        }
        self.done = true;
        self.mode = Mode::Ending;
        // The status reads as the call is while it says its last words and cleans up. The
        // transcript goes the moment the status reads off.
        {
            let mut status = lock(&self.status);
            if last_words.is_some() {
                status.state = CallState::Speaking;
            } else {
                status.state = CallState::Off;
                status.last_transcript = None;
            }
        }
        if why == Ended::Shutdown {
            self.drop_held();
        } else {
            self.pass_on_held();
        }
        if let Some(playing) = self.playing.take() {
            self.sink.stop();
            if playing.by_tts {
                self.ports.stop_desk_line();
            }
            playing.task.abort();
            let _ = playing.task.await;
            if let Some(sound) = playing.sound {
                sound.finish(true);
            }
        }
        for sound in std::mem::take(&mut self.sounds) {
            sound.finish(false);
        }
        for update in self.updates.drain(..) {
            update.abort();
        }
        if let Some(words) = last_words {
            self.ports.speak_with_tts(words).await;
        }
        if let Some(reader) = self.reader.take() {
            reader.abort();
            let _ = reader.await;
        }
        // A request still running owns the worker; ending its task drops the worker.
        if let Some(request) = self.request.take() {
            request.abort();
            let _ = request.await;
        }
        if let Some(worker) = self.worker.take() {
            worker.close().await;
        }
        if let Err(error) = std::fs::remove_dir_all(&self.dir) {
            warn!(error = %error, "call folder not removed");
        }
        self.awake = None;
        info!(reason = why.as_str(), "call ended");
        self.ports.call_ended(why.as_str());
        let mut status = lock(&self.status);
        status.state = CallState::Off;
        // The transcript is shown only while the call lasts.
        status.last_transcript = None;
    }
}

/// The next health `health` reports. Once its worker is gone for good, `health` is set to
/// `None` and is no longer followed. Cancel-safe.
async fn health_change(health: &mut Option<watch::Receiver<Health>>) -> Health {
    let Some(receiver) = health.as_mut() else {
        return std::future::pending().await;
    };
    let changed = receiver.changed().await;
    let now = *receiver.borrow_and_update();
    if changed.is_err() {
        *health = None;
    }
    now
}

/// A call that ended before it listened.
struct NotOpened<'a, P> {
    ports: &'a P,
    status: &'a Mutex<CallStatus>,
    dir: &'a Path,
}

impl<P: CallPorts> NotOpened<'_, P> {
    /// Answers START, says why the call ended, and removes its folder.
    async fn close(&self, started: watch::Sender<bool>, why: Ended) {
        lock(self.status).state = CallState::Off;
        started.send_replace(true);
        if why == Ended::NoChief {
            // No worker runs yet: the phrase plays from the phrase folder, or else
            // through the `[tts]` command (Decision 8).
            let kept = phrase_file(
                &self.ports.phrases_dir(),
                &self.ports.voice_name(),
                NO_CHIEF,
            );
            let said = Said::Phrase(NO_CHIEF.into());
            if !kept.is_file() || self.ports.sink().play_said(&kept, &said).await.is_err() {
                self.ports.speak_with_tts(NO_CHIEF).await;
            }
        } else {
            self.ports.speak_with_tts(VOICE_FAILED).await;
        }
        let _ = std::fs::remove_dir_all(self.dir);
        info!(reason = why.as_str(), "call ended");
        self.ports.call_ended(why.as_str());
    }
}
