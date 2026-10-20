//! The conversation core shared by the desk and, later, away-web-app's phone call:
//! it hears the chief speak, finds where each turn ends, and writes it down as audio.

mod call;
mod hang_up;
mod mac;
mod phone;
mod phrases;
mod speech;
mod wav;
mod worker;

use call::{Ended, Event, chief_pane, lock, run_call, sweep_call_folders};
pub(crate) use mac::MacPorts;
#[cfg_attr(not(test), allow(unused_imports))]
pub(crate) use phone::{
    ANSWER_KEEP, AudioRefused, PHONE_SILENT_LIMIT, PIECE_LIMIT_BYTES, PIECES_QUEUED, PLAY_GRACE,
    PhoneCalls, PhoneLine, PhoneSound, PhoneSpeech, PhoneStarted, PhoneView, SPEECH_BASE,
    SPEECH_PER_CHAR,
};
/// The web app's `POST /call` says these when no phone call starts.
pub(crate) use phrases::{NO_CHIEF, VOICE_FAILED};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::{mpsc, oneshot, watch};
use tracing::info;
use worker::{VoiceWorker, WorkerError};

/// The sample rate of the audio the call listens to.
pub(crate) const SAMPLE_RATE: u32 = 16_000;

/// Where a call's audio comes from: 16 kHz mono samples as `i16` values (the byte order of
/// whatever carries them is the source's business, not the caller's).
pub(crate) trait AudioSource: Send + 'static {
    /// The next block of samples, of any length, an empty block included. `None` means the
    /// source has ended (ffmpeg exited, the phone hung up), and every later call returns `None`.
    ///
    /// Cancel-safe: dropping the future before it resolves loses no samples and shifts none.
    fn next_block(&mut self) -> impl Future<Output = Option<Vec<i16>>> + Send;

    /// The next block with the time it was recorded, so the core can tell speech recorded
    /// while a sound played from speech read while it played. The default stamps the block
    /// when it is read, which is when it was recorded for a source that reads as it records.
    ///
    /// Cancel-safe as `next_block` is.
    fn next_stamped(
        &mut self,
    ) -> impl Future<Output = Option<(Vec<i16>, tokio::time::Instant)>> + Send {
        async move {
            let block = self.next_block().await?;
            Some((block, tokio::time::Instant::now()))
        }
    }
}

/// What a sound says, for a sink that shows or relays it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Said {
    /// The tick that plays while an answer is awaited.
    Tick,
    /// One of Harold's own phrases.
    Phrase(String),
    /// A line said into the call.
    Line { text: String, by: Speaker },
}

/// Where a call's sound goes. The core plays one file at a time and knows when each ends.
pub(crate) trait AudioSink: Send + Sync + 'static {
    /// Plays the 16-bit PCM WAV at `wav` (at any sample rate). One `play` runs at a time.
    /// Resolves `Ok` when the sound has ended or `stop` ended it, and `Err` when it could not
    /// be played.
    fn play(&self, wav: &Path) -> impl Future<Output = Result<(), String>> + Send;
    /// `play`, told what the sound says. The default ignores that.
    fn play_said(
        &self,
        wav: &Path,
        said: &Said,
    ) -> impl Future<Output = Result<(), String>> + Send {
        let _ = said;
        self.play(wav)
    }
    /// Stops what `play` is playing now, and returns at once; the sound stops promptly. Does
    /// nothing when nothing plays, and does not affect a later `play`.
    fn stop(&self);
}

