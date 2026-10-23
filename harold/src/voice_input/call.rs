//! One call with the chief at the desk. A call is one task that owns the worker, the
//! detector and the state, and takes its events from one channel: sample blocks, control
//! commands, lines to say, timers, and the ends of the work it spawned (a request to the
//! worker, a sound, a turn-end update). `CallControl` starts it, talks to it and reads its
//! status.

use super::answerer::{Answer, Ask, CallAnswering, CallSoFar, Failure, Remark, Who};
use super::hand_over::HandOvers;
use super::hang_up::{Spoken, hang_up};
use super::models::Status as ModelsStatus;
use super::phone::sound_length;
use super::phrases::{
    BYE, CHIEF_BUSY, FROM_THE_CHIEF, MODEL_MISSING, NO_CHIEF, NOTHING_HEARD, PASSED_ON,
    VOICE_FAILED, VOICE_STOPPED, phrase_file,
};
use super::speech::{FRAME, MAX_TURN_FRAMES, START_FRAMES, SpeechDetector};
use super::wav::{pickup_wav, ringback_wav, tick_wav, wav_bytes};
use super::worker::{Health, LoadCause, Transcript, VoiceWorker, WorkerError, log_exited};
use super::{
    AudioSink, AudioSource, CallPorts, CallState, CallStatus, SAMPLE_RATE, Said, Speaker,
    SpeechModels, TurnResult,
};
use crate::agent::domain::EffectiveAgentState;
use crate::away_message::one_line;
use crate::outbound::left_out::{ChiefLookup, resolve_chief, same_folder};
use crate::telemetry::{self, StartedBy};
use crate::timed_prompts::{
    AgentPane, AgentsView, Attempt, Ports, Reason, Writer, type_when_ready,
};
use serde_json::Value;
use std::collections::VecDeque;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until};
use tracing::{Instrument as _, info, warn};
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
/// How long a run of the call answerer may take in all, the facts and the model, before the
/// call gives up on it whatever the answerer says. The answerer's own limit is shorter.
const RUN_LIMIT: Duration = Duration::from_secs(30);
/// The most remarks of this call kept for the answerer's next turn.
const REMARKS_KEPT: usize = 40;
/// The mark in front of each turn typed into the chief's pane.
pub(super) const MARK: &str = "🎙";
/// How long one cycle of the ringback lasts: the ring is whole or not played.
const RING_CYCLE: Duration = Duration::from_secs(3);
/// How long the call answerer's warm-up has before the call picks up without it.
const WARM_LIMIT: Duration = Duration::from_secs(5);
/// How long the facts the warm-up read are kept for the call's first turn.
const WARM_KEPT: Duration = Duration::from_secs(30);
const TICK_WAV: &str = "tick.wav";
const RINGBACK_WAV: &str = "ringback.wav";
const PICKUP_WAV: &str = "pickup.wav";
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
    /// The call answerer's run `id` ended.
    Answered {
        id: u64,
        answer: Answer,
    },
    /// The call answerer's warm-up ended, with the facts it read, or was cut at its limit.
    Warmed(Option<Value>),
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
        Attempt::Wait(reason) => Typing::Busy(reason),
        Attempt::EnterFailed { .. } => Typing::Failed,
    }
}

/// Opens a turn's `harold.call_turn` span, the root of the turn's trace. The stop hook's
/// `harold.turn` joins it. The span ends when its last holder lets go.
pub(super) fn open_turn(call_from: &'static str) -> tracing::Span {
    tracing::info_span!(
        target: telemetry::TRACE_TARGET,
        parent: None,
        "harold.call_turn",
        pane_id = tracing::field::Empty,
        call_from,
        result = tracing::field::Empty,
        first_sound_ms = tracing::field::Empty,
        answer_ms = tracing::field::Empty,
        busy_reason = tracing::field::Empty,
        answered_by = tracing::field::Empty,
    )
}

/// Logs a typed turn on its open `harold.call_turn` span.
pub(super) fn log_typed(turn: &tracing::Span, pane: &str, text: &str) {
    let words = text.split_whitespace().count();
    turn.record("pane_id", pane);
    turn.record("result", TurnResult::Typed.reason());
    // Fails only when no export layer is installed, and then there is no trace to join.
    let cx = turn.context();
    turn.in_scope(|| info!(pane_id = %pane, words, "spoken prompt typed"));
    telemetry::prompt_typed(
        pane,
        StartedBy::Voice,
        Some(cx),
        time::OffsetDateTime::now_utc(),
    );
}

/// Whether the worker heard words in the turn.
fn has_speech(transcript: &Transcript) -> bool {
    !one_line(&transcript.text).is_empty() && transcript.no_speech_prob <= NO_SPEECH_LIMIT
}

/// Logs a dropped turn. A turn dropped for a busy chief carries the last reason it was busy.
fn log_dropped(result: TurnResult, busy: Option<&Reason>) {
    match busy {
        Some(busy) => info!(
            reason = result.reason(),
            busy_reason = busy.as_str(),
            "spoken prompt dropped"
        ),
        None => info!(reason = result.reason(), "spoken prompt dropped"),
    }
}

/// Logs that a turn is held for a busy chief, and why, on the turn's span.
fn log_held(turn: &tracing::Span, busy: &Reason) {
    turn.record("busy_reason", busy.as_str());
    turn.in_scope(|| info!(reason = busy.as_str(), "spoken prompt held"));
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
                log_typed(&open_turn(ports.call_from()), &pane, &text);
                return;
            }
            Typing::Busy(_) if Instant::now() < gives_up_at => {
                next_try = Instant::now() + BUSY_RETRY;
                continue;
            }
            Typing::Busy(reason) => {
                log_dropped(TurnResult::ChiefBusy, Some(&reason));
                return;
            }
            Typing::NoChief => TurnResult::NoChief,
            Typing::Failed => TurnResult::Failed,
        };
        log_dropped(result, None);
        return;
    }
}

/// What a run that ended after its call did: a hand-over goes to the chief, and the words of
/// a turn whose run failed are typed into its pane when it is free.
fn pass_on_after_call<P: CallPorts>(
    ports: Arc<P>,
    hand_overs: &HandOvers,
    words: String,
    answer: Answer,
    turn: &tracing::Span,
) {
    match answer {
        Answer::Said {
            hand_over: Some(request),
            ..
        } => {
            hand_overs.start(ports, &request, Some(turn));
        }
        Answer::Said { .. } => {}
        Answer::Failed(_) => {
            let now = Instant::now();
            tokio::spawn(pass_on_when_free(ports, words, now, now + BUSY_LIMIT));
        }
    }
}

fn private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::DirBuilder::new().mode(0o700).create(dir)
}

