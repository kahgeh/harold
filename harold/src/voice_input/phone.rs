//! The phone's side of a call: pieces of audio the app posts, sounds the app fetches and
//! plays, and lines it says in its own voice. The ports share one `PhoneLine` with the app's
//! call routes, which hand it what the phone posts and show the phone what waits for it.

use super::call::lock;
use super::phrases::NO_ANSWER_IN_THREAD;
use super::worker::WorkerError;
use super::{
    AudioSink, AudioSource, CallControl, CallPorts, CallState, CallStatus, LineTaker, Said, Speaker,
};
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep, sleep_until, timeout};
use tracing::info;
use uuid::Uuid;

/// The most one posted piece may hold: one second of 16 kHz `i16` samples.
pub(crate) const PIECE_LIMIT_BYTES: usize = 32_000;
/// How many posted pieces wait for the call to read them before more are refused.
pub(crate) const PIECES_QUEUED: usize = 64;
/// The call's source ends once the phone has posted nothing for this long while nothing
/// played: the app was closed or the phone locked.
pub(crate) const PHONE_SILENT_LIMIT: Duration = Duration::from_secs(15);
/// How long past a sound's length the phone has to report it played.
pub(crate) const PLAY_GRACE: Duration = Duration::from_secs(10);
/// No sound is offered to the phone for longer than this.
pub(crate) const ANSWER_KEEP: Duration = Duration::from_secs(120);
/// A line for the phone's own voice counts as said after this, and `SPEECH_PER_CHAR` for
/// each of its characters, with no report from the phone.
pub(crate) const SPEECH_BASE: Duration = Duration::from_secs(2);
pub(crate) const SPEECH_PER_CHAR: Duration = Duration::from_millis(80);
/// How often the view takes the open call's state and transcript.
pub(super) const STATUS_EVERY: Duration = Duration::from_millis(100);

/// What the phone shows and plays now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PhoneView {
    pub state: CallState,
    pub transcript: Option<String>,
    /// The sound the phone is to fetch and play.
    pub sound: Option<PhoneSound>,
    /// The line the phone is to say in its own voice.
    pub speech: Option<PhoneSpeech>,
    /// Why the last call ended, once it has.
    pub ended: Option<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PhoneSound {
    pub id: String,
    pub said: Said,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PhoneSpeech {
    pub id: String,
    pub text: String,
}

/// Why a posted piece was not taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AudioRefused {
    /// No call is open to take it.
    NoCall,
    /// An odd number of bytes, or a time too far off to stamp.
    Malformed,
    /// More than `PIECE_LIMIT_BYTES`.
    TooLarge,
    /// `PIECES_QUEUED` pieces already wait to be read.
    Full,
}

/// What the phone's ports and the app's call routes share.
pub(crate) struct PhoneLine {
    inner: Mutex<Inner>,
    view: watch::Sender<PhoneView>,
    quiet: watch::Sender<Quiet>,
    /// Has the open call forget the turn being spoken; set once by the phone's calls.
    forget_turn: OnceLock<Box<dyn Fn() + Send + Sync>>,
}

#[derive(Default)]
struct Inner {
    /// Where the open call's source reads its pieces from.
    source: Option<Pieces>,
    /// The offered sounds, by id.
    sounds: HashMap<String, Offer>,
    speech: Option<Speech>,
    /// When the phone asked for the call being started, which the phone stamps its pieces
    /// from; the next source's base, and cleared once that start has answered.
    asked: Option<Instant>,
    /// The id the open call's pieces carry: a new one for each call, and for each START
    /// that takes the open call over.
    call: Option<String>,
    /// Counts the screens that have had the call: one more for each source opened and for
    /// each takeover.
    screen: u64,
}

/// A piece of samples and when it was recorded.
type Piece = (Vec<i16>, Instant);

/// A piece as it waits to be read, with the screen that posted it.
type Queued = (Vec<i16>, Instant, u64);

struct Pieces {
    sender: mpsc::Sender<Queued>,
    /// When the phone asked for the call, or else when the source opened: the phone
    /// stamps each piece in milliseconds from then.
    base: Instant,
}

struct Offer {
    wav: PathBuf,
    /// Whether the phone has fetched the file.
    fetched: bool,
    /// Dropped when the offer is taken back, which ends its play.
    _ended: oneshot::Sender<()>,
}

