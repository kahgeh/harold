//! The echo test: a hidden page that measures how much of Harold's voice the phone's mic
//! picks up while the phone plays it. The page sends one number for every 20 ms of what the
//! mic heard, never the sound; this module judges the numbers, keeps each run in a file and
//! makes the sample the phone plays.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
#[cfg(test)]
use std::time::Duration;

use askama::Template;
use axum::body::{Body, to_bytes};
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tokio::sync::Mutex as AsyncMutex;
use tracing::{info, warn};

use super::AppState;
use super::route::now_ms;
use crate::outbound::voice_note::{MODEL_TIMEOUT, Runner};

// The speech detector's own numbers (`voice_input/speech.rs`), which it keeps private.
/// Frames, from the first, that set the background.
const CALIBRATION_FRAMES: usize = 25;
/// How many times the background a frame must be to count as loud.
const LOUDNESS_FACTOR: f64 = 3.0;
/// The lowest background taken, so a silent room does not give a bar of 0.
const BACKGROUND_FLOOR: f64 = 30.0;

/// A run's most frames, at 20 ms each: 20 seconds.
pub(crate) const MAX_FRAMES: usize = 1000;
/// The most bytes a run's body may hold.
pub(crate) const MAX_BODY_BYTES: usize = 64 * 1024;
/// How long a silent run counts for the talk runs after it.
pub(crate) const HOUR_MS: i64 = 60 * 60 * 1000;
/// The share of playback frames over the bar that a silent run may have and still pass.
const SILENT_SHARE_LIMIT_PERCENT: usize = 5;
/// A talk run's bar, as a multiple of the silent run's 95th-percentile echo level.
const TALK_FACTOR: f64 = 2.0;
/// The window a talk run's voice must show in, and how many of its frames must be over the
/// bar: 10 of 15 frames in a row, 0.3 s.
const TALK_WINDOW: usize = 15;
const TALK_FRAMES_OVER: usize = 10;
/// The longest `user_agent` kept in a run's file, in characters.
const USER_AGENT_LIMIT: usize = 300;

/// The words `say` reads for the sample: about 15 seconds at its usual pace.
const SAMPLE_TEXT: &str = "Good morning. This is Harold, checking that your phone can tell \
    my voice from yours. While I talk, please keep still and say nothing, then talk over me \
    in the second run. Nothing you hear is recorded, and nothing leaves your Mac except \
    a few loudness numbers. Thank you for your patience.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Setup {
    Ear,
    Speaker,
    Headphones,
}

impl Setup {
    fn word(self) -> &'static str {
        match self {
            Setup::Ear => "ear",
            Setup::Speaker => "speaker",
            Setup::Headphones => "headphones",
        }
    }

    /// Where the phone is, in the verdict's line.
    fn place(self) -> &'static str {
        match self {
            Setup::Ear => "held to your ear",
            Setup::Speaker => "on the table",
            Setup::Headphones => "with headphones",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Kind {
    Silent,
    Talk,
}

impl Kind {
    fn word(self) -> &'static str {
        match self {
            Kind::Silent => "silent",
            Kind::Talk => "talk",
        }
    }
}

/// One run as the page sends it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RunBody {
    pub setup: Setup,
    pub kind: Kind,
    /// The loudness of each 20 ms of what the mic heard: the root mean square of 320 samples.
    pub frames: Vec<f64>,
    /// The frames the sample played over: `playing_from` up to, not including, `playing_to`.
    pub playing_from: usize,
    pub playing_to: usize,
    /// What `getSettings()` reported for the mic track; none when it said nothing.
    #[serde(default)]
    pub echo_cancellation: Option<bool>,
    /// Whether the page runs as the installed app.
    #[serde(default)]
    pub standalone: bool,
    #[serde(default)]
    pub user_agent: String,
}

/// Reads a run from its body. Err for a body over `MAX_BODY_BYTES`, something that is not a
/// run, an unknown `setup` or `kind`, more than `MAX_FRAMES` frames, a negative frame, or a
/// playback window that is empty or outside the frames.
pub(crate) fn parse_run(body: &[u8]) -> Result<RunBody, ()> {
    if body.len() > MAX_BODY_BYTES {
        return Err(());
    }
    let run: RunBody = serde_json::from_slice(body).map_err(|_| ())?;
    let window_ok = run.playing_from < run.playing_to && run.playing_to <= run.frames.len();
    if run.frames.len() > MAX_FRAMES
        || !window_ok
        || run.frames.iter().any(|f| f.is_nan() || *f < 0.0)
    {
        return Err(());
    }
    Ok(run)
}

