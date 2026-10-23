//! The speech check: a one-off page the user runs once on their iPhone, in the installed app,
//! to learn how Safari's speech recognition behaves in a call-shaped use. The page sends
//! booleans, counts, milliseconds and the recognizer's error names, never a word the user
//! said; this module checks that and logs one `app speech check` line. Stage 1 of
//! call-phone-speech removes it.

use askama::Template;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;
use tracing::{info, warn};

use super::AppState;

/// The most bytes a run's body may hold: 4 KB.
pub(crate) const MAX_BODY_BYTES: usize = 4096;
/// The longest time a run may report, in milliseconds: ten minutes.
const MAX_MS: u32 = 10 * 60 * 1000;
/// Check 3 restarts recognition this many times.
const RESTARTS: usize = 5;
/// Check 4 has this many sentences.
const SENTENCES: usize = 3;
/// The most words of the played sentence that check 5 can count.
const MAX_WORDS: u8 = 20;

/// The recognizer's error names, and three the page adds for what it saw itself: `missing`
/// (the browser has no recognition), `start-failed` (calling `start()` threw) and `other` (an
/// error name the recognizer gave that is not in this list). Any other name is refused, so
/// nothing the user said can ride in a name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ErrorName {
    NoSpeech,
    Aborted,
    AudioCapture,
    Network,
    NotAllowed,
    ServiceNotAllowed,
    BadGrammar,
    LanguageNotSupported,
    Missing,
    StartFailed,
    Other,
}

impl ErrorName {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ErrorName::NoSpeech => "no-speech",
            ErrorName::Aborted => "aborted",
            ErrorName::AudioCapture => "audio-capture",
            ErrorName::Network => "network",
            ErrorName::NotAllowed => "not-allowed",
            ErrorName::ServiceNotAllowed => "service-not-allowed",
            ErrorName::BadGrammar => "bad-grammar",
            ErrorName::LanguageNotSupported => "language-not-supported",
            ErrorName::Missing => "missing",
            ErrorName::StartFailed => "start-failed",
            ErrorName::Other => "other",
        }
    }
}

/// One run as the page sends it: a flat set of `check<n>_<what>` fields, each a yes or no, a
/// count, milliseconds or an error name. A field the run did not reach is left out.
/// Anything not named here, or of another type, is refused.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct Run {
    /// Whether the page runs as the installed app.
    pub standalone: bool,

    /// Check 0, recognition with the network off.
    /// Whether the phone said it was offline when the check started.
    pub check0_offline: bool,
    pub check0_started: bool,
    pub check0_results: bool,
    pub check0_first_result_ms: Option<u32>,
    pub check0_error: Option<ErrorName>,

    /// Check 1, recognition starts inside a tap.
    pub check1_started: bool,
    pub check1_results: bool,
    pub check1_first_result_ms: Option<u32>,
    pub check1_error: Option<ErrorName>,

    /// Check 2, recognition starts 2.5 s after a tap.
    pub check2_started: bool,
    pub check2_results: bool,
    pub check2_error: Option<ErrorName>,

    /// Check 3, recognition restarts without a tap: how many of the 5 started, and the
    /// error names of those that did not.
    pub check3_started: u8,
    pub check3_errors: Vec<ErrorName>,

    /// Check 4, when results become final: whether any came, then for each of the three
    /// sentences the ms from the sentence's showing to its final result, and from the last
    /// interim result to that final one; null where no final came.
    pub check4_results: bool,
    pub check4_final_at_ms: Vec<Option<u32>>,
    pub check4_gap_ms: Vec<Option<u32>>,
    pub check4_error: Option<ErrorName>,

    /// Check 5, listening while Harold's voice plays.
    pub check5_started: bool,
    pub check5_kept_running: bool,
    pub check5_playback_ok: bool,
    /// How many of the played sentence's words recognition heard, compared on the phone.
    pub check5_words_heard: u8,
    pub check5_error: Option<ErrorName>,

    /// Check 6, listening after Harold's voice.
    pub check6_restarted: bool,
    pub check6_heard: bool,
    pub check6_error: Option<ErrorName>,
}

