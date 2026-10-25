//! The call routes: the phone starts and ends a call with the chief, posts its audio in
//! pieces, fetches and reports the chief's answers, and talks now. No route logs or stores
//! the audio, a transcript or a line.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use datastar::prelude::PatchSignals;
use serde::Deserialize;
use tokio::sync::watch;
use tokio::time::Instant;
use tracing::{info, warn};
use uuid::Uuid;

use super::AppState;
use super::route::now_ms;
use crate::store::HaroldStore;
use crate::voice_input::{
    AudioRefused, CallPorts, CallState, CallStatus, LineTaker, NO_CHIEF, PhoneCalls, PhoneStarted,
    PhoneView, Speaker, TurnResult, VOICE_FAILED,
};

pub(crate) type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The phone's calls as the routes reach them: `AppCalls` in the daemon.
pub(crate) trait PhoneCallApi: Send + Sync {
    /// Whether `[voice_input]` is set, so a call can start.
    fn set_up(&self) -> bool;
    /// Starts a call, or takes the open one over, and answers its status and its new id.
    fn start(&self) -> BoxFuture<'_, PhoneStarted>;
    /// Ends the call, and answers once it has ended and cleaned up. With `no_sound` the page
    /// found no sound from the microphone: the call ends as `microphone_failed`, not `stopped`.
    fn stop(&self, no_sound: bool) -> BoxFuture<'_, CallStatus>;
    fn talk_now(&self);
    /// A piece for the call `call`; one for any other call is refused as `NoCall`.
    fn audio(&self, call: &str, at_ms: u64, bytes: &[u8]) -> Result<(), AudioRefused>;
    /// The phone has played the sound, or said the line, `id`; `start_ms` is the milliseconds
    /// from the sound being offered to it starting on the phone.
    fn played_at(&self, id: &str, start_ms: Option<u64>);
    fn answer(&self, id: &str) -> Option<Vec<u8>>;
    /// What the phone shows and plays, now and as it changes.
    fn view(&self) -> watch::Receiver<PhoneView>;
    /// Ends an open call with reason `shutdown`.
    fn shutdown(&self) -> BoxFuture<'_, ()>;
    /// Has the voice worker loaded while `visible` counts a page on screen. Called once, when
    /// the web app starts.
    fn watch_pages(&self, visible: Arc<dyn Fn() -> usize + Send + Sync>);
}

/// Ends a call open at the desk, and resolves once it has ended.
pub(crate) type EndDeskCall = Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>;

/// `POST /call`'s answer when no call can start at all.
const NOT_SET_UP: &str = "Voice is not set up on the Mac";

/// The phone's calls, as the web app holds them. Starting a phone call ends a call open at
/// the desk first, so the chief does not get turns from the desk's microphone too; and a
/// chief's line played on the phone is added to the thread, with no push.
pub(crate) struct AppCalls<B: CallPorts> {
    phone: PhoneCalls<B>,
    store: Arc<HaroldStore>,
    end_desk_call: EndDeskCall,
    /// The daemon's runtime. Lines arrive on a blocking thread, which writes the thread
    /// with `block_on`.
    runtime: tokio::runtime::Handle,
}

impl<B: CallPorts> AppCalls<B> {
    /// Call inside the daemon's runtime: the calls keep a handle to it.
    pub(crate) fn new(
        phone: PhoneCalls<B>,
        store: Arc<HaroldStore>,
        end_desk_call: EndDeskCall,
    ) -> Self {
        Self {
            phone,
            store,
            end_desk_call,
            runtime: tokio::runtime::Handle::current(),
        }
    }
}

impl<B: CallPorts> PhoneCallApi for AppCalls<B> {
    fn set_up(&self) -> bool {
        self.phone.set_up()
    }

