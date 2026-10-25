//! The TTS server: one per machine. It turns text into audio for the desk voice, for calls and
//! for the CLI, through `harold.tts.v1.TtsService`, and keeps the list of TTS models on the
//! machine. A TTS worker (the MLX worker on macOS, the ONNX worker on Linux) keeps one model
//! loaded; Harold starts it on the first request and stops it after the idle time.
//!
//! The server makes audio; Harold plays it on the machine (`play`).
//!
//! The call hooks (warming, the call guard, the default model's status) are used by `calls` only.
#![cfg_attr(not(feature = "calls"), allow(dead_code))]

pub(crate) mod approved;
pub(crate) mod fetch;
pub(crate) mod hub;
pub(crate) mod models;
pub(crate) mod play;
pub(crate) mod release_download;
pub(crate) mod service;
pub(crate) mod worker;

#[cfg(test)]
pub(crate) mod test_support;

use crate::store::HaroldStore;
use crate::telemetry;
use crate::tools::BoxFuture;
use models::{DefaultRow, DefaultStatus, Deps, Registry, ResolveError};
use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;
use tracing::{debug, info, warn};
use worker::{Chunk, Done, Failure, Refused, Supervisor};

/// The most characters one line may have.
pub(crate) const MAX_TEXT_CHARS: usize = 4_000;

/// Who asked for a line: the desk voice, or a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Caller {
    Desk,
    Call,
}

impl Caller {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Caller::Desk => "desk",
            Caller::Call => "call",
        }
    }
}

/// Why a request for a line failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SpeakFailure {
    InvalidText,
    UnknownModel,
    UnknownVoice,
    ModelDownloading,
    ModelMissing,
    WorkerFailed,
    TimedOut,
}

impl SpeakFailure {
    /// The word the API gives, and `tts failed` logs for the four reasons it logs.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            SpeakFailure::InvalidText => "invalid_text",
            SpeakFailure::UnknownModel => "unknown_model",
            SpeakFailure::UnknownVoice => "unknown_voice",
            SpeakFailure::ModelDownloading => "model_downloading",
            SpeakFailure::ModelMissing => "model_missing",
            SpeakFailure::WorkerFailed => "worker_failed",
            SpeakFailure::TimedOut => "timed_out",
        }
    }

    /// Whether `tts failed` logs it: the request was fine, and the speech failed.
    fn is_logged(self) -> bool {
        matches!(
            self,
            SpeakFailure::ModelDownloading
                | SpeakFailure::ModelMissing
                | SpeakFailure::WorkerFailed
                | SpeakFailure::TimedOut
        )
    }
}

impl From<Failure> for SpeakFailure {
    fn from(failure: Failure) -> Self {
        match failure {
            Failure::WorkerFailed => SpeakFailure::WorkerFailed,
            Failure::TimedOut => SpeakFailure::TimedOut,
            Failure::ModelMissing => SpeakFailure::ModelMissing,
            Failure::UnknownVoice => SpeakFailure::UnknownVoice,
        }
    }
}

/// All the audio of one line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Audio {
    pub sample_rate: u32,
    /// 16-bit little-endian mono PCM.
    pub pcm: Vec<u8>,
}

pub(crate) struct TtsServer {
    registry: Arc<Registry>,
    supervisor: Arc<Supervisor>,
    handle: tokio::runtime::Handle,
    /// The reasons `tts gpu unavailable` has been logged for in this run.
    gpu_noticed: Mutex<HashSet<&'static str>>,
}

/// One line being made. The first piece of audio is here; `next` gives each piece and, at the
/// end, how the line ended.
pub(crate) struct Speech {
    pub sample_rate: u32,
    first: Option<Vec<u8>>,
    chunks: mpsc::Receiver<Chunk>,
    end: Option<oneshot::Receiver<Result<Done, Failure>>>,
    span: tracing::Span,
    model: String,
    caller: Caller,
    server: Arc<TtsServer>,
}