/// Makes `<calls>/call-<uuid>/` for the user only, and writes the tick, the ringback and
/// the pick-up into it.
fn make_call_dir(calls: &Path, dir: &Path) -> std::io::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(calls)?;
    private_dir(dir)?;
    std::fs::write(dir.join(TICK_WAV), tick_wav())?;
    std::fs::write(dir.join(RINGBACK_WAV), ringback_wav())?;
    std::fs::write(dir.join(PICKUP_WAV), pickup_wav())
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
        remove_call_folder(&entry.path());
    }
}

/// Removes a call folder. One that is already gone needs no warning; any other failure does.
pub(super) fn remove_call_folder(dir: &Path) {
    match std::fs::remove_dir_all(dir) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            warn!(error = %error, "call folder not removed");
        }
        _ => {}
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
    Transcribe { dir: PathBuf, queued_at: Instant },
    Make { id: u64, text: String, wav: PathBuf },
}

/// What a sound is, as the voice and play spans name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    ChiefLine,
    /// The desk's turn-end update, said for a chief turn that ended with no line.
    Update,
    Phrase,
    OtherAgent,
    Tick,
    /// One cycle of the phone-call ring, from the tap until Harold picks up.
    Ringback,
    /// The short sound that ends the ringing.
    Pickup,
    /// The call answerer's `say`.
    Answer,
}

impl Source {
    fn as_str(self) -> &'static str {
        match self {
            Self::ChiefLine => "chief_line",
            Self::Update => "update",
            Self::Phrase => "phrase",
            Self::OtherAgent => "other_agent",
            Self::Tick => "tick",
            Self::Ringback => "ringback",
            Self::Pickup => "pickup",
            Self::Answer => "answer",
        }
    }
}

/// The `harold.call_turn` span of the turn being spoken, typed or answered.
struct TurnTrace {
    span: tracing::Span,
    /// When the speech was cut: the clock of `first_sound_ms` and `answer_ms`.
    cut_at: Option<Instant>,
    /// Whether the turn has been typed: only then is a line from the chief its answer.
    typed: bool,
    first_sound_noted: bool,
    answer_noted: bool,
}

impl TurnTrace {
    fn open(call_from: &'static str) -> Self {
        Self {
            span: open_turn(call_from),
            cut_at: None,
            typed: false,
            first_sound_noted: false,
            answer_noted: false,
        }
    }

    /// Notes, once each, the first sound of the turn other than the tick, and the first
    /// that answers it, as milliseconds after the cut.
    fn sound_started(&mut self, source: Source, now: Instant) {
        let Some(cut_at) = self.cut_at else {
            return;
        };
        let ms = millis(now.saturating_duration_since(cut_at));
        if source != Source::Tick && !self.first_sound_noted {
            self.first_sound_noted = true;
            self.span.record("first_sound_ms", ms);
        }
        if matches!(source, Source::ChiefLine | Source::Update | Source::Answer)
            && !self.answer_noted
        {
            self.answer_noted = true;
            self.span.record("answer_ms", ms);
        }
    }
}

/// The `harold.call_speech` span of the speech now heard.
struct SpeechTrace {
    span: tracing::Span,
    /// Samples fed since the block that showed the speech began.
    fed: usize,
}

/// The call answerer's warm-up while the phone rings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WarmUp {
    /// The call has no answerer, or it is not warmed again.
    Off,
    Running,
    /// Ended, this long after the ringing began.
    Done(Duration),
}

/// The ringing: whole cycles of the ringback until the call is ready, then the pick-up.
struct Ring {
    /// The `harold.call_ring` span, from the tap to the pick-up.
    span: tracing::Span,
    started: Instant,
    /// Cycles played.
    cycles: u32,
    /// When the cycle now playing ends, or when to try again; `None` once the pick-up plays.
    next: Option<Instant>,
    /// The pick-up sound now playing.
    pickup: Option<u64>,
    /// Whether the worker was ready at the tap: `ready`, or `loaded_now`.
    worker: &'static str,
    warm: WarmUp,
}

/// The `harold.call_voice` span of a sound the worker makes.
struct VoiceTrace {
    span: tracing::Span,
    /// Until the worker takes the job.
    queued_at: Option<Instant>,
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
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
    source: Source,
    /// The `harold.call_turn` span of the turn it belongs to, which its spans join.
    turn: Option<tracing::Span>,
    voice: Option<VoiceTrace>,
    /// The call answerer's turn that this sound answers: it ends when the sound is let go.
    owned: Option<TurnTrace>,
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
}

/// A turn that waits for the call answerer, with its trace.
struct QueuedTurn {
    text: String,
    trace: TurnTrace,
}

/// The call answerer's run now going.
struct AnswerRun {
    id: u64,
    text: String,
    trace: TurnTrace,
    next_tick: Instant,
    gives_up_at: Instant,
    task: JoinHandle<()>,
    /// Set by talk now: the line is not spoken, but a hand-over still goes.
    silenced: bool,
}

/// A turn that waits for a busy chief.
struct HeldTurn {
    text: String,
    /// Why the chief was busy at the newest try.
    busy: Reason,
    next_try: Instant,
    gives_up_at: Instant,
}