struct Speech {
    id: String,
    /// Dropped when the line is taken back, which ends its wait.
    _ended: oneshot::Sender<()>,
}

/// The silence clock the open source ends by.
#[derive(Debug, Clone, Copy)]
struct Quiet {
    /// When the last piece arrived, or the last fetched sound or line stopped waiting.
    since: Instant,
    /// Whether a sound the phone fetched, or a line, waits for the phone; the clock does
    /// not run meanwhile.
    waiting: bool,
}

impl PhoneLine {
    pub(crate) fn new(set_up: bool) -> Arc<Self> {
        let state = if set_up {
            CallState::Off
        } else {
            CallState::NotSetUp
        };
        Arc::new(Self {
            inner: Mutex::default(),
            view: watch::channel(PhoneView {
                state,
                transcript: None,
                sound: None,
                speech: None,
                ended: None,
            })
            .0,
            quiet: watch::channel(Quiet {
                since: Instant::now(),
                waiting: false,
            })
            .0,
            forget_turn: OnceLock::new(),
        })
    }

    /// Sets what has the open call forget the turn being spoken. Only the first call counts.
    pub(super) fn set_forget_turn(&self, forget: impl Fn() + Send + Sync + 'static) {
        let _ = self.forget_turn.set(Box::new(forget));
    }

    fn forget_turn(&self) {
        if let Some(forget) = self.forget_turn.get() {
            forget();
        }
    }

    /// Takes a piece the phone posted for the call `call`: little-endian `i16` samples
    /// recorded `at_ms` milliseconds after the source's base: when the phone asked for the
    /// call. A piece carrying any id but the open call's is refused as if no call were open.
    pub(crate) fn audio(&self, call: &str, at_ms: u64, bytes: &[u8]) -> Result<(), AudioRefused> {
        let mut inner = lock(&self.inner);
        if inner.call.as_deref() != Some(call) {
            return Err(AudioRefused::NoCall);
        }
        let pieces = inner.source.as_ref().ok_or(AudioRefused::NoCall)?;
        if bytes.len() % 2 == 1 {
            return Err(AudioRefused::Malformed);
        }
        if bytes.len() > PIECE_LIMIT_BYTES {
            return Err(AudioRefused::TooLarge);
        }
        let at = pieces
            .base
            .checked_add(Duration::from_millis(at_ms))
            .ok_or(AudioRefused::Malformed)?;
        let samples = bytes
            .chunks_exact(2)
            .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        match pieces.sender.try_send((samples, at, inner.screen)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => return Err(AudioRefused::Full),
            Err(TrySendError::Closed(_)) => {
                inner.source = None;
                return Err(AudioRefused::NoCall);
            }
        }
        self.quiet.send_modify(|quiet| quiet.since = Instant::now());
        Ok(())
    }

    /// The phone has played the sound, or said the line, `id`.
    pub(crate) fn played(&self, id: &str) {
        self.take_back(id);
    }

    /// The offered sound `id`'s file, which the phone has now fetched. `None` once it is no
    /// longer offered.
    pub(crate) fn answer(&self, id: &str) -> Option<Vec<u8>> {
        // Read without the lock, so posted pieces do not wait behind the file.
        let wav = lock(&self.inner).sounds.get(id)?.wav.clone();
        let bytes = std::fs::read(&wav).ok()?;
        let mut inner = lock(&self.inner);
        inner.sounds.get_mut(id)?.fetched = true;
        self.note_waiting(&inner);
        Some(bytes)
    }

    /// What the phone shows and plays, now and as it changes.
    pub(crate) fn view(&self) -> watch::Receiver<PhoneView> {
        self.view.subscribe()
    }

    /// The open call's source's base, the time its pieces are stamped from.
    #[cfg(test)]
    fn source_base(&self) -> Option<Instant> {
        lock(&self.inner).source.as_ref().map(|pieces| pieces.base)
    }

    /// The id the open call's pieces carry; the last call's once it has ended.
    #[cfg(test)]
    pub(super) fn call_id(&self) -> Option<String> {
        lock(&self.inner).call.clone()
    }