    fn start(&self) -> BoxFuture<'_, PhoneStarted> {
        // Taken before the desk call ends: the phone stamps its pieces from just before it
        // sent `POST /call`, so the call's clock starts as the request arrives.
        let asked = Instant::now();
        Box::pin(async move {
            (self.end_desk_call)().await;
            self.phone.start_at(asked).await
        })
    }

    fn stop(&self, no_sound: bool) -> BoxFuture<'_, CallStatus> {
        Box::pin(self.phone.stop(no_sound))
    }

    fn talk_now(&self) {
        self.phone.talk_now();
    }

    fn audio(&self, call: &str, at_ms: u64, bytes: &[u8]) -> Result<(), AudioRefused> {
        self.phone.audio(call, at_ms, bytes)
    }

    fn played_at(&self, id: &str, start_ms: Option<u64>) {
        self.phone.played_at(id, start_ms);
    }

    fn answer(&self, id: &str) -> Option<Vec<u8>> {
        self.phone.answer(id)
    }

    fn view(&self) -> watch::Receiver<PhoneView> {
        self.phone.view()
    }

    fn shutdown(&self) -> BoxFuture<'_, ()> {
        Box::pin(self.phone.shutdown())
    }

    fn watch_pages(&self, visible: Arc<dyn Fn() -> usize + Send + Sync>) {
        self.phone.watch_pages(visible);
    }
}

impl<B: CallPorts> LineTaker for AppCalls<B> {
    /// Plays the chief's line on the phone and, once played, adds it to the thread, where
    /// the phone reads it after the call. A failed write is logged without the line.
    fn say_blocking(&self, line: &str, by: Speaker) -> Option<bool> {
        let played = self.phone.say_blocking(line, by)?;
        if played && by == Speaker::Chief {
            let event_id = format!("call-line-{}", Uuid::new_v4().simple());
            let written =
                self.runtime
                    .block_on(self.store.add_chief_app_message(&event_id, now_ms(), line));
            if written.is_err() {
                warn!("app thread write failed");
            }
        }
        Some(played)
    }
}

/// `?call=<id>&at=<ms>` on a posted piece: the call it is for, as `POST /call` answered,
/// and when it was recorded, in milliseconds from the call's start.
#[derive(Deserialize)]
pub(crate) struct AudioAt {
    call: Option<String>,
    at: u64,
}

/// `?id=<id>` on a played report.
#[derive(Deserialize)]
pub(crate) struct PlayedId {
    id: Option<String>,
    /// Read as text, so a value that is no number leaves it out instead of refusing the report.
    start_ms: Option<String>,
}

/// The longest gap the phone can report: a minute.
const START_MS_LIMIT: i64 = 60_000;

/// A reported `start_ms`, held to 0 to 60,000; `None` when it is no whole number.
fn read_start_ms(raw: &str) -> Option<u64> {
    let ms = raw.parse::<i64>().ok()?;
    Some(ms.clamp(0, START_MS_LIMIT).unsigned_abs())
}

/// `POST /call`: always `200` as Datastar events, which the page's `@post` reads. Once the
/// call listens the `_callId` signal is patched to the call's id, which each piece of its
/// audio carries; a call already open is taken over: it goes on with a new id, and pieces
/// carrying the older one are refused. Otherwise `_callWhy` is patched to the line that
/// gives the reason no call started.
pub(crate) async fn post_call(State(state): State<AppState>) -> Response {
    let Some(calls) = state.calls.as_ref().filter(|calls| calls.set_up()) else {
        return call_signal("_callWhy", NOT_SET_UP);
    };
    let PhoneStarted { status, call } = calls.start().await;
    let why = match (status.state, status.last_result) {
        (CallState::NotSetUp, _) => NOT_SET_UP,
        (CallState::Off, Some(TurnResult::NoChief)) => NO_CHIEF,
        (CallState::Off, _) => VOICE_FAILED,
        // A call that listens always has an id; without one the phone could post nothing.
        _ => match call {
            Some(call) => return call_signal("_callId", &call),
            None => VOICE_FAILED,
        },
    };
    call_signal("_callWhy", why)
}

/// The answer to `POST /call`: the signal `name` patched to `value`.
fn call_signal(name: &str, value: &str) -> Response {
    let signal = serde_json::json!({ (name): value }).to_string();
    super::datastar_events([PatchSignals::new(signal)])
}