impl Speech {
    /// The next piece of audio, `None` at the end of the line.
    pub(crate) async fn next(&mut self) -> Result<Option<Vec<u8>>, SpeakFailure> {
        if let Some(first) = self.first.take() {
            return Ok(Some(first));
        }
        match self.chunks.recv().await {
            Some(chunk) => Ok(Some(chunk.pcm)),
            None => self.finish().await.map(|()| None),
        }
    }

    async fn finish(&mut self) -> Result<(), SpeakFailure> {
        let Some(end) = self.end.take() else {
            return Ok(());
        };
        match end.await {
            Ok(Ok(done)) => {
                self.span.record("device", done.device.as_str());
                self.span.record("result", "spoke");
                self.server.note_gpu(done.gpu_reason);
                Ok(())
            }
            Ok(Err(failure)) => Err(self.server.failed(
                &self.span,
                &self.model,
                self.caller,
                SpeakFailure::from(failure),
            )),
            Err(_) => Err(self.server.failed(
                &self.span,
                &self.model,
                self.caller,
                SpeakFailure::WorkerFailed,
            )),
        }
    }
}

impl TtsServer {
    /// A server over `registry` and `supervisor`. Called inside the runtime: the blocking
    /// callers reach the runtime through the handle it takes.
    pub(crate) fn new(registry: Arc<Registry>, supervisor: Arc<Supervisor>) -> Arc<Self> {
        Arc::new(Self {
            registry,
            supervisor,
            handle: tokio::runtime::Handle::current(),
            gpu_noticed: Mutex::default(),
        })
    }

    pub(crate) fn registry(&self) -> &Arc<Registry> {
        &self.registry
    }

    pub(crate) fn supervisor(&self) -> &Arc<Supervisor> {
        &self.supervisor
    }

    /// Logs `tts gpu unavailable` once for each reason in this run: the worker restarts many
    /// times a day, and the same fact would repeat.
    fn note_gpu(&self, reason: Option<&'static str>) {
        let Some(reason) = reason else {
            return;
        };
        let first = self
            .gpu_noticed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(reason);
        if first {
            warn!(reason, "tts gpu unavailable");
        }
    }

    /// Records the failure on the line's span and logs `tts failed`, and returns it.
    fn failed(
        &self,
        span: &tracing::Span,
        model: &str,
        caller: Caller,
        failure: SpeakFailure,
    ) -> SpeakFailure {
        span.record("result", "failed");
        if failure.is_logged() {
            warn!(
                reason = failure.as_str(),
                model,
                caller = caller.as_str(),
                "tts failed"
            );
        }
        failure
    }