    /// Hands the open call to a newer screen: a new id, which the call's pieces carry from
    /// now on, and a new base, `base`, which they are stamped from. Pieces carrying the
    /// older id are refused, which ends the call on the older screen, and those the older
    /// screen posted that still wait are not read. `None` when no source is open.
    pub(super) fn take_over(&self, base: Instant) -> Option<String> {
        let mut inner = lock(&self.inner);
        let pieces = inner.source.as_mut()?;
        pieces.base = base;
        inner.screen += 1;
        let call = new_id();
        inner.call = Some(call.clone());
        Some(call)
    }

    /// A source for a new call, its pieces stamped from when the phone asked for the call.
    /// An older source ends, and reads nothing more.
    pub(super) fn open_source(self: &Arc<Self>) -> PhoneSource {
        let (sender, pieces) = mpsc::channel(PIECES_QUEUED);
        let opened = Instant::now();
        let mut inner = lock(&self.inner);
        let base = inner.asked.take().unwrap_or(opened);
        inner.source = Some(Pieces { sender, base });
        inner.screen += 1;
        inner.call = Some(new_id());
        let screen = inner.screen;
        drop(inner);
        self.quiet.send_modify(|quiet| quiet.since = opened);
        PhoneSource {
            line: Arc::clone(self),
            pieces,
            screen,
            quiet: self.quiet.subscribe(),
            ended: false,
        }
    }

    /// Offers the sound at `wav` under a new id until the returned guard is dropped; the
    /// receiver resolves when the offer is taken back before that.
    fn offer(&self, wav: &Path, said: &Said) -> (Waiting<'_>, oneshot::Receiver<()>) {
        let id = new_id();
        let (ended, taken_back) = oneshot::channel();
        let mut inner = lock(&self.inner);
        let offer = Offer {
            wav: wav.to_path_buf(),
            fetched: false,
            _ended: ended,
        };
        inner.sounds.insert(id.clone(), offer);
        let sound = PhoneSound {
            id: id.clone(),
            said: said.clone(),
        };
        self.view.send_modify(|view| view.sound = Some(sound));
        self.note_waiting(&inner);
        (Waiting { line: self, id }, taken_back)
    }

    /// The same for a line the phone says in its own voice. A line still waiting is
    /// replaced, and its wait ends.
    fn offer_speech(&self, text: &str) -> (Waiting<'_>, oneshot::Receiver<()>) {
        let id = new_id();
        let (ended, taken_back) = oneshot::channel();
        let mut inner = lock(&self.inner);
        inner.speech = Some(Speech {
            id: id.clone(),
            _ended: ended,
        });
        let speech = PhoneSpeech {
            id: id.clone(),
            text: text.to_string(),
        };
        self.view.send_modify(|view| view.speech = Some(speech));
        self.note_waiting(&inner);
        (Waiting { line: self, id }, taken_back)
    }

    /// Takes back the sound or the line `id` if it still waits. For a sound still offered,
    /// answers whether the phone had fetched it; `None` when it was already taken back.
    fn take_back(&self, id: &str) -> Option<bool> {
        let mut inner = lock(&self.inner);
        let fetched = inner.sounds.remove(id).map(|offer| offer.fetched);
        if fetched.is_none() && inner.speech.as_ref().is_some_and(|speech| speech.id == id) {
            inner.speech = None;
        }
        self.view.send_if_modified(|view| {
            let sound = view.sound.take_if(|sound| sound.id == id).is_some();
            let speech = view.speech.take_if(|speech| speech.id == id).is_some();
            sound || speech
        });
        self.note_waiting(&inner);
        fetched
    }

    /// Takes back every offered sound.
    fn take_back_sounds(&self) {
        let ids: Vec<String> = lock(&self.inner).sounds.keys().cloned().collect();
        for id in ids {
            self.take_back(&id);
        }
    }

    /// Takes back the waiting line, if any.
    fn take_back_speech(&self) {
        let id = lock(&self.inner)
            .speech
            .as_ref()
            .map(|speech| speech.id.clone());
        if let Some(id) = id {
            self.take_back(&id);
        }
    }

    /// Restarts the silence clock when the last fetched sound or line stops waiting. A sound
    /// the phone has not fetched does not stop the clock: a phone that is gone fetches none.
    fn note_waiting(&self, inner: &Inner) {
        let waiting = inner.speech.is_some() || inner.sounds.values().any(|offer| offer.fetched);
        self.quiet.send_if_modified(|quiet| {
            if quiet.waiting == waiting {
                return false;
            }
            *quiet = Quiet {
                since: Instant::now(),
                waiting,
            };
            true
        });
    }
}