/// The background: the mean of the first `CALIBRATION_FRAMES` frames, at least
/// `BACKGROUND_FLOOR`, as the speech detector takes it.
pub(crate) fn background(frames: &[f64]) -> f64 {
    let first = &frames[..frames.len().min(CALIBRATION_FRAMES)];
    if first.is_empty() {
        return BACKGROUND_FLOOR;
    }
    (first.iter().sum::<f64>() / first.len() as f64).max(BACKGROUND_FLOOR)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Outcome {
    Pass,
    Fail,
    RunSilentFirst,
}

impl Outcome {
    fn word(self) -> &'static str {
        match self {
            Outcome::Pass => "pass",
            Outcome::Fail => "fail",
            Outcome::RunSilentFirst => "run_silent_first",
        }
    }
}

/// What Harold finds in a run.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct Verdict {
    #[serde(rename = "verdict")]
    pub outcome: Outcome,
    /// The sentence the page shows.
    pub line: String,
    pub background: f64,
    /// Silent runs: the share of playback frames over three times the background.
    pub share_over_bar: Option<f64>,
    /// Silent runs: the median and 95th-percentile level of the playback frames.
    pub median: Option<f64>,
    pub p95: Option<f64>,
    /// Talk runs with a bar: whether the voice stood out from the echo.
    pub voice_found: Option<bool>,
}

/// The value at nearest rank `fraction` of the sorted `levels`.
fn percentile(sorted: &[f64], fraction: f64) -> f64 {
    let rank = (fraction * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

fn median(sorted: &[f64]) -> f64 {
    let n = sorted.len();
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    }
}

/// The verdict on `run`. `silent_p95` is the 95th-percentile echo level of the newest silent
/// run with the same setup in the last hour, which a talk run needs.
pub(crate) fn judge(run: &RunBody, silent_p95: Option<f64>) -> Verdict {
    let background = background(&run.frames);
    let playback = &run.frames[run.playing_from..run.playing_to];
    let subject = match run.kind {
        Kind::Silent => "Silent",
        Kind::Talk => "Talking",
    };
    let place = run.setup.place();
    match run.kind {
        Kind::Silent => {
            let bar = LOUDNESS_FACTOR * background;
            let over = playback.iter().filter(|level| **level > bar).count();
            let mut sorted = playback.to_vec();
            sorted.sort_by(f64::total_cmp);
            let pass = over * 100 <= playback.len() * SILENT_SHARE_LIMIT_PERCENT;
            let share = over as f64 / playback.len() as f64;
            let outcome = if pass { Outcome::Pass } else { Outcome::Fail };
            Verdict {
                outcome,
                line: format!(
                    "{subject}, {place}: {}. The echo crossed the bar in {}% of frames.",
                    outcome.word(),
                    (share * 100.0).round()
                ),
                background,
                share_over_bar: Some(share),
                median: Some(median(&sorted)),
                p95: Some(percentile(&sorted, 0.95)),
                voice_found: None,
            }
        }
        Kind::Talk => {
            let Some(silent_p95) = silent_p95 else {
                return Verdict {
                    outcome: Outcome::RunSilentFirst,
                    line: format!(
                        "{subject}, {place}: run silent first. There is no silent run for this setup in the last hour."
                    ),
                    background,
                    share_over_bar: None,
                    median: None,
                    p95: None,
                    voice_found: None,
                };
            };
            let bar = TALK_FACTOR * silent_p95;
            let found = playback.windows(TALK_WINDOW).any(|window| {
                window.iter().filter(|level| **level > bar).count() >= TALK_FRAMES_OVER
            });
            let outcome = if found { Outcome::Pass } else { Outcome::Fail };
            let words = if found {
                "Your voice stood out from the echo."
            } else {
                "Your voice did not stand out from the echo."
            };
            Verdict {
                outcome,
                line: format!("{subject}, {place}: {}. {words}", outcome.word()),
                background,
                share_over_bar: None,
                median: None,
                p95: None,
                voice_found: Some(found),
            }
        }
    }
}

/// The silent runs of the last hour: when each ended, its setup and its 95th-percentile
/// echo level. Kept in memory; a restart empties it.
#[derive(Default)]
pub(crate) struct Recent(Mutex<Vec<(Setup, i64, f64)>>);

impl Recent {
    pub(crate) fn record(&self, setup: Setup, at_ms: i64, p95: f64) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((setup, at_ms, p95));
    }

    /// The level of the newest silent run with `setup` less than an hour before `now_ms`.
    pub(crate) fn newest_silent_p95(&self, setup: Setup, now_ms: i64) -> Option<f64> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|(s, at, _)| *s == setup && now_ms - *at < HOUR_MS)
            .max_by_key(|(_, at, _)| *at)
            .map(|(_, _, p95)| *p95)
    }
}

