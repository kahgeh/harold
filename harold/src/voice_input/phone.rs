//! The phone's side of a call: pieces of audio the app posts, sounds the app fetches and
//! plays, and lines it says in its own voice. The ports share one `PhoneLine` with the app's
//! call routes, which hand it what the phone posts and show the phone what waits for it.

use super::call::lock;
use super::phrases::NO_ANSWER_IN_THREAD;
use super::worker::WorkerError;
use super::{AudioSink, AudioSource, CallPorts, CallState, Said};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{Instant, sleep_until, timeout};
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
}

#[derive(Default)]
struct Inner {
    /// Where the open call's source reads its pieces from.
    source: Option<Pieces>,
    /// The offered sounds, by id.
    sounds: HashMap<String, Offer>,
    speech: Option<Speech>,
}

/// A piece of samples and when it was recorded.
type Piece = (Vec<i16>, Instant);

struct Pieces {
    sender: mpsc::Sender<Piece>,
    /// When the source opened: the phone stamps each piece in milliseconds from then.
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
    /// When the last piece arrived, or the last sound or line stopped waiting.
    since: Instant,
    /// Whether a sound or a line waits for the phone; the clock does not run meanwhile.
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
        })
    }

    /// Takes a piece the phone posted: little-endian `i16` samples recorded `at_ms`
    /// milliseconds after the call's source opened.
    pub(crate) fn audio(&self, at_ms: u64, bytes: &[u8]) -> Result<(), AudioRefused> {
        let mut inner = lock(&self.inner);
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
        match pieces.sender.try_send((samples, at)) {
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
        let mut inner = lock(&self.inner);
        let offer = inner.sounds.get_mut(id)?;
        let bytes = std::fs::read(&offer.wav).ok()?;
        offer.fetched = true;
        Some(bytes)
    }

    /// What the phone shows and plays, now and as it changes.
    pub(crate) fn view(&self) -> watch::Receiver<PhoneView> {
        self.view.subscribe()
    }

    /// A source for a new call. An older source ends once it has read what was posted to it.
    pub(super) fn open_source(&self) -> PhoneSource {
        let (sender, pieces) = mpsc::channel(PIECES_QUEUED);
        let base = Instant::now();
        lock(&self.inner).source = Some(Pieces { sender, base });
        self.quiet.send_modify(|quiet| quiet.since = base);
        PhoneSource {
            pieces,
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

    /// Takes back the sound or the line `id` if it still waits, and answers whether the
    /// phone had fetched it.
    fn take_back(&self, id: &str) -> bool {
        let mut inner = lock(&self.inner);
        let fetched = match inner.sounds.remove(id) {
            Some(offer) => offer.fetched,
            None => {
                if inner.speech.as_ref().is_some_and(|speech| speech.id == id) {
                    inner.speech = None;
                }
                false
            }
        };
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

    /// Restarts the silence clock when the last sound or line stops waiting.
    fn note_waiting(&self, inner: &Inner) {
        let waiting = !inner.sounds.is_empty() || inner.speech.is_some();
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
    /// Takes it back now, and answers whether the phone had fetched it.
    fn take_back(&self) -> bool {
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
    let Ok(bytes) = std::fs::read(wav) else {
        return Duration::ZERO;
    };
    let u32_at = |at: usize| {
        let field = bytes.get(at..at + 4)?;
        Some(u32::from_le_bytes([field[0], field[1], field[2], field[3]]))
    };
    let Some(byte_rate) = u32_at(28).filter(|rate| *rate > 0) else {
        return Duration::ZERO;
    };
    // The chunks follow `RIFF <size> WAVE`, each an id and a size, padded to an even length.
    let mut at = 12;
    while let Some(size) = u32_at(at + 4) {
        if &bytes[at..at + 4] == b"data" {
            return Duration::from_secs_f64(f64::from(size) / f64::from(byte_rate));
        }
        at += 8 + size as usize + size as usize % 2;
    }
    Duration::ZERO
}

/// The call's audio, as the phone posts it.
pub(crate) struct PhoneSource {
    pieces: mpsc::Receiver<Piece>,
    quiet: watch::Receiver<Quiet>,
    ended: bool,
}

impl AudioSource for PhoneSource {
    async fn next_block(&mut self) -> Option<Vec<i16>> {
        let (samples, _) = self.next_stamped().await?;
        Some(samples)
    }

    /// Ends `PHONE_SILENT_LIMIT` after the last piece, counting no time a sound or a line
    /// waited for the phone, or when a newer source replaced this one.
    async fn next_stamped(&mut self) -> Option<Piece> {
        while !self.ended {
            let quiet = *self.quiet.borrow_and_update();
            tokio::select! {
                biased;
                piece = self.pieces.recv() => match piece {
                    Some(piece) => return Some(piece),
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
}

impl PhoneSink {
    pub(super) fn new(line: Arc<PhoneLine>) -> Self {
        Self { line }
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
    async fn play_said(&self, wav: &Path, said: &Said) -> Result<(), String> {
        let limit = (sound_length(wav) + PLAY_GRACE).min(ANSWER_KEEP);
        let (offered, taken_back) = self.line.offer(wav, said);
        let ended_early = timeout(limit, taken_back).await.is_ok();
        let fetched = offered.take_back();
        if ended_early || fetched {
            Ok(())
        } else {
            Err("not fetched by the phone".to_string())
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
        let sink = Arc::new(PhoneSink::new(Arc::clone(&line)));
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

    fn start_worker(&self, call_dir: &Path) -> Result<B::Worker, WorkerError> {
        self.base.start_worker(call_dir)
    }

    fn panes(&self) -> &B::Panes {
        self.base.panes()
    }

    fn chief_folder(&self) -> Option<String> {
        self.base.chief_folder()
    }

    /// Says `line` in the phone's own voice: resolves when the phone reports it said, or
    /// after `SPEECH_BASE` and `SPEECH_PER_CHAR` for each character.
    async fn speak_with_tts(&self, line: &str) {
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