/// A sound or a line that waits for the phone, taken back when this is dropped: when its
/// play ends, however it ends, the task playing it aborted included.
struct Waiting<'a> {
    line: &'a PhoneLine,
    id: String,
}

impl Waiting<'_> {
    /// Takes it back now, as `PhoneLine::take_back` does.
    fn take_back(&self) -> Option<bool> {
        self.line.take_back(&self.id)
    }
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.line.take_back(&self.id);
    }
}

fn new_id() -> String {
    Uuid::new_v4().simple().to_string()
}

/// How long the WAV at `wav` lasts: its `data` chunk's size over the byte rate in its
/// header. A file that cannot be read lasts no time.
fn sound_length(wav: &Path) -> Duration {
    read_length(wav).unwrap_or(Duration::ZERO)
}

/// Reads the header and the chunk headers only, not the samples.
fn read_length(wav: &Path) -> std::io::Result<Duration> {
    let mut file = File::open(wav)?;
    let mut header = [0; 32];
    file.read_exact(&mut header)?;
    let byte_rate = u32::from_le_bytes([header[28], header[29], header[30], header[31]]);
    if byte_rate == 0 {
        return Ok(Duration::ZERO);
    }
    // The chunks follow `RIFF <size> WAVE`, each an id and a size, padded to an even length.
    let mut at = 12;
    loop {
        file.seek(SeekFrom::Start(at))?;
        let mut chunk = [0; 8];
        file.read_exact(&mut chunk)?;
        let size = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
        if &chunk[..4] == b"data" {
            return Ok(Duration::from_secs_f64(
                f64::from(size) / f64::from(byte_rate),
            ));
        }
        at += 8 + u64::from(size) + u64::from(size % 2);
    }
}

/// The call's audio, as the phone posts it.
pub(crate) struct PhoneSource {
    line: Arc<PhoneLine>,
    pieces: mpsc::Receiver<Queued>,
    /// The screen whose pieces this source reads now.
    screen: u64,
    quiet: watch::Receiver<Quiet>,
    ended: bool,
}

impl PhoneSource {
    /// The queued piece, unless a newer screen has taken the call over since it was posted.
    /// Before the newer screen's first piece, the call forgets the turn being spoken: the
    /// pieces the call read before are then the last of the older screen's, so the forget
    /// reaches the call after them and before anything of the newer screen's.
    fn take(&mut self, (samples, at, screen): Queued) -> Option<Piece> {
        if screen < lock(&self.line.inner).screen {
            return None;
        }
        if screen > self.screen {
            self.screen = screen;
            self.line.forget_turn();
        }
        Some((samples, at))
    }
}

impl AudioSource for PhoneSource {
    async fn next_block(&mut self) -> Option<Vec<i16>> {
        let (samples, _) = self.next_stamped().await?;
        Some(samples)
    }

    /// Ends `PHONE_SILENT_LIMIT` after the last piece, counting no time a sound the phone
    /// fetched, or a line, waited for it; or when a newer source replaced this one. Skips
    /// the pieces an older screen posted before a newer one took the call over.
    async fn next_stamped(&mut self) -> Option<Piece> {
        while !self.ended {
            let quiet = *self.quiet.borrow_and_update();
            tokio::select! {
                biased;
                piece = self.pieces.recv() => match piece {
                    Some(queued) => {
                        if let Some(piece) = self.take(queued) {
                            return Some(piece);
                        }
                    }
                    None => self.ended = true,
                },
                changed = self.quiet.changed() => self.ended = changed.is_err(),
                () = sleep_until(quiet.since + PHONE_SILENT_LIMIT), if !quiet.waiting => {
                    // Pieces posted from now on are refused: no call takes them.
                    self.pieces.close();
                    self.ended = true;
                }
            }
        }
        None
    }
}

/// The phone's speaker: each sound is offered for the phone to fetch, play and report.
pub(crate) struct PhoneSink {
    line: Arc<PhoneLine>,
    /// Whether the call has opened. Until it has, nothing is offered: a call that ends before
    /// it opens answers START with `409` and the line saying why, and the phone, which has
    /// then ended the call and shows the line, would never fetch an offer. The offer would
    /// only hold up the call's end, and the next Call tap, until its limit.
    opened: AtomicBool,
}