/// What the echo test routes need: where the runs go, how the sample is made, the sample
/// once made, and the silent runs of the last hour.
pub(crate) struct EchoTest {
    folder: PathBuf,
    runner: Arc<dyn Runner + Send + Sync>,
    sample: AsyncMutex<Option<Arc<Vec<u8>>>>,
    recent: Recent,
}

impl EchoTest {
    pub(crate) fn new(folder: PathBuf, runner: Arc<dyn Runner + Send + Sync>) -> Self {
        Self {
            folder,
            runner,
            sample: AsyncMutex::new(None),
            recent: Recent::default(),
        }
    }

    /// The sample, made by `say` at the first call and kept. None when `say` fails, and the
    /// next call tries again.
    async fn sample(&self) -> Option<Arc<Vec<u8>>> {
        let mut kept = self.sample.lock().await;
        if let Some(sample) = kept.as_ref() {
            return Some(Arc::clone(sample));
        }
        let made = self.make_sample().await;
        if made.is_none() {
            warn!(reason = "say_failed", "echo test sample not made");
        }
        *kept = made.clone();
        made
    }

    async fn make_sample(&self) -> Option<Arc<Vec<u8>>> {
        let folder = self.folder.clone();
        let runner = Arc::clone(&self.runner);
        tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&folder).ok()?;
            let out = folder.join(format!("sample-{}.wav", uuid::Uuid::new_v4()));
            let args = [
                "-o",
                &out.to_string_lossy(),
                "--file-format=WAVE",
                "--data-format=LEI16@24000",
                SAMPLE_TEXT,
            ]
            .map(String::from);
            let made = runner.run("say", &args, MODEL_TIMEOUT);
            let bytes = made.then(|| std::fs::read(&out).ok()).flatten();
            let _ = std::fs::remove_file(&out);
            bytes.filter(|bytes| !bytes.is_empty()).map(Arc::new)
        })
        .await
        .ok()
        .flatten()
    }
}

/// Where the runs go: `echo-test/` beside the store's folder, where `web-push/` lives.
pub(crate) fn echo_test_path(store_path: &Path) -> PathBuf {
    store_path
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .join("echo-test")
}

/// A runner that is never asked: the default for tests that do not use the echo test.
#[cfg(test)]
pub(crate) struct NeverRuns;

#[cfg(test)]
impl Runner for NeverRuns {
    fn run(&self, _program: &str, _args: &[String], _timeout: Duration) -> bool {
        false
    }
}

#[derive(Template)]
#[template(path = "web_app/echo_test.html")]
struct EchoTestPage {
    app_name: String,
}