/// What a call reads and does outside the core. The daemon's are `MacPorts`; the phone's
/// come with away-web-app.
pub(crate) trait CallPorts: Send + Sync + 'static {
    type Source: AudioSource;
    type Sink: AudioSink;
    type Worker: VoiceWorker;
    type Panes: crate::timed_prompts::Ports;
    /// Held for the length of the call; dropping it lets the display sleep again. Decision 16.
    type Awake: Send + 'static;
    fn open_source(&self) -> Result<Self::Source, String>;
    /// The one sink every call plays through.
    fn sink(&self) -> Arc<Self::Sink>;
    /// Starts the worker for a call whose folder is `call_dir`; loading goes on in the background.
    fn start_worker(&self, call_dir: &Path) -> Result<Self::Worker, WorkerError>;
    fn panes(&self) -> &Self::Panes;
    /// `[notify] chief`.
    fn chief_folder(&self) -> Option<String>;
    /// Says `line` with the `[tts]` command; resolves when it has ended.
    fn speak_with_tts(&self, line: &str) -> impl Future<Output = ()> + Send;
    /// Stops a line the `[tts]` command is saying now.
    fn stop_desk_line(&self);
    /// The desk's turn-end update for a turn started by `prompt` (Open point 1).
    fn turn_end_update(&self, prompt: &str) -> impl Future<Output = String> + Send;
    /// Call folders are made under this: `$TMPDIR/harold-voice-input` in the daemon.
    fn calls_dir(&self) -> PathBuf;
    /// Phrases are kept here between calls: `$TMPDIR/harold-voice-phrases` in the daemon.
    fn phrases_dir(&self) -> PathBuf;
    /// The Kokoro voice, part of each phrase's file name.
    fn voice_name(&self) -> String;
    /// Whether Harold's voice is muted now (`outbound::is_tts_muted` in the daemon). Decision 17.
    fn voice_muted(&self) -> bool;
    fn keep_awake(&self) -> Result<Self::Awake, String>;
    /// A call ended, for `reason` (the `reason` of its `call ended` line). Does nothing by default.
    fn call_ended(&self, reason: &'static str) {
        let _ = reason;
    }
    /// What Harold says when the chief has not answered in 90 seconds.
    fn no_answer_phrase(&self) -> &'static str {
        phrases::NO_ANSWER
    }
}

/// What a call is doing, as the dashboard's button shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CallState {
    /// No `[voice_input]`: no call can start.
    NotSetUp,
    Off,
    Listening,
    Transcribing,
    /// A turn was typed and the chief has not answered yet.
    Waiting,
    /// Harold plays a line or a phrase; the user is not heard.
    Speaking,
}

/// What became of the user's last turn, or of the last START.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TurnResult {
    Typed,
    NothingHeard,
    NoChief,
    ChiefBusy,
    Failed,
    VoiceMuted,
}

impl TurnResult {
    /// The `reason` a log line carries.
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Self::Typed => "typed",
            Self::NothingHeard => "nothing_heard",
            Self::NoChief => "no_chief",
            Self::ChiefBusy => "chief_busy",
            Self::Failed => "failed",
            Self::VoiceMuted => "voice_muted",
        }
    }
}

/// The answer to every call action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CallStatus {
    pub state: CallState,
    /// The pane of the agent in the chief's folder, read when the status is asked for.
    pub chief_pane_id: Option<String>,
    /// The last transcript typed into the chief's pane in this call.
    pub last_transcript: Option<String>,
    pub last_result: Option<TurnResult>,
}

/// Who sent a line said into an open call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Speaker {
    /// `harold speak` from the chief: the answer to the user's turn.
    Chief,
    /// Another agent's turn-end update.
    OtherAgent,
}

/// Starts, ends and talks to at most one call at a time.
pub(crate) struct CallControl<P: CallPorts> {
    ports: Arc<P>,
    set_up: bool,
    status: Arc<Mutex<CallStatus>>,
    call: Mutex<Option<Running<P::Worker>>>,
}

/// The call started last.
struct Running<W> {
    events: mpsc::UnboundedSender<Event<W>>,
    /// Turns true once the call listens, or has ended before it did.
    opened: watch::Receiver<bool>,
    /// Turns true once the call has ended and cleaned up.
    ended: watch::Receiver<bool>,
}

impl<W> Running<W> {
    fn is_open(&self) -> bool {
        !*self.ended.borrow() && !self.events.is_closed()
    }
}

impl<P: CallPorts> CallControl<P> {
    pub(crate) fn new(ports: P, set_up: bool) -> Self {
        let state = if set_up {
            CallState::Off
        } else {
            CallState::NotSetUp
        };
        Self {
            ports: Arc::new(ports),
            set_up,
            status: Arc::new(Mutex::new(CallStatus {
                state,
                chief_pane_id: None,
                last_transcript: None,
                last_result: None,
            })),
            call: Mutex::new(None),
        }
    }