impl PhoneSink {
    /// A sink that offers every sound, as one in an open call does.
    #[cfg(test)]
    pub(super) fn new(line: Arc<PhoneLine>) -> Self {
        Self {
            line,
            opened: AtomicBool::new(true),
        }
    }

    /// A sink for a call's ports, which offers nothing until the call opens.
    fn unopened(line: Arc<PhoneLine>) -> Self {
        Self {
            line,
            opened: AtomicBool::new(false),
        }
    }

    fn opened(&self) -> bool {
        self.opened.load(Ordering::SeqCst)
    }
}

impl AudioSink for PhoneSink {
    /// Offered as a phrase with no words: the core always says what a sound says.
    async fn play(&self, wav: &Path) -> Result<(), String> {
        self.play_said(wav, &Said::Phrase(String::new())).await
    }

    /// Resolves when the phone reports the sound played or `stop` takes it back. With
    /// neither, at the sound's length and `PLAY_GRACE`, but never past `ANSWER_KEEP`: a
    /// sound the phone fetched then counts as played, one it never fetched as failed.
    /// Before the call opens, offers nothing and resolves at once: the phone shows the line.
    async fn play_said(&self, wav: &Path, said: &Said) -> Result<(), String> {
        if !self.opened() {
            return Ok(());
        }
        let limit = (sound_length(wav) + PLAY_GRACE).min(ANSWER_KEEP);
        let (offered, taken_back) = self.line.offer(wav, said);
        let _ = timeout(limit, taken_back).await;
        // At the limit, a sound still offered fails unless the phone fetched it. One already
        // taken back was reported played or ended by `stop`, perhaps just as the limit passed.
        match offered.take_back() {
            Some(false) => Err("not fetched by the phone".to_string()),
            Some(true) | None => Ok(()),
        }
    }

    fn stop(&self) {
        self.line.take_back_sounds();
    }
}

/// A call's ports for the phone: its audio, sounds and voice go through the `PhoneLine`, and
/// the rest is the daemon's, through `base`.
pub(crate) struct PhonePorts<B: CallPorts> {
    base: B,
    line: Arc<PhoneLine>,
    sink: Arc<PhoneSink>,
}

impl<B: CallPorts> PhonePorts<B> {
    pub(super) fn new(base: B, line: Arc<PhoneLine>) -> Self {
        let sink = Arc::new(PhoneSink::unopened(Arc::clone(&line)));
        Self { base, line, sink }
    }
}

impl<B: CallPorts> CallPorts for PhonePorts<B> {
    type Source = PhoneSource;
    type Sink = PhoneSink;
    type Worker = B::Worker;
    type Panes = B::Panes;
    type Awake = ();

    fn open_source(&self) -> Result<PhoneSource, String> {
        Ok(self.line.open_source())
    }

    fn sink(&self) -> Arc<PhoneSink> {
        Arc::clone(&self.sink)
    }

    /// The call opens once its worker has started; from then on sounds and lines are offered.
    fn start_worker(&self, call_dir: &Path) -> Result<B::Worker, WorkerError> {
        let worker = self.base.start_worker(call_dir)?;
        self.sink.opened.store(true, Ordering::SeqCst);
        Ok(worker)
    }

    fn panes(&self) -> &B::Panes {
        self.base.panes()
    }

    fn chief_folder(&self) -> Option<String> {
        self.base.chief_folder()
    }

    /// Says `line` in the phone's own voice: resolves when the phone reports it said, or
    /// after `SPEECH_BASE` and `SPEECH_PER_CHAR` for each character. Before the call opens,
    /// offers nothing and resolves at once, as the sink does.
    async fn speak_with_tts(&self, line: &str) {
        if !self.sink.opened() {
            return;
        }
        let chars = u32::try_from(line.chars().count()).unwrap_or(u32::MAX);
        let limit = SPEECH_BASE + SPEECH_PER_CHAR.saturating_mul(chars);
        let (_waiting, taken_back) = self.line.offer_speech(line);
        let _ = timeout(limit, taken_back).await;
    }

    fn stop_desk_line(&self) {
        self.line.take_back_speech();
    }