    /// Makes `text` in `voice` with `model` (empty for the defaults). Returns when the first
    /// piece of audio is here, or the request has failed.
    pub(crate) async fn speak(
        self: &Arc<Self>,
        caller: Caller,
        text: &str,
        model: &str,
        voice: &str,
    ) -> Result<Speech, SpeakFailure> {
        let started = Instant::now();
        let span = tracing::info_span!(
            target: telemetry::TRACE_TARGET,
            parent: None,
            "harold.tts.speak",
            model = tracing::field::Empty,
            caller = caller.as_str(),
            result = tracing::field::Empty,
            first_audio_ms = tracing::field::Empty,
            loaded = tracing::field::Empty,
            device = tracing::field::Empty,
        );
        let length = text.chars().count();
        if text.trim().is_empty() || length > MAX_TEXT_CHARS {
            span.record("result", "failed");
            return Err(SpeakFailure::InvalidText);
        }
        let resolved = match self.registry.resolve(model, voice) {
            Ok(resolved) => resolved,
            Err(error) => {
                let failure = match error {
                    ResolveError::UnknownModel => SpeakFailure::UnknownModel,
                    ResolveError::UnknownVoice => SpeakFailure::UnknownVoice,
                    ResolveError::Downloading => SpeakFailure::ModelDownloading,
                    ResolveError::Missing(_) => SpeakFailure::ModelMissing,
                };
                let shown = if model.is_empty() {
                    self.registry.default_pair().0
                } else {
                    model.to_string()
                };
                span.record("model", shown.as_str());
                return Err(self.failed(&span, &shown, caller, failure));
            }
        };
        span.record("model", resolved.id.as_str());
        let speaking = self
            .supervisor
            .speak(&resolved.load, text.to_string(), resolved.voice.clone())
            .await;
        let speaking = match speaking {
            Ok(speaking) => speaking,
            Err(Refused { failure, loaded }) => {
                span.record("loaded", loaded);
                return Err(self.failed(&span, &resolved.id, caller, failure.into()));
            }
        };
        let first_audio_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        span.record("first_audio_ms", first_audio_ms);
        span.record("loaded", speaking.loaded);
        if let Some(ready) = &speaking.ready {
            self.registry.note_voices(&resolved.id, &ready.voices);
            self.note_gpu(ready.gpu_reason);
            debug!(model = %resolved.id, load_ms = ready.load_ms, "tts worker ready");
        }
        Ok(Speech {
            sample_rate: speaking.first.sample_rate,
            first: Some(speaking.first.pcm),
            chunks: speaking.chunks,
            end: Some(speaking.end),
            span,
            model: resolved.id,
            caller,
            server: Arc::clone(self),
        })
    }

    /// The whole line as one piece of audio.
    pub(crate) async fn speak_all(
        self: &Arc<Self>,
        caller: Caller,
        text: &str,
    ) -> Result<Audio, SpeakFailure> {
        let mut speech = self.speak(caller, text, "", "").await?;
        let mut pcm = Vec::new();
        while let Some(piece) = speech.next().await? {
            pcm.extend(piece);
        }
        Ok(Audio {
            sample_rate: speech.sample_rate,
            pcm,
        })
    }

    /// `speak_all` for a caller that is not async: the desk voice runs on a thread that
    /// waits. The work runs on the daemon's runtime.
    pub(crate) fn speak_all_blocking(
        self: &Arc<Self>,
        caller: Caller,
        text: &str,
    ) -> Result<Audio, SpeakFailure> {
        let (tx, rx) = std::sync::mpsc::channel();
        let server = Arc::clone(self);
        let text = text.to_string();
        self.handle.spawn(async move {
            let _ = tx.send(server.speak_all(caller, &text).await);
        });
        rx.recv().unwrap_or(Err(SpeakFailure::WorkerFailed))
    }

    /// A call is ringing: starts the worker for the default model without a request, so the
    /// first line of the call does not wait for the load, and starts the model's download if
    /// it is held back. The worker is not stopped for being idle until the call ends.
    pub(crate) fn warm_for_call(self: &Arc<Self>) {
        self.registry.for_call();
        let server = Arc::clone(self);
        self.handle.spawn(async move {
            let mut changes = server.registry.subscribe();
            loop {
                match server.registry.default_status() {
                    DefaultStatus::Ready => break,
                    DefaultStatus::Failed { .. } => return,
                    _ => {}
                }
                if changes.changed().await.is_err() {
                    return;
                }
            }
            let Ok(resolved) = server.registry.resolve("", "") else {
                return;
            };
            if let Err(failure) = server
                .supervisor
                .warm(&resolved.load, &resolved.voice)
                .await
            {
                worker::log_launch_failure(&resolved.id, failure);
            }
        });
    }

    /// A call opened: the worker is kept while it is on.
    pub(crate) fn call_opened(&self) {
        self.supervisor.call_opened();
    }

    /// A call ended: the idle time starts now.
    pub(crate) fn call_ended(&self) {
        self.supervisor.call_ended();
    }
}

/// Held for the length of a call: the worker is kept while it lives, and its idle time starts
/// when it is dropped.
pub(crate) struct CallGuard(Arc<TtsServer>);

