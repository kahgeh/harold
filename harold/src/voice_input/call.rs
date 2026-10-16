//! One call with the chief at the desk. A call is one task that owns the worker, the
//! detector and the state, and takes its events from one channel: sample blocks, control
//! commands, lines to say, timers, and the ends of the work it spawned (a request to the
//! worker, a sound, a turn-end update). `CallControl` starts it, talks to it and reads its
//! status.

use super::hang_up::{Spoken, hang_up};
use super::phrases::{
    BYE, CHIEF_BUSY, NO_ANSWER, NO_CHIEF, NOTHING_HEARD, VOICE_FAILED, VOICE_STOPPED, phrase_file,
};
use super::speech::SpeechDetector;
use super::wav::{tick_wav, wav_bytes};
use super::worker::{Transcript, VoiceWorker, WorkerError};
use super::{
    AudioSink, AudioSource, CallPorts, CallState, CallStatus, SAMPLE_RATE, Speaker, TurnResult,
};
use crate::agent::domain::EffectiveAgentState;
use crate::away_message::one_line;
use crate::outbound::left_out::same_folder;
use crate::telemetry::{self, StartedBy};
use crate::timed_prompts::{AgentsView, Attempt, Ports, Reason, Writer, type_when_ready};
use std::collections::VecDeque;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until};
use tracing::{info, warn};
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
        }
    }
}

/// What a call's task hears about.
pub(super) enum Event<W> {
    Block(Vec<i16>),
    SourceEnded,
    Stop(Ended),
    TalkNow,
    Say {
        line: String,
        by: Speaker,
        reply: oneshot::Sender<bool>,
    },
    /// A transcribe request ended; the worker comes back with its answer.
    Transcribed {
        dir: PathBuf,
        result: Result<Transcript, WorkerError>,
        worker: W,
    },
    /// A say request for the sound `id` ended.
    Made {
        id: u64,
        wav: PathBuf,
        result: Result<(), WorkerError>,
        worker: W,
    },
    Played {
        id: u64,
    },
    TurnEndUpdate(String),
}