/// `POST /call/audio?call=<id>&at=<ms>`: a piece of little-endian `i16` audio for the call
/// `id`. `204` once taken; `400` without a numeric `at`; `409` without the open call's id;
/// else the refusal's status.
pub(crate) async fn post_audio(
    State(state): State<AppState>,
    at: Result<Query<AudioAt>, QueryRejection>,
    body: Bytes,
) -> Response {
    let Some(calls) = &state.calls else {
        return StatusCode::CONFLICT.into_response();
    };
    let Ok(Query(AudioAt { call, at })) = at else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Some(call) = call else {
        return refusal_status(AudioRefused::NoCall).into_response();
    };
    match calls.audio(&call, at, &body) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(refused) => refusal_status(refused).into_response(),
    }
}

/// The status a refused piece answers.
pub(crate) fn refusal_status(refused: AudioRefused) -> StatusCode {
    match refused {
        AudioRefused::NoCall => StatusCode::CONFLICT,
        AudioRefused::Malformed => StatusCode::BAD_REQUEST,
        AudioRefused::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
        AudioRefused::Full => StatusCode::SERVICE_UNAVAILABLE,
    }
}

/// `POST /call/played?id=<id>`: the phone has played the sound, or said the line, `id`.
/// `204` whether or not it was still offered: a late report changes nothing.
pub(crate) async fn post_played(
    State(state): State<AppState>,
    id: Result<Query<PlayedId>, QueryRejection>,
) -> Response {
    let Some(calls) = &state.calls else {
        return StatusCode::CONFLICT.into_response();
    };
    if let Ok(Query(PlayedId {
        id: Some(id),
        start_ms,
    })) = id
    {
        calls.played_at(&id, start_ms.as_deref().and_then(read_start_ms));
    }
    StatusCode::NO_CONTENT.into_response()
}

/// `POST /call/talk-now`: stops what plays and listens at once.
pub(crate) async fn post_talk_now(State(state): State<AppState>) -> Response {
    let Some(calls) = &state.calls else {
        return StatusCode::CONFLICT.into_response();
    };
    calls.talk_now();
    StatusCode::NO_CONTENT.into_response()
}

/// `?why=` on `DELETE /call`. Only `no_sound` means anything; any other value is a plain stop.
#[derive(Deserialize)]
pub(crate) struct StopWhy {
    why: Option<String>,
}

/// `DELETE /call`: ends the call, and answers `204` once it has ended and cleaned up. With
/// `?why=no_sound` the call ends as `microphone_failed`, else as `stopped`.
pub(crate) async fn delete_call(
    State(state): State<AppState>,
    why: Result<Query<StopWhy>, QueryRejection>,
) -> Response {
    let Some(calls) = &state.calls else {
        return StatusCode::CONFLICT.into_response();
    };
    let no_sound = matches!(why, Ok(Query(StopWhy { why: Some(why) })) if why == "no_sound");
    calls.stop(no_sound).await;
    StatusCode::NO_CONTENT.into_response()
}

/// The facts the page keeps about a call's microphone, posted once 3 seconds in, or when
/// `start()` threw. Nothing here is free text: `check_words` and `check_counts` hold each
/// value to its known words or numbers before anything is logged.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CallCheck {
    problem: Option<String>,
    step: Option<String>,
    error_name: Option<String>,
    context_state: Option<String>,
    track_state: Option<String>,
    track_muted: Option<bool>,
    pieces_made: Option<u32>,
    pieces_posted: Option<u32>,
    pieces_dropped: Option<DroppedPieces>,
    posts_failed: Option<u32>,
    empty_blocks: Option<u32>,
}

/// The pieces call.js dropped before posting, by reason.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DroppedPieces {
    playing: u32,
    echo_tail: u32,
    before_start: u32,
    not_open: u32,
}

const CHECK_PROBLEMS: &[&str] = &["no_sound"];
const CHECK_STEPS: &[&str] = &["microphone", "context", "worklet", "call"];
const CONTEXT_STATES: &[&str] = &["running", "suspended", "closed", "interrupted"];
const TRACK_STATES: &[&str] = &["live", "ended", "none"];
/// The most any one count may be.
const CHECK_COUNT_LIMIT: u32 = 100_000;
/// The longest browser error name accepted.
const ERROR_NAME_LIMIT: usize = 40;