    async fn turn_end_update(&self, prompt: &str) -> String {
        self.base.turn_end_update(prompt).await
    }

    fn calls_dir(&self) -> PathBuf {
        self.base.calls_dir()
    }

    fn phrases_dir(&self) -> PathBuf {
        self.base.phrases_dir()
    }

    fn voice_name(&self) -> String {
        self.base.voice_name()
    }

    /// Muting Harold's voice silences the Mac's speakers, not the phone.
    fn voice_muted(&self) -> bool {
        false
    }

    /// The phone keeps its own screen on.
    fn keep_awake(&self) -> Result<(), String> {
        Ok(())
    }

    fn call_ended(&self, reason: &'static str) {
        self.sink.opened.store(false, Ordering::SeqCst);
        info!(reason, "app call ended");
        self.line.view.send_modify(|view| {
            *view = PhoneView {
                state: CallState::Off,
                transcript: None,
                sound: None,
                speech: None,
                ended: Some(reason),
            }
        });
    }

    fn no_answer_phrase(&self) -> &'static str {
        NO_ANSWER_IN_THREAD
    }
}

/// What a START answers: the call's status and, while the call listens, the id the
/// phone's pieces for it carry.
#[derive(Debug, Clone)]
pub(crate) struct PhoneStarted {
    pub status: CallStatus,
    pub call: Option<String>,
}

/// The phone's calls, as the web app holds them: each runs on the shared core through the
/// phone's ports, and while it is open the view follows its state and transcript.
pub(crate) struct PhoneCalls<B: CallPorts> {
    calls: Arc<CallControl<PhonePorts<B>>>,
    line: Arc<PhoneLine>,
    set_up: bool,
    /// The task that copies the open call's status into the view: one for each call.
    follower: Mutex<Option<JoinHandle<()>>>,
}

impl<B: CallPorts> LineTaker for PhoneCalls<B> {
    fn say_blocking(&self, line: &str, by: Speaker) -> Option<bool> {
        PhoneCalls::say_blocking(self, line, by)
    }
}

impl<B: CallPorts> PhoneCalls<B> {
    pub(crate) fn new(base: B, set_up: bool) -> Self {
        let line = PhoneLine::new(set_up);
        let ports = PhonePorts::new(base, Arc::clone(&line));
        let calls = Arc::new(CallControl::new(ports, set_up));
        let forgets = Arc::downgrade(&calls);
        line.set_forget_turn(move || {
            if let Some(calls) = forgets.upgrade() {
                calls.forget_turn();
            }
        });
        Self {
            calls,
            line,
            set_up,
            follower: Mutex::default(),
        }
    }

    /// Starts a call as `CallControl::start` does, and once it listens has the view follow
    /// it, answering the id its pieces carry. The phone stamps its pieces from `asked`, when
    /// it asked for the call: before a desk call ended, so the Mac's clock is not behind the
    /// phone's. With a call already open, a newer screen takes it over: the same call goes
    /// on with a new id, its pieces stamped from `asked`.
    pub(crate) async fn start_at(&self, asked: Instant) -> PhoneStarted {
        self.start_with(Some(asked)).await
    }

    /// The same with the pieces stamped from when the source opens, or, for a takeover,
    /// from now.
    #[cfg(test)]
    pub(crate) async fn start(&self) -> CallStatus {
        self.start_with(None).await.status
    }

    async fn start_with(&self, asked: Option<Instant>) -> PhoneStarted {
        if self.set_up {
            self.forget_ended_call();
        }
        let screen = {
            let mut inner = lock(&self.line.inner);
            inner.asked = asked;
            inner.screen
        };
        let status = self.calls.start().await;
        let (unread, opened) = {
            let mut inner = lock(&self.line.inner);
            // A call already open, or one that never opened its source, leaves it unread.
            (inner.asked.take(), inner.screen != screen)
        };
        if matches!(status.state, CallState::Off | CallState::NotSetUp) {
            return PhoneStarted { status, call: None };
        }
        let call = if opened {
            lock(&self.line.inner).call.clone()
        } else {
            // A takeover means the user starts again: what the older screen was saying is
            // dropped. The source forgets it again before the newer screen's first piece, in
            // case a piece of the older screen's reaches the call after this.
            let call = self.line.take_over(unread.unwrap_or_else(Instant::now));
            self.calls.forget_turn();
            call
        };
        let started = PhoneStarted { status, call };
        let mut follower = lock(&self.follower);
        if follower.as_ref().is_some_and(|task| !task.is_finished()) {
            return started;
        }
        info!("app call started");
        let calls = Arc::clone(&self.calls);
        let line = Arc::clone(&self.line);
        *follower = Some(tokio::spawn(follow(calls, line)));
        started
    }