    /// The call's status, with the chief's pane read now.
    pub(crate) fn status(&self) -> CallStatus {
        let mut status = lock(&self.status).clone();
        status.chief_pane_id = chief_pane(&*self.ports);
        status
    }

    /// Starts a call, and answers once it listens or has ended. With a call already on,
    /// or still opening, answers its status once it has opened, and starts nothing new.
    pub(crate) async fn start(&self) -> CallStatus {
        if !self.set_up {
            return self.status();
        }
        let mut opened = {
            let mut call = lock(&self.call);
            if let Some(running) = call.as_ref().filter(|running| running.is_open()) {
                running.opened.clone()
            } else {
                if self.ports.voice_muted() {
                    drop(call);
                    {
                        let mut status = lock(&self.status);
                        status.state = CallState::Off;
                        status.last_result = Some(TurnResult::VoiceMuted);
                    }
                    info!(reason = TurnResult::VoiceMuted.reason(), "call not started");
                    return self.status();
                }
                let (events, inbox) = mpsc::unbounded_channel();
                let (opened_tx, opened) = watch::channel(false);
                let (ended_tx, ended) = watch::channel(false);
                let ports = Arc::clone(&self.ports);
                let status = Arc::clone(&self.status);
                let own_events = events.clone();
                tokio::spawn(async move {
                    run_call(ports, status, own_events, inbox, opened_tx).await;
                    let _ = ended_tx.send(true);
                });
                *call = Some(Running {
                    events,
                    opened: opened.clone(),
                    ended,
                });
                opened
            }
        };
        let _ = opened.wait_for(|opened| *opened).await;
        self.status()
    }

    /// Ends the call, and answers once it has ended and cleaned up.
    pub(crate) async fn stop(&self) -> CallStatus {
        self.end(Ended::Stopped).await;
        self.status()
    }

    /// Stops what plays now and listens at once. The chief's lines are then not played
    /// until the next turn is typed (Decision 12).
    pub(crate) fn talk_now(&self) -> CallStatus {
        self.send(Event::TalkNow);
        self.status()
    }

    /// Plays `line` in the open call and resolves once it has ended. `None` when no call is open.
    /// `Some(false)` when the call ended before playing it, so the delivery retries outside the call.
    /// Test-only: every line Harold says in a call, the desk's or the phone's, arrives on a
    /// blocking thread and goes through `say_blocking`.
    #[cfg(test)]
    pub(crate) async fn say(&self, line: &str, by: Speaker) -> Option<bool> {
        Some(self.send_line(line, by)?.await.unwrap_or(false))
    }

    /// The same from a blocking thread (the projector's `spawn_blocking`); never call it from
    /// async code. With no call open it returns `None` at once.
    pub(crate) fn say_blocking(&self, line: &str, by: Speaker) -> Option<bool> {
        Some(self.send_line(line, by)?.blocking_recv().unwrap_or(false))
    }

    /// Hands `line` to the open call; the answer resolves once it has ended. `None` when no
    /// call is open.
    fn send_line(&self, line: &str, by: Speaker) -> Option<oneshot::Receiver<bool>> {
        let (reply, said) = oneshot::channel();
        let line = line.to_string();
        self.send(Event::Say { line, by, reply }).then_some(said)
    }

    /// Has the open call forget the turn being spoken, if any, as if it had not been heard.
    /// A phone's call uses it when a newer screen takes the call over. Does nothing when no
    /// call is open.
    pub(crate) fn forget_turn(&self) {
        self.send(Event::ForgetTurn);
    }

    /// Ends an open call with reason `shutdown`.
    pub(crate) async fn shutdown(&self) {
        self.end(Ended::Shutdown).await;
    }

    /// Sends `event` to the open call; false when no call is open.
    fn send(&self, event: Event<P::Worker>) -> bool {
        let call = lock(&self.call);
        call.as_ref()
            .is_some_and(|running| running.is_open() && running.events.send(event).is_ok())
    }