impl TtsServer {
    /// A call opened: the guard keeps the worker until the call ends.
    pub(crate) fn call_guard(self: &Arc<Self>) -> CallGuard {
        self.call_opened();
        CallGuard(Arc::clone(self))
    }
}

impl Drop for CallGuard {
    fn drop(&mut self) {
        self.0.call_ended();
    }
}

/// The server of this run.
static SERVER: OnceLock<Arc<TtsServer>> = OnceLock::new();

pub(crate) fn init_server(server: Arc<TtsServer>) {
    assert!(
        SERVER.set(server).is_ok(),
        "init_server called more than once"
    );
}

/// The server of this run, once started.
pub(crate) fn server() -> Option<Arc<TtsServer>> {
    SERVER.get().cloned()
}

/// The default model and voice, kept in Harold's store.
struct StoreDefault(Arc<HaroldStore>);

impl DefaultRow for StoreDefault {
    fn load(&self) -> BoxFuture<'_, Option<(String, String)>> {
        Box::pin(async move { self.0.tts_default().await.ok().flatten() })
    }

    fn save<'a>(&'a self, model: &'a str, voice: &'a str) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.0
                .set_tts_default(model, voice)
                .await
                .map_err(|error| error.to_string())
        })
    }
}

/// The Python the TTS worker and the model downloads run with: `[voice_input] command` on the
/// Mac, `[tts] python` on Linux.
pub(crate) fn python_of(settings: &crate::settings::Settings) -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        settings
            .voice_input
            .as_ref()
            .map(|voice_input| voice_input.command.clone())
            .or_else(|| settings.tts.python.clone())
    }
    #[cfg(not(target_os = "macos"))]
    {
        settings.tts.python.clone()
    }
}

/// Starts the TTS server of this run: the model registry (which looks at the disk at once and
/// downloads what is missing in the background), the worker supervisor and its idle reaper.
/// `store_path` is the store's folder, beside which `tts-models/` is made.
pub(crate) async fn start(
    settings: &crate::settings::Settings,
    store: Arc<HaroldStore>,
    store_path: &str,
    shutdown: watch::Receiver<()>,
) -> Arc<TtsServer> {
    let data = Path::new(store_path)
        .parent()
        .unwrap_or(Path::new("."))
        .to_path_buf();
    let folder = data.join("tts-models");
    let python = python_of(settings);
    let env = fetch::Env::from_process();
    let mut kokoro = models::kokoro_row(&settings.tts);
    if python.is_none() {
        if settings.tts.engine == crate::settings::TtsEngine::Server {
            warn!("tts server has no python: set [tts] python");
        }
        kokoro.phase = models::Phase::Failed("failed");
    }
    let python = python.unwrap_or_default();
    let registry = Registry::new(
        Deps {
            folder: folder.clone(),
            approved_list: crate::settings::tts_models_path(),
            env: env.clone(),
            host: Arc::new(fetch::SystemHost),
            fetcher: Arc::new(hub::ScriptFetcher::new(python.clone(), folder.clone(), env)),
            release: Arc::new(release_download::ReleaseDownloader::production()),
            default_row: Arc::new(StoreDefault(store)),
        },
        kokoro,
    );
    if let Some(voice) = &settings.tts.legacy_voice {
        registry.start_with_voice(voice);
    }
    registry.load_default().await;
    let idle = std::time::Duration::from_secs(60 * settings.tts.idle_minutes);
    let supervisor = Supervisor::new(Arc::new(worker::ProcessLauncher::new(python, folder)), idle);
    tokio::spawn(Arc::clone(&supervisor).tend(shutdown));
    let server = TtsServer::new(Arc::clone(&registry), supervisor);
    registry.start_up();
    info!(
        worker = worker::WorkerKind::THIS_BUILD.as_str(),
        "tts server started"
    );
    server
}

#[cfg(test)]
#[path = "server_tests.rs"]
mod tests;