    /// Unless a call is open, lets go of the last call's follower and clears why it ended
    /// from the view, so the next call's status shows.
    fn forget_ended_call(&self) {
        let mut follower = lock(&self.follower);
        let following = follower.as_ref().is_some_and(|task| !task.is_finished());
        if following && self.line.view.borrow().ended.is_none() {
            return;
        }
        if let Some(task) = follower.take() {
            task.abort();
        }
        self.line
            .view
            .send_if_modified(|view| view.ended.take().is_some());
    }

    pub(crate) async fn stop(&self) -> CallStatus {
        self.calls.stop().await
    }

    pub(crate) fn talk_now(&self) -> CallStatus {
        self.calls.talk_now()
    }

    #[cfg(test)]
    pub(crate) fn status(&self) -> CallStatus {
        self.calls.status()
    }

    /// Ends an open call with reason `shutdown`, and its follower with it.
    pub(crate) async fn shutdown(&self) {
        self.calls.shutdown().await;
        if let Some(task) = lock(&self.follower).take() {
            task.abort();
        }
    }

    /// Plays the chief's `line` on the phone, from a blocking thread, as
    /// `CallControl::say_blocking` does. `None` for another agent's line: those are still
    /// said at the desk.
    pub(crate) fn say_blocking(&self, line: &str, by: Speaker) -> Option<bool> {
        if by == Speaker::OtherAgent {
            return None;
        }
        self.calls.say_blocking(line, by)
    }

    /// The same from async code, as `CallControl::say` does.
    #[cfg(test)]
    pub(crate) async fn say(&self, line: &str, by: Speaker) -> Option<bool> {
        if by == Speaker::OtherAgent {
            return None;
        }
        self.calls.say(line, by).await
    }

    pub(crate) fn audio(&self, call: &str, at_ms: u64, bytes: &[u8]) -> Result<(), AudioRefused> {
        self.line.audio(call, at_ms, bytes)
    }

    pub(crate) fn played(&self, id: &str) {
        self.line.played(id);
    }

    pub(crate) fn answer(&self, id: &str) -> Option<Vec<u8>> {
        self.line.answer(id)
    }

    pub(crate) fn view(&self) -> watch::Receiver<PhoneView> {
        self.line.view()
    }

    pub(crate) fn set_up(&self) -> bool {
        self.set_up
    }

    /// The open call's source's base, so a test can stamp the pieces it posts.
    #[cfg(test)]
    pub(crate) fn source_base(&self) -> Option<Instant> {
        self.line.source_base()
    }

    /// The id the open call's pieces carry, so a test can post them.
    #[cfg(test)]
    pub(crate) fn call_id(&self) -> Option<String> {
        self.line.call_id()
    }

    /// Whether a follower still copies a call's status into the view.
    #[cfg(test)]
    pub(crate) fn is_following(&self) -> bool {
        lock(&self.follower)
            .as_ref()
            .is_some_and(|task| !task.is_finished())
    }
}

/// Copies the call's state and transcript into the view every `STATUS_EVERY` until the call
/// ends. The core reports the end through `call_ended` before its status reads off, so once
/// the view shows the end nothing more is copied: a status read just before would otherwise
/// put the ended call back on the screen.
async fn follow<B: CallPorts>(calls: Arc<CallControl<PhonePorts<B>>>, line: Arc<PhoneLine>) {
    loop {
        let status = calls.status();
        if status.state == CallState::Off {
            return;
        }
        let mut ended = false;
        line.view.send_if_modified(|view| {
            if view.ended.is_some() {
                ended = true;
                return false;
            }
            let changed = view.state != status.state || view.transcript != status.last_transcript;
            view.state = status.state;
            view.transcript = status.last_transcript;
            changed
        });
        if ended {
            return;
        }
        sleep(STATUS_EVERY).await;
    }
}