    async fn end(&self, why: Ended) {
        let mut ended = {
            let call = lock(&self.call);
            match call.as_ref() {
                Some(running) if running.is_open() => {
                    let _ = running.events.send(Event::Stop(why));
                    running.ended.clone()
                }
                _ => return,
            }
        };
        let _ = ended.wait_for(|ended| *ended).await;
    }
}

/// The daemon's calls with the chief at the desk.
pub(crate) type DaemonCalls = CallControl<MacPorts>;

static DAEMON_CALLS: OnceLock<DaemonCalls> = OnceLock::new();

/// Sets the daemon's calls, once, at start-up, and removes the call folders a Harold that
/// crashed or was killed during a call left behind.
pub(crate) fn init_daemon_calls(calls: DaemonCalls) {
    sweep_call_folders(&calls.ports.calls_dir());
    assert!(
        DAEMON_CALLS.set(calls).is_ok(),
        "init_daemon_calls called more than once"
    );
}

/// The daemon's calls; `None` before start-up sets them, and in `--diagnostics` and tests.
pub(crate) fn daemon_calls() -> Option<&'static DaemonCalls> {
    DAEMON_CALLS.get()
}

/// Somewhere the chief's spoken lines can be said: a set of calls, of which one may be open.
pub(crate) trait LineTaker: Send + Sync {
    /// Says `line` in the open call, from a blocking thread, and returns once it has ended.
    /// `None` when no call is open (or the line is not one these calls say).
    fn say_blocking(&self, line: &str, by: Speaker) -> Option<bool>;
}

impl<P: CallPorts> LineTaker for CallControl<P> {
    fn say_blocking(&self, line: &str, by: Speaker) -> Option<bool> {
        CallControl::say_blocking(self, line, by)
    }
}

/// The result of the first of `calls` that takes `line`: `None` when none has a call open.
/// A call that takes it ends the search, whether or not it played the line.
fn say_in_first_open(calls: &[&dyn LineTaker], line: &str, by: Speaker) -> Option<bool> {
    calls.iter().find_map(|calls| calls.say_blocking(line, by))
}

static PHONE_CALLS: OnceLock<Arc<dyn LineTaker>> = OnceLock::new();

/// Sets the phone's calls, once, at start-up.
pub(crate) fn init_phone_calls(calls: Arc<dyn LineTaker>) {
    assert!(
        PHONE_CALLS.set(calls).is_ok(),
        "init_phone_calls called more than once"
    );
}

/// Says `line` in the phone's open call, from a blocking thread, and returns once it has
/// ended. `None` when Harold has no phone calls or no phone call is open: then it neither
/// blocks nor needs a runtime. The projector asks this before the screen lock decides, so a
/// desk call is not asked here: at the desk the line still goes by the lock's rules.
pub(crate) fn say_in_phone_call(line: &str, by: Speaker) -> Option<bool> {
    say_in_phone_call_over(PHONE_CALLS.get().map(|calls| calls.as_ref()), line, by)
}

/// `say_in_phone_call` over `phone`, the phone's calls if Harold has them.
pub(crate) fn say_in_phone_call_over(
    phone: Option<&dyn LineTaker>,
    line: &str,
    by: Speaker,
) -> Option<bool> {
    phone?.say_blocking(line, by)
}

/// Says `line` in the open call at the Mac (the desk's), from a blocking thread, and returns
/// once it has ended. `None` when Harold has no desk call or none is open: then it neither
/// blocks nor needs a runtime. The phone's call is not tried here: it belongs to the
/// `web_app` list, which `say_in_phone_call` is asked under, so a pane held off that list is
/// never heard in it.
pub(crate) fn say_in_open_call(line: &str, by: Speaker) -> Option<bool> {
    let desk = daemon_calls().map(|calls| calls as &dyn LineTaker);
    let calls: Vec<&dyn LineTaker> = desk.into_iter().collect();
    say_in_first_open(&calls, line, by)
}

#[cfg(test)]
#[path = "voice_input_tests.rs"]
pub(crate) mod tests;

#[cfg(test)]
#[path = "voice_input_phone_tests.rs"]
mod phone_tests;