pub(super) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The pane of the agent in the chief's folder.
pub(super) fn chief_pane<P: CallPorts>(ports: &P) -> Option<String> {
    let folder = ports.chief_folder()?;
    ports
        .panes()
        .agents()
        .panes
        .into_iter()
        .find(|pane| same_folder(&pane.folder, &folder))
        .map(|pane| pane.pane_id)
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

// ---------------------------------------------------------------------------
// The call's task
// ---------------------------------------------------------------------------

pub(super) async fn run_call<P: CallPorts>(
    ports: Arc<P>,
    status: Arc<Mutex<CallStatus>>,
    events: mpsc::UnboundedSender<Event<P::Worker>>,
    mut inbox: mpsc::UnboundedReceiver<Event<P::Worker>>,
    started: oneshot::Sender<()>,
) {
    let Some(mut call) = Call::open(ports, status, events, started).await else {
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
        started: oneshot::Sender<()>,
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
            while let Some(block) = source.next_block().await {
                if blocks.send(Event::Block(block)).is_err() {
                    return;
                }
            }
            let _ = blocks.send(Event::SourceEnded);
        });
        info!("call started");
        let call = Self {
            sink: ports.sink(),
            ports,
            status,
            events,
            dir,
            detector: SpeechDetector::new(),
            mode: Mode::Listening,
            worker: Some(worker),
            request: None,
            jobs: VecDeque::new(),
            worker_exits: 0,
            sounds: VecDeque::new(),
            playing: None,
            unmuted_at: Instant::now(),
            awaiting: None,
            held: None,
            skip_chief_lines: false,
            done: false,
            reader: Some(reader),
            updates: Vec::new(),
            awake,
            next_id: 0,
        };
        call.publish();
        let _ = started.send(());
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
            }
        }
    }

    async fn on_event(&mut self, event: Event<P::Worker>) {
        match event {
            Event::Block(block) => self.heard_block(&block),
            Event::SourceEnded => {
                self.end(Ended::MicrophoneFailed, Some(VOICE_FAILED)).await;
            }
            Event::Stop(why) => self.end(why, None).await,
            Event::TalkNow => self.talk_now(),
            Event::Say { line, by, reply } => self.line(line, by, Some(reply)),
            Event::Transcribed {
                dir,
                result,
                worker,
            } => {
                let _ = std::fs::remove_dir_all(&dir);
                self.transcribed(result, worker).await;
            }
            Event::Made {
                id,
                wav,
                result,
                worker,
            } => self.made(id, wav, result, worker).await,
            Event::Played { id } => self.played(id).await,
            Event::TurnEndUpdate(line) => self.turn_end_update(line),
        }
    }

    // --- the samples ---

    fn heard_block(&mut self, block: &[i16]) {
        if self.mode != Mode::Listening {
            self.detector.reset();
            return;
        }
        let muted = self.playing.is_some() || Instant::now() < self.unmuted_at;
        let Some(turn) = self.detector.push(block, muted).into_iter().next() else {
            return;
        };
        self.turn_ended(&turn);
    }

    /// Writes the turn to `<call dir>/turn-<uuid>/turn.wav` for the worker to transcribe.
    fn turn_ended(&mut self, samples: &[i16]) {
        self.mode = Mode::Transcribing;
        self.detector.reset();
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
        self.request = Some(tokio::spawn(async move {
            let event = match job {
                Job::Transcribe { dir } => {
                    let result = worker.transcribe(&dir.join(TURN_WAV)).await;
                    Event::Transcribed {
                        dir,
                        result,
                        worker,
                    }
                }
                Job::Make { id, text, wav } => {
                    let result = worker.say(&text, &wav).await;
                    Event::Made {
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

    async fn transcribed(&mut self, result: Result<Transcript, WorkerError>, worker: P::Worker) {
        self.request = None;
        match result {
            Ok(transcript) => {
                self.worker = Some(worker);
                self.heard(transcript).await;
            }
            Err(error) => {
                self.dropped(TurnResult::Failed);
                self.mode = Mode::Listening;
                self.worker_failed(error, worker, None).await;
            }
        }
    }

    async fn made(
        &mut self,
        id: u64,
        wav: PathBuf,
        result: Result<(), WorkerError>,
        worker: P::Worker,
    ) {
        self.request = None;
        let at = self.sounds.iter().position(|sound| sound.id == id);
        match result {
            Ok(()) => {
                self.worker = Some(worker);
                match at {
                    Some(at) => self.keep_made(at),
                    None => {
                        let _ = std::fs::remove_file(&wav);
                    }
                }
            }
            // The worker could not make it: the `[tts]` command says it instead.
            Err(WorkerError::Failed) => {
                self.worker = Some(worker);
                let _ = std::fs::remove_file(&wav);
                if let Some(at) = at {
                    self.sounds[at].audio = Audio::Tts;
                }
            }
            Err(error) => {
                let retry = at.map(|at| Job::Make {
                    id,
                    text: self.sounds[at].text.clone(),
                    wav,
                });
                self.worker_failed(error, worker, retry).await;
            }
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

    /// After a request failed: a worker that is still running goes on. One that exited is
    /// started once more, and `retry` is sent again; a second exit, or a worker that could
    /// not load, ends the call.
    async fn worker_failed(&mut self, error: WorkerError, worker: P::Worker, retry: Option<Job>) {
        let Some(reason) = error.exit_reason() else {
            self.worker = Some(worker);
            return;
        };
        warn!(reason, "voice worker exited");
        worker.close().await;
        self.worker_exits += 1;
        if error == WorkerError::FailedToLoad || self.worker_exits > 1 {
            self.end(Ended::VoiceFailed, Some(VOICE_FAILED)).await;
            return;
        }
        match self.ports.start_worker(&self.dir) {
            Ok(worker) => self.worker = Some(worker),
            Err(_) => {
                self.end(Ended::VoiceFailed, Some(VOICE_FAILED)).await;
                return;
            }
        }
        if let Some(job) = retry {
            self.jobs.push_front(job);
        }
        let id = self.new_id();
        self.sounds.push_front(Sound {
            id,
            text: VOICE_STOPPED.to_string(),
            audio: Audio::Tts,
            by: None,
            answers: false,
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
                let turn = match self.held.take() {
                    Some(held) => format!("{} {turn}", held.text),
                    None => turn.to_string(),
                };
                self.last_turn(turn).await;
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

    /// The turn said before a bye: typed once if the chief takes it, then the call ends.
    async fn last_turn(&mut self, text: String) {
        match try_type(&*self.ports, &text).await {
            Typing::Typed(pane) => self.typed(pane, text),
            Typing::NoChief => self.dropped(TurnResult::NoChief),
            Typing::Busy => self.dropped(TurnResult::ChiefBusy),
            Typing::Failed => self.dropped(TurnResult::Failed),
        }
        self.say_bye();
    }

    fn typed(&mut self, pane: String, text: String) {
        let words = text.split_whitespace().count();
        info!(pane_id = %pane, words, "spoken prompt typed");
        telemetry::prompt_typed(
            &pane,
            StartedBy::Voice,
            None,
            time::OffsetDateTime::now_utc(),
        );
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

    fn dropped(&self, result: TurnResult) {
        info!(reason = result.reason(), "spoken prompt dropped");
        lock(&self.status).last_result = Some(result);
    }

    /// Drops a turn still waiting for a busy chief.
    fn drop_held(&mut self) {
        if self.held.take().is_some() {
            self.dropped(TurnResult::ChiefBusy);
        }
    }

    fn say_bye(&mut self) {
        self.drop_held();
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
        held.chain(awaiting).min()
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
            self.phrase(NO_ANSWER, After::Nothing);
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
            self.updates.push(tokio::spawn(async move {
                let line = ports.turn_end_update(&prompt).await;
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
        let Some(sound) = self.sounds.pop_front() else {
            return;
        };
        if sound.answers && self.awaiting.take().is_some() && self.mode == Mode::Waiting {
            self.mode = Mode::Listening;
        }
        let by_tts = matches!(sound.audio, Audio::Tts);
        let task = match &sound.audio {
            Audio::Wav { path, .. } => self.spawn_play(sound.id, path.clone()),
            Audio::Tts => self.spawn_tts(sound.id, sound.text.clone()),
        };
        self.playing = Some(Playing {
            id: sound.id,
            task,
            by_tts,
            sound: Some(sound),
        });
    }

    fn play_tick(&mut self) {
        if self.playing.is_some() || !self.sounds.is_empty() {
            return;
        }
        let id = self.new_id();
        let task = self.spawn_play(id, self.dir.join(TICK_WAV));
        self.playing = Some(Playing {
            id,
            task,
            by_tts: false,
            sound: None,
        });
    }

    fn spawn_play(&self, id: u64, wav: PathBuf) -> JoinHandle<()> {
        let sink = Arc::clone(&self.sink);
        let events = self.events.clone();
        tokio::spawn(async move {
            if let Err(error) = sink.play(&wav).await {
                warn!(error = %error, "call sound not played");
            }
            let _ = events.send(Event::Played { id });
        })
    }

    fn spawn_tts(&self, id: u64, text: String) -> JoinHandle<()> {
        let ports = Arc::clone(&self.ports);
        let events = self.events.clone();
        tokio::spawn(async move {
            ports.speak_with_tts(&text).await;
            let _ = events.send(Event::Played { id });
        })
    }

    async fn played(&mut self, id: u64) {
        if self.playing.as_ref().is_none_or(|playing| playing.id != id) {
            return;
        }
        let Some(playing) = self.playing.take() else {
            return;
        };
        self.unmuted_at = Instant::now() + MUTE_TAIL;
        if let Some(After::End(why)) = playing.sound.map(|sound| sound.finish(true)) {
            self.end(why, None).await;
        }
    }

    /// Stops what plays, listens at once, and takes the chief's lines as said until the
    /// next turn is typed (Decision 12).
    fn talk_now(&mut self) {
        self.sink.stop();
        if self.playing.as_ref().is_some_and(|playing| playing.by_tts) {
            self.ports.stop_desk_line();
        }
        self.skip_chief_lines = true;
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
            .is_some_and(|playing| playing.sound.is_some());
        let state = match self.mode {
            _ if speaking => CallState::Speaking,
            Mode::Listening => CallState::Listening,
            Mode::Transcribing => CallState::Transcribing,
            Mode::Waiting => CallState::Waiting,
            Mode::Ending => CallState::Speaking,
        };
        lock(&self.status).state = state;
    }

    /// Ends the call: a turn not yet typed is dropped, lines not yet played go back to
    /// their senders, `last_words` are said through the `[tts]` command, and the source,
    /// the worker, the call's folder and the display's wake are let go.
    async fn end(&mut self, why: Ended, last_words: Option<&'static str>) {
        if self.done {
            return;
        }
        self.done = true;
        self.mode = Mode::Ending;
        self.drop_held();
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
        lock(&self.status).state = CallState::Off;
    }
}

/// A call that ended before it listened.
struct NotOpened<'a, P> {
    ports: &'a P,
    status: &'a Mutex<CallStatus>,
    dir: &'a Path,
}

impl<P: CallPorts> NotOpened<'_, P> {
    /// Answers START, says why the call ended, and removes its folder.
    async fn close(&self, started: oneshot::Sender<()>, why: Ended) {
        lock(self.status).state = CallState::Off;
        let _ = started.send(());
        if why == Ended::NoChief {
            // No worker runs yet: the phrase plays from the phrase folder, or else
            // through the `[tts]` command (Decision 8).
            let kept = phrase_file(
                &self.ports.phrases_dir(),
                &self.ports.voice_name(),
                NO_CHIEF,
            );
            if !kept.is_file() || self.ports.sink().play(&kept).await.is_err() {
                self.ports.speak_with_tts(NO_CHIEF).await;
            }
        } else {
            self.ports.speak_with_tts(VOICE_FAILED).await;
        }
        let _ = std::fs::remove_dir_all(self.dir);
        info!(reason = why.as_str(), "call ended");
    }
}