/// `GET /echo-test`: the test page.
pub(crate) async fn get_page(State(state): State<AppState>) -> Response {
    let page = EchoTestPage {
        app_name: state.identity.name.clone(),
    };
    match page.render() {
        Ok(html) => Html(html).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// `GET /echo-test/sample`: 15 seconds of speech as `audio/wav`; `503` when `say` fails.
pub(crate) async fn get_sample(State(state): State<AppState>) -> Response {
    match state.echo_test.sample().await {
        Some(sample) => ([(header::CONTENT_TYPE, "audio/wav")], sample.to_vec()).into_response(),
        None => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

/// The run as it is written: what the page sent, then the verdict.
#[derive(Serialize)]
struct SavedRun<'a> {
    at_ms: i64,
    #[serde(flatten)]
    run: &'a RunBody,
    verdict: &'a Verdict,
}

/// Writes `saved` to a new file in `folder`, named `<UTC time>-<setup>-<kind>.json`.
fn write_run(folder: &Path, at_ms: i64, saved: &SavedRun<'_>) -> std::io::Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(folder)?;
    let stamp = OffsetDateTime::from_unix_timestamp_nanos(i128::from(at_ms) * 1_000_000)
        .map(|at| {
            format!(
                "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
                at.year(),
                u8::from(at.month()),
                at.day(),
                at.hour(),
                at.minute(),
                at.second()
            )
        })
        .unwrap_or_else(|_| at_ms.to_string());
    let json = serde_json::to_vec(saved).map_err(std::io::Error::other)?;
    let (setup, kind) = (saved.run.setup.word(), saved.run.kind.word());
    // A second run in the same second gets a number rather than replacing the first.
    for n in 1.. {
        let suffix = if n == 1 {
            String::new()
        } else {
            format!("-{n}")
        };
        let path = folder.join(format!("{stamp}{suffix}-{setup}-{kind}.json"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => return file.write_all(&json),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    unreachable!("the loop returns")
}

fn not_saved(status: StatusCode, reason: &'static str) -> Response {
    warn!(reason, "echo test run not saved");
    (
        status,
        axum::Json(serde_json::json!({ "saved": false, "reason": reason })),
    )
        .into_response()
}

/// `POST /echo-test/runs`: judges one run, writes it under `echo-test/` and answers
/// `{ saved, verdict, line, ... }`. `400` for a run `parse_run` refuses; `500` when the file
/// cannot be written.
pub(crate) async fn post_run(State(state): State<AppState>, body: Body) -> Response {
    let Ok(bytes) = to_bytes(body, MAX_BODY_BYTES + 1).await else {
        return not_saved(StatusCode::BAD_REQUEST, "bad_request");
    };
    let Ok(mut run) = parse_run(&bytes) else {
        return not_saved(StatusCode::BAD_REQUEST, "bad_request");
    };
    run.user_agent.truncate(
        run.user_agent
            .char_indices()
            .nth(USER_AGENT_LIMIT)
            .map_or(run.user_agent.len(), |(at, _)| at),
    );
    let echo = &state.echo_test;
    let at_ms = now_ms();
    let verdict = judge(&run, echo.recent.newest_silent_p95(run.setup, at_ms));
    let saved = SavedRun {
        at_ms,
        run: &run,
        verdict: &verdict,
    };
    let folder = echo.folder.clone();
    let written = {
        let json_run = serde_json::to_vec(&saved).map(|_| ());
        match json_run {
            Ok(()) => write_run(&folder, at_ms, &saved),
            Err(error) => Err(std::io::Error::other(error)),
        }
    };
    if written.is_err() {
        return not_saved(StatusCode::INTERNAL_SERVER_ERROR, "write_failed");
    }
    if let (Kind::Silent, Some(p95)) = (run.kind, verdict.p95) {
        echo.recent.record(run.setup, at_ms, p95);
    }
    log_run(&run, &verdict);
    let mut answer = serde_json::to_value(&verdict).unwrap_or_default();
    answer["saved"] = true.into();
    axum::Json(answer).into_response()
}

/// The `echo test run` line: fixed words, a share or a yes or no, never sound or text.
fn log_run(run: &RunBody, verdict: &Verdict) {
    let (setup, kind) = (run.setup.word(), run.kind.word());
    let echo_cancellation = run.echo_cancellation.unwrap_or(false);
    let standalone = run.standalone;
    let verdict_word = verdict.outcome.word();
    if let Some(share) = verdict.share_over_bar {
        let share_over_bar = (share * 10_000.0).round() / 10_000.0;
        info!(
            setup,
            kind,
            verdict = verdict_word,
            share_over_bar,
            echo_cancellation,
            standalone,
            "echo test run"
        );
    } else if let Some(voice_found) = verdict.voice_found {
        info!(
            setup,
            kind,
            verdict = verdict_word,
            voice_found,
            echo_cancellation,
            standalone,
            "echo test run"
        );
    } else {
        info!(
            setup,
            kind,
            verdict = verdict_word,
            echo_cancellation,
            standalone,
            "echo test run"
        );
    }
}