/// How one try at typing a turn ended.
enum Typing {
    Typed(String),
    NoChief,
    Busy(Reason),
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
    /// The ringing, from the tap until the pick-up has played: the screen says connecting.
    ring: Option<Ring>,
    /// The facts the answerer's warm-up read, and when, for the call's first turn.
    warm_facts: Option<(Value, Instant)>,
    /// When the worker became ready.
    ready_at: Option<Instant>,
    /// When the turn at the worker was queued.
    transcribe_queued_at: Option<Instant>,
    /// The speech models, followed while the worker waits for them.
    models_rx: Option<watch::Receiver<ModelsStatus>>,
    /// The speech models the call asked about.
    models: Option<Arc<SpeechModels>>,
    /// Whether the worker has already been started again for a model it could not find.
    models_retried: bool,
    request: Option<JoinHandle<()>>,
    jobs: VecDeque<Job>,
    worker_exits: u32,
    sounds: VecDeque<Sound>,
    playing: Option<Playing>,
    /// When the microphone is heard again after the last sound.
    unmuted_at: Instant,
    awaiting: Option<Awaiting>,
    held: Option<HeldTurn>,
    /// The turn from its speech to its answer played: it ends when the answer has played or
    /// been stopped, or the turn is dropped, forgotten, given up or the call ends.
    turn: Option<TurnTrace>,
    speech: Option<SpeechTrace>,
    /// The `harold.call_transcribe` span of the turn at the worker.
    transcribing: Option<tracing::Span>,
    /// Set by talk now: the chief's lines are taken as said until the next turn is typed.
    skip_chief_lines: bool,
    /// Set by talk now: sounds that wait after talk now do so until the next turn is cut
    /// or typed, or until this time, when no turn has started by then.
    hold_others_until: Option<Instant>,
    /// When the last turn was cut or the last sound played; the call ends `IDLE_LIMIT` after.
    active_at: Instant,
    done: bool,
    /// When the call started, the clock of `first_heard_ms`.
    started_at: Instant,
    /// What the source gave, in samples, in every mode, muted or not.
    heard_samples: u64,
    /// How long after the start the first block came.
    first_heard: Option<Duration>,
    /// Turns cut.
    speech_cut: u64,
    reader: Option<JoinHandle<()>>,
    updates: Vec<JoinHandle<()>>,
    awake: Option<P::Awake>,
    next_id: u64,
    /// The run going when the call ended, with the words it answers.
    detached: Option<(u64, String, tracing::Span)>,
    /// The call answerer, when the daemon has one.
    answering: Option<Arc<CallAnswering>>,
    /// Set when the answerer's program could not be started: the call stops trying it.
    answerer_gone: bool,
    run: Option<AnswerRun>,
    /// Turns heard while a run goes, or while a turn for the chief is awaited.
    queued: VecDeque<QueuedTurn>,
    /// What was said in this call, for the answerer's next turn.
    remarks: Vec<Remark>,
    hand_overs: HandOvers,
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
        // A call asks the speech models first: one held back or failed starts its download
        // now. The worker starts once they are all on the Mac.
        let models = ports.speech_models();
        if let Some(models) = &models {
            models.for_call();
        }
        let models_rx = models.as_ref().map(|models| models.status());
        let models_ready = models_rx
            .as_ref()
            .is_none_or(|status| *status.borrow() == ModelsStatus::Ready);
        let worker = if models_ready {
            match ports.start_worker(&dir) {
                Ok(worker) => Some(worker),
                Err(error) => {
                    log_exited(error);
                    not_opened.close(started, Ended::VoiceFailed).await;
                    return None;
                }
            }
        } else {
            None
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
            answering: ports.answering(),
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
            ring: None,
            warm_facts: None,
            ready_at: None,
            transcribe_queued_at: None,
            models_rx: None,
            models,
            models_retried: false,
            request: None,
            jobs: VecDeque::new(),
            worker_exits: 0,
            sounds: VecDeque::new(),
            playing: None,
            unmuted_at: Instant::now(),
            awaiting: None,
            held: None,
            turn: None,
            speech: None,
            transcribing: None,
            skip_chief_lines: false,
            hold_others_until: None,
            active_at: Instant::now(),
            done: false,
            started_at: Instant::now(),
            heard_samples: 0,
            first_heard: None,
            speech_cut: 0,
            reader: Some(reader),
            updates: Vec::new(),
            awake,
            next_id: 0,
            detached: None,
            answerer_gone: false,
            run: None,
            queued: VecDeque::new(),
            remarks: Vec::new(),
            hand_overs: HandOvers::default(),
        };
        match worker {
            Some(worker) => {
                call.watch_worker(&worker);
                call.worker = Some(worker);
            }
            None => {
                // Look at the models now: one that has already failed ends the call.
                let mut status = models_rx;
                if let Some(status) = status.as_mut() {
                    status.mark_changed();
                }
                call.models_rx = status;
            }
        }
        if call.worker_ready {
            call.became_ready();
        }
        // The ringing is when the call starts what it needs: the worker is started above,
        // and the answerer warms up now.
        let warm = call.start_warm_up();
        call.begin_ring(warm);
        call.publish();
        call.ports.call_opened();
        started.send_replace(true);
        Some(call)
    }

    async fn run(&mut self, inbox: &mut mpsc::UnboundedReceiver<Event<P::Worker>>) {
        while !self.done {
            self.next_answer().await;
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
                status = models_change(&mut self.models_rx), if self.models_rx.is_some() => {
                    self.models_changed(status).await;
                }
            }
        }
        // A run that ended while the call closed down sent its answer to an inbox no one
        // reads: from here on a send fails and the run passes its answer on itself, and
        // what already arrived is passed on now.
        inbox.close();
        while let Ok(event) = inbox.try_recv() {
            let Event::Answered { id, answer } = event else {
                continue;
            };
            if self.detached.as_ref().is_some_and(|(run, ..)| *run == id)
                && let Some((_, words, turn)) = self.detached.take()
            {
                pass_on_after_call(
                    Arc::clone(&self.ports),
                    &self.hand_overs,
                    words,
                    answer,
                    &turn,
                );
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
            Event::Say { line, by, reply } => self.line(line, by, Some(reply), false),
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
            Event::Warmed(facts) => self.warmed(facts),
            Event::TurnEndUpdate(line) => self.turn_end_update(line),
            Event::Answered { id, answer } => self.answered(id, answer).await,
        }
    }

    // --- the samples ---

    /// A block recorded while a sound played, or within the tail after, is not heard, even
    /// when it is read later.
    fn heard_block(&mut self, block: &[i16], captured_at: Instant) {
        self.heard_samples += block.len() as u64;
        if self.first_heard.is_none() {
            self.first_heard = Some(self.started_at.elapsed());
        }
        if self.mode != Mode::Listening {
            self.detector.reset();
            return;
        }
        // The ringing is sound like any other: the user is not heard before the pick-up.
        let muted = self.playing.is_some() || self.ring.is_some() || captured_at < self.unmuted_at;
        let cut = self.detector.push(block, muted).into_iter().next();
        if let Some(speech) = self.speech.as_mut() {
            speech.fed += block.len();
        } else if cut.is_some() || self.detector.in_turn() {
            self.speech_started(block.len());
        }
        if let Some(turn) = cut {
            self.turn_ended(&turn);
        }
    }

    /// Opens the turn's `harold.call_turn` span, or goes on with the one a busy chief holds,
    /// and the `harold.call_speech` span in it.
    fn speech_started(&mut self, fed: usize) {
        let ports = &self.ports;
        let turn = self
            .turn
            .get_or_insert_with(|| TurnTrace::open(ports.call_from()));
        let span = tracing::info_span!(
            target: telemetry::TRACE_TARGET,
            parent: &turn.span,
            "harold.call_speech",
            cut = tracing::field::Empty
        );
        self.speech = Some(SpeechTrace { span, fed });
    }

    /// Ends the turn's `harold.call_turn` span, with `result` unless it has one.
    fn end_turn(&mut self, result: Option<&str>) {
        if let (Some(turn), Some(result)) = (self.turn.take(), result) {
            turn.span.record("result", result);
            turn.span.record("answered_by", "none");
        }
    }

    /// Forgets the speech of a turn not yet ended, keeping the background level. A newer
    /// phone screen that takes the call over means the user is starting again: joining the
    /// older screen's half of a sentence to the newer's could type a garbled line, with Enter,
    /// into the chief's pane. Only a turn being spoken while listening has anything to forget.
    fn forget_turn(&mut self) {
        if self.mode == Mode::Listening {
            self.detector.reset();
            if self.speech.take().is_some() && self.held.is_none() {
                self.end_turn(Some("forgotten"));
            }
        }
    }

    /// Writes the turn to `<call dir>/turn-<uuid>/turn.wav` for the worker to transcribe.
    fn turn_ended(&mut self, samples: &[i16]) {
        self.speech_cut += 1;
        self.mode = Mode::Transcribing;
        self.detector.reset();
        self.hold_others_until = None;
        self.active_at = Instant::now();
        if let Some(speech) = self.speech.take() {
            let long = speech.fed >= (MAX_TURN_FRAMES - START_FRAMES) * FRAME;
            speech
                .span
                .record("cut", if long { "max_length" } else { "quiet" });
        }
        if let Some(turn) = self.turn.as_mut() {
            turn.cut_at = Some(Instant::now());
        }
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
        let audio_ms = samples.len() as u64 * 1000 / u64::from(SAMPLE_RATE);
        self.transcribing = Some(tracing::info_span!(
            target: telemetry::TRACE_TARGET,
            parent: self.turn.as_ref().and_then(|turn| turn.span.id()),
            "harold.call_transcribe",
            audio_ms,
            waited_ms = tracing::field::Empty,
            load_waited_ms = tracing::field::Empty,
            language = tracing::field::Empty,
            result = tracing::field::Empty
        ));
        self.transcribe_queued_at = Some(Instant::now());
        self.jobs.push_back(Job::Transcribe {
            dir,
            queued_at: Instant::now(),
        });
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
        self.note_wait(&job);
        let events = self.events.clone();
        let generation = self.generation;
        self.request = Some(tokio::spawn(async move {
            let event = match job {
                Job::Transcribe { dir, .. } => {
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

    /// Notes on the job's span how long the job waited for the worker.
    fn note_wait(&mut self, job: &Job) {
        let now = Instant::now();
        match job {
            Job::Transcribe { queued_at, .. } => {
                if let Some(span) = &self.transcribing {
                    span.record(
                        "waited_ms",
                        millis(now.saturating_duration_since(*queued_at)),
                    );
                }
            }
            Job::Make { id, .. } => {
                let voice = self
                    .sounds
                    .iter_mut()
                    .find(|sound| sound.id == *id)
                    .and_then(|sound| sound.voice.as_mut());
                if let Some(voice) = voice
                    && let Some(queued_at) = voice.queued_at.take()
                {
                    let waited = millis(now.saturating_duration_since(queued_at));
                    voice.span.record("waited_ms", waited);
                }
            }
        }
    }

    async fn transcribed(
        &mut self,
        generation: u64,
        result: Result<Transcript, WorkerError>,
        worker: P::Worker,
    ) {
        self.request = None;
        let span = self.transcribing.take();
        if let (Some(span), Some(queued_at)) = (&span, self.transcribe_queued_at.take()) {
            // How long the turn waited for the worker to finish loading: none when the worker
            // was ready before the turn was cut.
            // A worker that answered was ready, whether or not its health has been read yet.
            let ready = *self.ready_at.get_or_insert_with(Instant::now);
            let waited = ready.saturating_duration_since(queued_at);
            span.record("load_waited_ms", millis(waited));
        }
        let error = match result {
            Ok(transcript) => {
                if let Some(span) = span {
                    span.record("language", transcript.language.as_str());
                    let result = if has_speech(&transcript) {
                        "text"
                    } else {
                        "no_speech"
                    };
                    span.record("result", result);
                }
                self.worker_back(generation, worker).await;
                self.heard(transcript).await;
                return;
            }
            Err(error) => error,
        };
        if let Some(span) = span {
            span.record("result", "failed");
        }
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
                if let Some(voice) = at.and_then(|at| self.sounds[at].voice.take()) {
                    voice.span.record("audio_ms", millis(sound_length(&wav)));
                    voice.span.record("result", "made");
                }
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
                if let Some(voice) = at.and_then(|at| self.sounds[at].voice.take()) {
                    voice.span.record("result", "failed");
                }
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
            Health::Ready => self.became_ready(),
            Health::Gone(error) => self.worker_died(self.generation, error).await,
        }
    }

    /// The worker is ready. The ringing ends at the end of its cycle, once the warm-up has
    /// too.
    fn became_ready(&mut self) {
        self.worker_ready = true;
        self.ready_at.get_or_insert_with(Instant::now);
        let Some(worker) = self.worker.as_ref() else {
            return;
        };
        if let Some(started_by) = worker.ready_log_due() {
            let load_ms = worker.load_ms();
            info!(load_ms, started_by, "voice worker ready");
        }
    }

    /// The speech models changed while the worker waits for them: the worker starts when they
    /// are all on the Mac, and the call ends when one will not come.
    async fn models_changed(&mut self, status: ModelsStatus) {
        match status {
            ModelsStatus::Ready if self.worker.is_none() => self.start_worker_again().await,
            ModelsStatus::Ready => self.models_rx = None,
            ModelsStatus::Failed(missing) => {
                self.models_rx = None;
                self.ports.model_missing(&missing);
                warn!(model = %missing.model, reason = missing.reason, "call ended for a speech model");
                self.end_turn(Some("voice_failed"));
                self.end(Ended::VoiceFailed, Some(MODEL_MISSING)).await;
            }
            ModelsStatus::Checking | ModelsStatus::Waiting | ModelsStatus::Downloading { .. } => {}
        }
    }

    /// Starts the worker the call was waiting for the speech models to start.
    async fn start_worker_again(&mut self) {
        self.models_rx = None;
        match self.ports.start_worker(&self.dir) {
            Ok(worker) => {
                self.watch_worker(&worker);
                self.worker = Some(worker);
                if self.worker_ready {
                    self.became_ready();
                }
            }
            Err(error) => {
                log_exited(error);
                self.end(Ended::VoiceFailed, Some(VOICE_FAILED)).await;
            }
        }
    }

    /// The worker could not find `model`: the models look at it again, and the worker starts
    /// again when they are done. Once for each call.
    fn wait_for_model(&mut self, model: &str) {
        let Some(models) = self.models.clone() else {
            return;
        };
        self.models_retried = true;
        models.model_missing(model, true);
        let mut status = models.status();
        status.mark_changed();
        self.models_rx = Some(status);
        self.ready_at = None;
        self.begin_ring(WarmUp::Off);
    }

    /// The worker of `generation` has gone, seen through its health or a request; each
    /// death counts once. A worker that never loaded, a second death, or a restart that
    /// fails ends the call. Otherwise the worker is started once more, and requests go to
    /// it from now on.
    async fn worker_died(&mut self, generation: u64, error: WorkerError) {
        if self.done || generation != self.generation {
            return;
        }
        log_exited(error);
        if self.worker_ready {
            info!(reason = "exited", "voice worker stopped");
        }
        self.generation += 1;
        self.health = None;
        if let Some(worker) = self.worker.take() {
            worker.close().await;
        }
        // A speech model the worker could not find may be on its way: look again, once.
        if let (Some(model), false) = (error.model(), self.models_retried)
            && error.cause() == Some(LoadCause::ModelMissing)
            && self.models.is_some()
        {
            self.wait_for_model(model);
            return;
        }
        if let (Some(model), Some(models)) = (error.model(), self.models.as_ref())
            && error.cause() == Some(LoadCause::ModelMissing)
        {
            // Still missing after the models were looked at again.
            let missing = models.missing(model, "failed");
            self.ports.model_missing(&missing);
            if self.mode == Mode::Transcribing {
                self.dropped(TurnResult::Failed);
            }
            self.end(Ended::VoiceFailed, Some(MODEL_MISSING)).await;
            return;
        }
        self.worker_exits += 1;
        let restarted = if error.is_load_failure() || !self.worker_ready || self.worker_exits > 1 {
            None
        } else {
            match self.ports.start_worker(&self.dir) {
                Ok(worker) => Some(worker),
                Err(error) => {
                    log_exited(error);
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
            source: Source::Phrase,
            turn: None,
            voice: None,
            owned: None,
        });
    }

    // --- a turn ---

    async fn heard(&mut self, transcript: Transcript) {
        let text = one_line(&transcript.text);
        if !has_speech(&transcript) {
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

    /// Gives the turn to the call answerer, or types it, or joins it to the turn waiting for
    /// a busy chief (Decision 9).
    async fn turn(&mut self, text: String) {
        self.mode = Mode::Listening;
        if let Some(held) = self.held.as_mut() {
            held.text.push(' ');
            held.text.push_str(&text);
            return;
        }
        if self.answerer_usable() {
            let ports = &self.ports;
            let trace = self
                .turn
                .take()
                .unwrap_or_else(|| TurnTrace::open(ports.call_from()));
            self.queued.push_back(QueuedTurn { text, trace });
            return;
        }
        self.turn_to_chief(text).await;
    }

    /// Types the turn into the chief's pane, as every turn was before the call answerer.
    async fn turn_to_chief(&mut self, text: String) {
        match try_type(&*self.ports, &text).await {
            Typing::Typed(pane) => self.typed(pane, text),
            Typing::NoChief => self.no_chief(),
            Typing::Busy(busy) => {
                self.log_held(&busy);
                let now = Instant::now();
                self.held = Some(HeldTurn {
                    text,
                    busy,
                    next_try: now + BUSY_RETRY,
                    gives_up_at: now + BUSY_LIMIT,
                });
                self.phrase(CHIEF_BUSY, After::Nothing);
            }
            Typing::Failed => self.dropped(TurnResult::Failed),
        }
    }

    // --- the call answerer ---

    fn answerer_usable(&self) -> bool {
        self.answering.is_some() && !self.answerer_gone
    }

    /// Notes what was said in this call, for the answerer's next turn.
    fn remark(&mut self, who: Who, text: &str) {
        if self.answering.is_none() {
            return;
        }
        if self.remarks.len() >= REMARKS_KEPT {
            self.remarks.remove(0);
        }
        self.remarks.push(Remark {
            who,
            text: text.to_string(),
        });
    }

    /// Starts a run for the first turn waiting, when none goes, the user is not speaking
    /// and no turn waits for a busy chief. Turns waiting when the answerer is gone go to the
    /// chief.
    async fn next_answer(&mut self) {
        if self.done || self.run.is_some() || self.mode != Mode::Listening || self.held.is_some() {
            return;
        }
        if self.answerer_gone && !self.queued.is_empty() {
            if let Some(queued) = self.queued.pop_front() {
                self.fall_back(queued.text, queued.trace).await;
            }
            return;
        }
        let Some(answering) = self.answering.clone().filter(|_| !self.answerer_gone) else {
            return;
        };
        let Some(queued) = self.queued.pop_front() else {
            return;
        };
        let id = self.new_id();
        let call = CallSoFar {
            remarks: self.remarks.clone(),
            hand_overs: self.hand_overs.facts(),
        };
        self.remark(Who::You, &queued.text);
        let words = queued.text.clone();
        let span = queued.trace.span.clone();
        // The first turn within `WARM_KEPT` of the warm-up uses the facts it read.
        let facts = self
            .warm_facts
            .take()
            .filter(|(_, at)| at.elapsed() <= WARM_KEPT)
            .map(|(facts, _)| facts);
        let events = self.events.clone();
        let ports = Arc::clone(&self.ports);
        let hand_overs = self.hand_overs.clone();
        let task = tokio::spawn(async move {
            let ask = Ask {
                words: &words,
                call: &call,
                turn: &span,
                facts,
            };
            let answer = answering.answerer.answer(ask).await;
            // The call has ended: the request still goes to the chief, as a turn said
            // before a bye does.
            if let Err(mpsc::error::SendError(Event::Answered { answer, .. })) =
                events.send(Event::Answered { id, answer })
            {
                pass_on_after_call(ports, &hand_overs, words, answer, &span);
            }
        });
        let now = Instant::now();
        self.active_at = now;
        self.run = Some(AnswerRun {
            id,
            text: queued.text,
            trace: queued.trace,
            next_tick: now + TICK_EVERY,
            gives_up_at: now + RUN_LIMIT,
            task,
            silenced: false,
        });
    }

    /// Hands the turns waiting for the answerer to the chief in the background, as a turn
    /// said before a bye is.
    fn pass_on_queued(&mut self) {
        for queued in std::mem::take(&mut self.queued) {
            tokio::spawn(pass_on_when_free(
                Arc::clone(&self.ports),
                queued.text,
                Instant::now(),
                Instant::now() + BUSY_LIMIT,
            ));
        }
    }

    /// A run ended. Its line is spoken and its hand-over started, or the turn goes to the
    /// chief.
    async fn answered(&mut self, id: u64, answer: Answer) {
        let Some(mut run) = self.run.take_if(|run| run.id == id) else {
            return;
        };
        if self.done {
            return;
        }
        // A bye was said while the run went: its hand-over still goes, its line does not.
        run.silenced |= self.mode == Mode::Ending;
        match answer {
            Answer::Said { say, hand_over } => self.run_said(run, say, hand_over),
            Answer::Failed(failure) => {
                if failure == Failure::NotStarted {
                    self.answerer_gone = true;
                }
                if self.mode == Mode::Ending {
                    // A bye was said: the words go to the chief in the background.
                    pass_on_after_call(
                        Arc::clone(&self.ports),
                        &self.hand_overs,
                        run.text,
                        Answer::Failed(failure),
                        &run.trace.span,
                    );
                } else {
                    self.fall_back(run.text, run.trace).await;
                }
            }
        }
    }

    fn run_said(&mut self, run: AnswerRun, say: String, hand_over: Option<String>) {
        let handed_over = hand_over.is_some();
        if let Some(request) = &hand_over {
            self.hand_overs
                .start(Arc::clone(&self.ports), request, Some(&run.trace.span));
        }
        let say = if say.trim().is_empty() {
            PASSED_ON.to_string()
        } else {
            one_line(&say)
        };
        if !run.silenced {
            self.remark(Who::Answerer, &say);
        }
        let result = if handed_over {
            TurnResult::HandedOver
        } else {
            TurnResult::Answered
        };
        run.trace.span.record("result", result.reason());
        run.trace.span.record("answered_by", "answerer");
        {
            let mut status = lock(&self.status);
            status.last_result = Some(result);
            status.last_transcript = Some(run.text);
        }
        self.active_at = Instant::now();
        if !run.silenced {
            self.say_answer(say, run.trace);
        }
    }

    /// A run that gave no answer: the turn goes to the chief as it would without the
    /// answerer. When the user has moved on to another turn, it is passed on in the
    /// background instead, as a turn said before a bye is.
    async fn fall_back(&mut self, text: String, trace: TurnTrace) {
        let moved_on = self.turn.is_some()
            || !self.queued.is_empty()
            || self.held.is_some()
            || self.mode != Mode::Listening;
        if moved_on {
            let gives_up_at = Instant::now() + BUSY_LIMIT;
            tokio::spawn(pass_on_when_free(
                Arc::clone(&self.ports),
                text,
                Instant::now(),
                gives_up_at,
            ));
            return;
        }
        self.turn = Some(trace);
        self.turn_to_chief(text).await;
    }

    /// Queues the answerer's `say` to be made into audio and played. The turn ends when the
    /// sound is let go.
    fn say_answer(&mut self, text: String, trace: TurnTrace) {
        let id = self.new_id();
        let wav = self.dir.join(format!("answer-{}.wav", Uuid::new_v4()));
        let span = trace.span.clone();
        let voice = self.voice_trace_in(span.id(), Source::Answer);
        self.jobs.push_back(Job::Make {
            id,
            text: text.clone(),
            wav: wav.clone(),
        });
        self.sounds.push_back(Sound {
            id,
            text,
            audio: Audio::Wav {
                path: wav,
                ready: false,
                keep_as: None,
                remove: true,
            },
            by: None,
            answers: false,
            waits_after_talk_now: false,
            after: After::Nothing,
            reply: None,
            source: Source::Answer,
            turn: Some(span),
            voice: Some(voice),
            owned: Some(trace),
        });
    }

    /// The turn said before a bye, and then the call ends. A busy chief gets the turn as
    /// it would during the call, until `gives_up_at`, from a task that outlives the call and
    /// holds no audio.
    async fn last_turn(&mut self, text: String, gives_up_at: Instant) {
        match try_type(&*self.ports, &text).await {
            Typing::Typed(pane) => self.typed(pane, text),
            Typing::NoChief => self.dropped(TurnResult::NoChief),
            Typing::Busy(busy) => {
                self.log_held(&busy);
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

    /// Logs that the turn is held for a busy chief, on the turn's span.
    fn log_held(&mut self, busy: &Reason) {
        let ports = &self.ports;
        let turn = self
            .turn
            .get_or_insert_with(|| TurnTrace::open(ports.call_from()));
        log_held(&turn.span, busy);
    }

    fn typed(&mut self, pane: String, text: String) {
        let ports = &self.ports;
        let turn = self
            .turn
            .get_or_insert_with(|| TurnTrace::open(ports.call_from()));
        turn.typed = true;
        turn.span.record("answered_by", "chief");
        log_typed(&turn.span, &pane, &text);
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
        });
    }

    /// A turn that came to nothing. A turn waiting for a busy chief goes on.
    fn dropped(&mut self, result: TurnResult) {
        self.dropped_busy(result, None);
    }

    /// The same, for a turn dropped for a busy chief with the reason it was last busy.
    fn dropped_busy(&mut self, result: TurnResult, busy: Option<&Reason>) {
        log_dropped(result, busy);
        lock(&self.status).last_result = Some(result);
        if self.held.is_none() {
            self.end_turn(Some(result.reason()));
        }
    }

    /// Drops a turn still waiting for a busy chief.
    fn drop_held(&mut self) {
        if let Some(held) = self.held.take() {
            self.dropped_busy(TurnResult::ChiefBusy, Some(&held.busy));
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
        self.pass_on_queued();
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
        let running = self
            .run
            .iter()
            .flat_map(|run| [run.next_tick, run.gives_up_at]);
        let idle = std::iter::once(self.active_at + IDLE_LIMIT);
        held.chain(awaiting)
            .chain(running)
            .chain(self.ring.iter().filter_map(|ring| ring.next))
            .chain(self.hold_others_until)
            .chain(idle)
            .min()
    }

    async fn on_timer(&mut self) {
        let now = Instant::now();
        if let Some(held) = &self.held {
            if now >= held.gives_up_at {
                let busy = held.busy.clone();
                self.held = None;
                self.dropped_busy(TurnResult::ChiefBusy, Some(&busy));
            } else if now >= held.next_try {
                self.try_held(now).await;
            }
        }
        self.await_answer(now);
        self.await_run(now).await;
        if self
            .ring
            .as_ref()
            .and_then(|ring| ring.next)
            .is_some_and(|due| now >= due)
        {
            self.ring_cycle_ended(now);
        }
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
        let quiet = self.ring.is_none()
            && self.mode == Mode::Listening
            && self.run.is_none()
            && self.queued.is_empty()
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
        self.end_turn(Some("voice_muted"));
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
            Typing::Busy(busy) => {
                if let Some(held) = self.held.as_mut() {
                    held.next_try = Instant::now() + BUSY_RETRY;
                    if let Some(turn) = self.turn.as_ref() {
                        turn.span.record("busy_reason", busy.as_str());
                    }
                    held.busy = busy;
                }
            }
            Typing::Failed => {
                self.held = None;
                self.dropped(TurnResult::Failed);
            }
        }
    }

    /// The tick while a run of the call answerer goes, and the end of a run that never
    /// reports.
    async fn await_run(&mut self, now: Instant) {
        let Some(run) = self.run.as_mut() else {
            return;
        };
        if now >= run.gives_up_at {
            run.task.abort();
            let id = run.id;
            warn!(reason = Failure::TimedOut.as_str(), "call answer failed");
            self.answered(id, Answer::Failed(Failure::TimedOut)).await;
            return;
        }
        if now >= run.next_tick {
            run.next_tick += TICK_EVERY;
            // The user speaking is not talked over.
            if self.mode == Mode::Listening && !self.detector.in_turn() {
                self.play_tick();
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
            self.end_turn(None);
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
            let turn = self
                .turn
                .as_ref()
                .map_or_else(tracing::Span::none, |turn| turn.span.clone());
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
    fn line(
        &mut self,
        line: String,
        by: Speaker,
        reply: Option<oneshot::Sender<bool>>,
        update: bool,
    ) {
        if self.mode == Mode::Ending {
            if let Some(reply) = reply {
                let _ = reply.send(false);
            }
            return;
        }
        if by == Speaker::Chief {
            self.remark(Who::Chief, &line);
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
        // A line the chief says between turns of a call the call answerer answers, such as
        // a hand-over's report, is told apart from the answerer's own by a phrase first.
        if by == Speaker::Chief && !answers && self.answering.is_some() {
            self.phrase(FROM_THE_CHIEF, After::Nothing);
            if let Some(preface) = self.sounds.back_mut() {
                preface.by = Some(Speaker::Chief);
            }
        }
        let id = self.new_id();
        let wav = self.dir.join(format!("answer-{}.wav", Uuid::new_v4()));
        let source = match (by, update) {
            (Speaker::OtherAgent, _) => Source::OtherAgent,
            (Speaker::Chief, true) => Source::Update,
            (Speaker::Chief, false) => Source::ChiefLine,
        };
        let voice = self.voice_trace(source);
        let turn = self.turn_span(source);
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
            source,
            turn,
            voice: Some(voice),
            owned: None,
        });
    }

    /// The span of the turn a sound belongs to: a line another agent says is not part of it.
    fn turn_span(&self, source: Source) -> Option<tracing::Span> {
        let answer = matches!(source, Source::ChiefLine | Source::Update);
        self.turn
            .as_ref()
            .filter(|turn| turn.typed || !answer)
            .filter(|_| source != Source::OtherAgent)
            .map(|turn| turn.span.clone())
    }

    /// Opens the `harold.call_voice` span of a sound queued for the worker.
    fn voice_trace(&self, source: Source) -> VoiceTrace {
        let parent = self.turn_span(source).and_then(|turn| turn.id());
        self.voice_trace_in(parent, source)
    }

    /// The same, joining the span `parent`.
    fn voice_trace_in(&self, parent: Option<tracing::span::Id>, source: Source) -> VoiceTrace {
        let span = tracing::info_span!(
            target: telemetry::TRACE_TARGET,
            parent: parent,
            "harold.call_voice",
            source = source.as_str(),
            waited_ms = tracing::field::Empty,
            audio_ms = tracing::field::Empty,
            result = tracing::field::Empty
        );
        VoiceTrace {
            span,
            queued_at: Some(Instant::now()),
        }
    }

    /// The chief's turn ended with no line: the desk's turn-end update answers it.
    fn turn_end_update(&mut self, line: String) {
        let Some(awaiting) = self.awaiting.as_mut() else {
            return;
        };
        if !awaiting.update_asked || awaiting.answered {
            return;
        }
        self.line(line, Speaker::Chief, None, true);
    }

    /// One of Harold's phrases, from the phrase folder, or made by the worker and kept
    /// there for later calls (Decision 8).
    fn phrase(&mut self, text: &'static str, after: After) {
        let kept = phrase_file(&self.ports.phrases_dir(), &self.ports.voice_name(), text);
        let id = self.new_id();
        let mut voice = None;
        let turn = self.turn_span(Source::Phrase);
        let audio = if kept.is_file() {
            Audio::Wav {
                path: kept,
                ready: true,
                keep_as: None,
                remove: false,
            }
        } else {
            let wav = self.dir.join(format!("phrase-{}.wav", Uuid::new_v4()));
            voice = Some(self.voice_trace(Source::Phrase));
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
            source: Source::Phrase,
            turn,
            voice,
            owned: None,
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
        let Some(mut sound) = self.sounds.pop_front() else {
            return;
        };
        if sound.answers && self.awaiting.take().is_some() && self.mode == Mode::Waiting {
            self.mode = Mode::Listening;
        }
        let by_tts = matches!(sound.audio, Audio::Tts);
        let span = self.play_span(sound.source, sound.turn.as_ref());
        match sound.owned.as_mut() {
            Some(trace) => trace.sound_started(sound.source, Instant::now()),
            None => self.note_started(&sound),
        }
        let task = match &sound.audio {
            Audio::Wav { path, .. } => self.spawn_play(sound.id, path.clone(), sound.said(), span),
            Audio::Tts => self.spawn_tts(sound.id, sound.text.clone(), span),
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
        let turn = self
            .turn
            .as_ref()
            .map(|turn| turn.span.clone())
            .or_else(|| self.run.as_ref().map(|run| run.trace.span.clone()));
        let span = self.play_span(Source::Tick, turn.as_ref());
        let task = self.spawn_play(id, self.dir.join(TICK_WAV), Said::Tick, span);
        self.playing = Some(Playing {
            id,
            task,
            by_tts: false,
            stopped: false,
            sound: None,
        });
    }

    /// Starts the ringing: the ringback plays from the next turn of the loop.
    fn begin_ring(&mut self, warm: WarmUp) {
        let now = Instant::now();
        // A ring going on carries on: it is one span, from the tap to the pick-up.
        if let Some(ring) = self.ring.as_mut() {
            ring.next = ring.next.or(Some(now));
            return;
        }
        let span = tracing::info_span!(
            target: telemetry::TRACE_TARGET,
            parent: None,
            "harold.call_ring",
            cycles = tracing::field::Empty,
            worker = tracing::field::Empty,
            warm_ms = tracing::field::Empty,
        );
        self.ring = Some(Ring {
            span,
            started: now,
            cycles: 0,
            next: Some(now),
            pickup: None,
            worker: if self.worker_ready {
                "ready"
            } else {
                "loaded_now"
            },
            warm,
        });
    }

    /// Has the call answerer read the call facts and start its program, which takes no
    /// model call, while the phone rings; the limit cuts it. Gives its state.
    fn start_warm_up(&self) -> WarmUp {
        let Some(answering) = self.answering.clone().filter(|_| !self.answerer_gone) else {
            return WarmUp::Off;
        };
        let events = self.events.clone();
        tokio::spawn(async move {
            let facts = tokio::time::timeout(WARM_LIMIT, answering.answerer.warm())
                .await
                .ok()
                .flatten();
            let _ = events.send(Event::Warmed(facts));
        });
        WarmUp::Running
    }

    /// The warm-up ended: its facts are kept for the first turn.
    fn warmed(&mut self, facts: Option<Value>) {
        let now = Instant::now();
        if let Some(ring) = self.ring.as_mut()
            && ring.warm == WarmUp::Running
        {
            ring.warm = WarmUp::Done(now.saturating_duration_since(ring.started));
        }
        self.warm_facts = facts.map(|facts| (facts, now));
    }

    /// A cycle of the ringback ended, or the call was to try again: if the call is ready,
    /// the pick-up plays; if not, another whole cycle does. The first cycle always plays.
    fn ring_cycle_ended(&mut self, now: Instant) {
        let Some(ring) = self.ring.as_ref() else {
            return;
        };
        let cycles = ring.cycles;
        // The ring waits for a sound still playing or in line, and the user mid-turn.
        let busy = self.playing.is_some()
            || !self.sounds.is_empty()
            || (self.mode == Mode::Listening && self.detector.in_turn());
        // The warm-up's own limit ends it; this one is for a task that never reported.
        let warmed = ring.warm != WarmUp::Running || ring.started.elapsed() >= WARM_LIMIT * 2;
        let ready = self.worker_ready && warmed;
        let (wav, said_source) = if cycles > 0 && ready {
            (PICKUP_WAV, Source::Pickup)
        } else {
            (RINGBACK_WAV, Source::Ringback)
        };
        if busy {
            if let Some(ring) = self.ring.as_mut() {
                ring.next = Some(now + Duration::from_millis(20));
            }
            return;
        }
        let id = self.new_id();
        let span = self.play_span(said_source, None);
        let task = self.spawn_play(id, self.dir.join(wav), Said::Tick, span);
        self.playing = Some(Playing {
            id,
            task,
            by_tts: false,
            stopped: false,
            sound: None,
        });
        let Some(ring) = self.ring.as_mut() else {
            return;
        };
        if said_source == Source::Pickup {
            ring.pickup = Some(id);
            ring.next = None;
        } else {
            ring.cycles += 1;
            ring.next = Some(now + RING_CYCLE);
        }
    }

    /// The pick-up has played: the ringing ends and the call listens.
    fn pickup_played(&mut self) {
        let Some(ring) = self.ring.take() else {
            return;
        };
        ring.span.record("cycles", u64::from(ring.cycles));
        ring.span.record("worker", ring.worker);
        if let WarmUp::Done(took) = ring.warm {
            ring.span.record("warm_ms", millis(took));
        }
        self.active_at = Instant::now();
    }

    /// Opens the `harold.call_play` span of a sound, which the task playing it runs in.
    fn play_span(&self, source: Source, turn: Option<&tracing::Span>) -> tracing::Span {
        tracing::info_span!(
            target: telemetry::TRACE_TARGET,
            parent: turn.and_then(tracing::Span::id),
            "harold.call_play",
            source = source.as_str(),
            call_from = self.ports.call_from(),
            start_ms = tracing::field::Empty
        )
    }

    /// Notes on the turn the sound belongs to when its sound started.
    fn note_started(&mut self, sound: &Sound) {
        let Some(own) = sound.turn.as_ref().and_then(tracing::Span::id) else {
            return;
        };
        if let Some(turn) = self
            .turn
            .as_mut()
            .filter(|turn| turn.span.id() == Some(own))
        {
            turn.sound_started(sound.source, Instant::now());
        }
    }

    fn spawn_play(&self, id: u64, wav: PathBuf, said: Said, span: tracing::Span) -> JoinHandle<()> {
        let sink = Arc::clone(&self.sink);
        let events = self.events.clone();
        tokio::spawn(
            async move {
                let played = match sink.play_said(&wav, &said).await {
                    Ok(()) => true,
                    Err(error) => {
                        warn!(error = %error, "call sound not played");
                        false
                    }
                };
                let _ = events.send(Event::Played { id, played });
            }
            .instrument(span),
        )
    }

    fn spawn_tts(&self, id: u64, text: String, span: tracing::Span) -> JoinHandle<()> {
        let ports = Arc::clone(&self.ports);
        let events = self.events.clone();
        tokio::spawn(
            async move {
                ports.speak_with_tts(&text).await;
                let _ = events.send(Event::Played { id, played: true });
            }
            .instrument(span),
        )
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
        if self.ring.as_ref().and_then(|ring| ring.pickup) == Some(id) {
            self.pickup_played();
        }
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
        // Only the turn the sound belongs to ends: a newer turn may have started since.
        let own = sound.turn.as_ref().and_then(tracing::Span::id);
        if sound.answers && own.is_some() && self.turn.as_ref().and_then(|t| t.span.id()) == own {
            self.end_turn(None);
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
        if self.awaiting.take().is_some() {
            self.end_turn(None);
        }
        if self.mode == Mode::Waiting {
            self.mode = Mode::Listening;
        }
        // The user is starting again: a run going still hands over its request, but its
        // line is not spoken.
        if let Some(run) = self.run.as_mut() {
            run.silenced = true;
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
            Mode::Listening | Mode::Transcribing if self.ring.is_some() => CallState::Connecting,
            Mode::Listening => CallState::Listening,
            Mode::Transcribing => CallState::Transcribing,
            Mode::Waiting => CallState::Waiting,
            Mode::Ending => CallState::Speaking,
        };
        let percent = self
            .models_rx
            .as_ref()
            .and_then(|status| status.borrow().percent());
        let mut status = lock(&self.status);
        status.state = state;
        status.model_percent = percent.filter(|_| self.ring.is_some());
    }

    /// Logs the end of a call that listened, with how much sound it read and how loud.
    fn log_ended(&self, why: Ended) {
        let tally = self.detector.tally();
        let heard_ms = self.heard_samples * 1000 / u64::from(SAMPLE_RATE);
        let first_heard_ms = self.first_heard.map_or(0, millis);
        let loudest = tally.loudest.round() as u64;
        let (reason, loud_frames, speech_cut) =
            (why.as_str(), tally.loud_frames as u64, self.speech_cut);
        match self.detector.background() {
            Some(background) => info!(
                reason,
                heard_ms,
                first_heard_ms,
                background = background.round() as u64,
                loudest,
                loud_frames,
                speech_cut,
                "call ended"
            ),
            None => info!(
                reason,
                heard_ms, first_heard_ms, loudest, loud_frames, speech_cut, "call ended"
            ),
        }
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
        if let Some(ring) = self.ring.take() {
            ring.span.record("cycles", u64::from(ring.cycles));
            ring.span.record("worker", ring.worker);
        }
        self.speech = None;
        self.transcribing = None;
        self.end_turn(None);
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
        // A run going when the call ends is let finish: its hand-over, or its turn if it
        // fails, still goes to the chief. Harold stopping drops it with the rest.
        if let Some(run) = self.run.take() {
            if why == Ended::Shutdown {
                run.task.abort();
            } else {
                self.detached = Some((run.id, run.text, run.trace.span.clone()));
            }
        }
        if why == Ended::Shutdown {
            self.queued.clear();
        } else {
            self.pass_on_queued();
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
            // A worker kept for the next call is stopped later, by its keeper.
            if self.worker_ready && !worker.kept() {
                info!(reason = "call_ended", "voice worker stopped");
            }
            worker.close().await;
        }
        remove_call_folder(&self.dir);
        self.awake = None;
        self.log_ended(why);
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

/// The next status `status` reports. Once the speech models are gone for good, `status` is
/// set to `None` and is no longer followed. Cancel-safe.
async fn models_change(status: &mut Option<watch::Receiver<ModelsStatus>>) -> ModelsStatus {
    let Some(receiver) = status.as_mut() else {
        return std::future::pending().await;
    };
    let changed = receiver.changed().await;
    let now = receiver.borrow_and_update().clone();
    if changed.is_err() {
        *status = None;
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
