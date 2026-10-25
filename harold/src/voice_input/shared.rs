//! What the web app, the projector and the settings know of a call, in every build. The
//! calls themselves (`calls` feature) build on these; without that feature the types stay,
//! so the web app's call routes compile and answer that no call is set up.

// Without `calls`, nothing makes most of these values.
#![cfg_attr(not(feature = "calls"), allow(dead_code))]

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

/// What a call is doing, as the dashboard's button shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CallState {
    /// No `[voice_input]`: no call can start.
    NotSetUp,
    Off,
    /// The call is open and the voice worker cannot listen yet: it is loading, or a speech
    /// model is still being fetched.
    Connecting,
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
    /// The call answerer's line played, and nothing was handed over.
    Answered,
    /// The call answerer's line played, with a hand-over to the chief.
    HandedOver,
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
            Self::Answered => "answered",
            Self::HandedOver => "handed_over",
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
    /// How far a speech model's download has come, in whole percent, while the call
    /// connects.
    pub model_percent: Option<u8>,
}

/// Who sent a line said into an open call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Speaker {
    /// `harold speak` from the chief: the answer to the user's turn.
    Chief,
    /// Another agent's turn-end update.
    OtherAgent,
}

/// Somewhere the chief's spoken lines can be said: a set of calls, of which one may be open.
pub(crate) trait LineTaker: Send + Sync {
    /// Says `line` in the open call, from a blocking thread, and returns once it has ended.
    /// `None` when no call is open.
    fn say_blocking(&self, line: &str, by: Speaker) -> Option<bool>;
}

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
    /// How far a speech model's download has come, while the call connects.
    pub model_percent: Option<u8>,
    /// The speech model that is not on the Mac, when the last call ended because of it.
    pub missing: Option<Missing>,
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

/// What a START answers: the call's status and, while the call listens, the id the
/// phone's pieces for it carry.
#[derive(Debug, Clone)]
pub(crate) struct PhoneStarted {
    pub status: CallStatus,
    pub call: Option<String>,
}

/// A speech model that is not on the Mac, with what the phone shows about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Missing {
    pub model: String,
    pub reason: &'static str,
    /// The command that fetches it by hand, when the model and commit are fit to show.
    pub command: Option<String>,
}

impl Missing {
    /// The notice on the phone's screen.
    pub(crate) fn notice(&self) -> String {
        let mut text = format!(
            "The speech model {} didn't download ({}).",
            self.model, self.reason
        );
        if let Some(command) = &self.command {
            text.push_str(&format!(
                " To get it, run on the Mac: {command}, then call again."
            ));
        }
        text
    }
}

/// `text` without a leading `H<1 to 4 digits>:` and the spaces after it: the tag the chief
/// puts on its report so Harold can match it to a hand-over, which nobody should hear or
/// read. A tag with nothing after it, and any other line, comes back as it is.
pub(crate) fn without_hand_over_tag(text: &str) -> &str {
    let Some(rest) = text.strip_prefix('H') else {
        return text;
    };
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    if !(1..=4).contains(&digits) {
        return text;
    }
    let Some(after) = rest[digits..].strip_prefix(':') else {
        return text;
    };
    let said = after.trim_start_matches(' ');
    if said.is_empty() { text } else { said }
}

/// The web app's `POST /call` says this when there is no chief to take the turns.
pub(crate) const NO_CHIEF: &str = "There's no chief running.";
/// Said through the `[tts]` command, never by the worker.
pub(crate) const VOICE_FAILED: &str = "The voice failed. Call ended.";