impl Run {
    /// Whether every number is inside its limit.
    fn within_limits(&self) -> bool {
        let times = [self.check0_first_result_ms, self.check1_first_result_ms];
        let sentence_times = self
            .check4_final_at_ms
            .iter()
            .chain(&self.check4_gap_ms)
            .copied();
        times
            .into_iter()
            .chain(sentence_times)
            .flatten()
            .all(|ms| ms <= MAX_MS)
            && usize::from(self.check3_started) <= RESTARTS
            && self.check3_errors.len() <= RESTARTS
            && self.check4_final_at_ms.len() <= SENTENCES
            && self.check4_gap_ms.len() <= SENTENCES
            && self.check5_words_heard <= MAX_WORDS
    }
}

/// Reads a run from its body. Err for a body over `MAX_BODY_BYTES`, something that is not a
/// JSON object of the fields above with the right types, an error name outside the list, or
/// a number outside its limit.
pub(crate) fn parse_run(body: &[u8]) -> Result<Run, ()> {
    if body.len() > MAX_BODY_BYTES {
        return Err(());
    }
    // serde would also read a struct from a JSON array, field by field in order.
    if body.trim_ascii_start().first() != Some(&b'{') {
        return Err(());
    }
    let run: Run = serde_json::from_slice(body).map_err(|_| ())?;
    run.within_limits().then_some(run).ok_or(())
}

/// The error names as one comma-separated word, `-` for none; the page's lists in the log.
fn names(errors: &[ErrorName]) -> String {
    if errors.is_empty() {
        return "-".into();
    }
    errors
        .iter()
        .map(|error| error.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

/// The milliseconds as one comma-separated word, `-` for a sentence with no final.
fn times(times: &[Option<u32>]) -> String {
    if times.is_empty() {
        return "-".into();
    }
    times
        .iter()
        .map(|ms| ms.map_or_else(|| "-".to_owned(), |ms| ms.to_string()))
        .collect::<Vec<_>>()
        .join(",")
}

/// The `app speech check` line: yes or no, counts, milliseconds and error names, never words.
fn log_run(run: &Run) {
    let error = |name: Option<ErrorName>| name.map(ErrorName::as_str);
    info!(
        standalone = run.standalone,
        check0_offline = run.check0_offline,
        check0_started = run.check0_started,
        check0_results = run.check0_results,
        check0_first_result_ms = run.check0_first_result_ms,
        check0_error = error(run.check0_error),
        check1_started = run.check1_started,
        check1_results = run.check1_results,
        check1_first_result_ms = run.check1_first_result_ms,
        check1_error = error(run.check1_error),
        check2_started = run.check2_started,
        check2_results = run.check2_results,
        check2_error = error(run.check2_error),
        check3_started = run.check3_started,
        check3_errors = names(&run.check3_errors),
        check4_results = run.check4_results,
        check4_final_at_ms = times(&run.check4_final_at_ms),
        check4_gap_ms = times(&run.check4_gap_ms),
        check4_error = error(run.check4_error),
        check5_started = run.check5_started,
        check5_kept_running = run.check5_kept_running,
        check5_playback_ok = run.check5_playback_ok,
        check5_words_heard = run.check5_words_heard,
        check5_error = error(run.check5_error),
        check6_restarted = run.check6_restarted,
        check6_heard = run.check6_heard,
        check6_error = error(run.check6_error),
        "app speech check"
    );
}

#[derive(Template)]
#[template(path = "web_app/speech_check.html")]
struct SpeechCheckPage {
    app_name: String,
}

/// `GET /speech-check`: the check page.
pub(crate) async fn get_page(State(state): State<AppState>) -> Response {
    let page = SpeechCheckPage {
        app_name: state.identity.name.clone(),
    };
    match page.render() {
        Ok(html) => Html(html).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// `POST /speech-check/runs`: logs one run and answers `204`. `400`, with nothing logged but
/// a warning, for a body `parse_run` refuses; the route's body limit answers `413` for a body
/// over 4 KB.
pub(crate) async fn post_run(body: Bytes) -> Response {
    let Ok(run) = parse_run(&body) else {
        warn!(reason = "bad_request", "app speech check not logged");
        return StatusCode::BAD_REQUEST.into_response();
    };
    log_run(&run);
    StatusCode::NO_CONTENT.into_response()
}