impl CallCheck {
    /// Whether every word is one of its known values, `error_name` is 1 to 40 ASCII letters
    /// and every count is 0 to 100,000.
    fn valid(&self) -> bool {
        let word = |value: &Option<String>, known: &[&str]| {
            value.as_deref().is_none_or(|value| known.contains(&value))
        };
        let name = self.error_name.as_deref().is_none_or(|name| {
            (1..=ERROR_NAME_LIMIT).contains(&name.len())
                && name.chars().all(|c| c.is_ascii_alphabetic())
        });
        let dropped = self
            .pieces_dropped
            .as_ref()
            .map(|d| [d.playing, d.echo_tail, d.before_start, d.not_open]);
        let counts = [
            self.pieces_made,
            self.pieces_posted,
            self.posts_failed,
            self.empty_blocks,
        ]
        .into_iter()
        .flatten()
        .chain(dropped.into_iter().flatten());
        word(&self.problem, CHECK_PROBLEMS)
            && word(&self.step, CHECK_STEPS)
            && word(&self.context_state, CONTEXT_STATES)
            && word(&self.track_state, TRACK_STATES)
            && name
            && counts.into_iter().all(|count| count <= CHECK_COUNT_LIMIT)
    }

    /// The dropped counts as one fixed-shape line, such as `playing=0 echo_tail=2
    /// before_start=0 not_open=0`, built from the validated numbers only.
    fn dropped_line(&self) -> Option<String> {
        let d = self.pieces_dropped.as_ref()?;
        Some(format!(
            "playing={} echo_tail={} before_start={} not_open={}",
            d.playing, d.echo_tail, d.before_start, d.not_open
        ))
    }

    fn log(&self) {
        let dropped = self.dropped_line();
        let dropped = dropped.as_deref();
        if self.problem.is_some() || self.step.is_some() {
            warn!(
                problem = self.problem.as_deref(),
                step = self.step.as_deref(),
                error_name = self.error_name.as_deref(),
                context_state = self.context_state.as_deref(),
                track_state = self.track_state.as_deref(),
                track_muted = self.track_muted,
                pieces_made = self.pieces_made,
                pieces_posted = self.pieces_posted,
                pieces_dropped = dropped,
                posts_failed = self.posts_failed,
                empty_blocks = self.empty_blocks,
                "app call microphone failed"
            );
        } else {
            info!(
                error_name = self.error_name.as_deref(),
                context_state = self.context_state.as_deref(),
                track_state = self.track_state.as_deref(),
                track_muted = self.track_muted,
                pieces_made = self.pieces_made,
                pieces_posted = self.pieces_posted,
                pieces_dropped = dropped,
                posts_failed = self.posts_failed,
                empty_blocks = self.empty_blocks,
                "app call check"
            );
        }
    }
}

/// `POST /call/check`: the page's facts about the call's microphone. `204` once logged;
/// `400`, and nothing logged, for a body that is not the known JSON or holds a value out of
/// range.
pub(crate) async fn post_check(
    State(state): State<AppState>,
    check: Result<Json<CallCheck>, JsonRejection>,
) -> Response {
    if state.calls.is_none() {
        return StatusCode::CONFLICT.into_response();
    }
    match check {
        Ok(Json(check)) if check.valid() => {
            check.log();
            StatusCode::NO_CONTENT.into_response()
        }
        _ => StatusCode::BAD_REQUEST.into_response(),
    }
}

/// `GET /call/answer/{id}`: the offered sound's WAV, which counts as fetched from now on;
/// `404` once it is no longer offered.
pub(crate) async fn get_answer(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let Some(calls) = &state.calls else {
        return StatusCode::CONFLICT.into_response();
    };
    match calls.answer(&id) {
        Some(wav) => ([(header::CONTENT_TYPE, "audio/wav")], wav).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[cfg(test)]
#[path = "call_tests.rs"]
mod tests;
