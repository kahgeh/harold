//! The TTS worker: a Python process that keeps one TTS model loaded and makes the audio of a
//! line. The worker is chosen by the target the build is for: the MLX worker on macOS (the
//! Apple GPU), the ONNX worker on Linux (the NVIDIA GPU, with the CPU as the fallback).
//!
//! `Supervisor` starts the worker on the first request, keeps it while requests come or a call
//! is open, stops it after the idle time, and starts it again on the next request. A worker
//! that exits fails its request with `worker_failed`.

use crate::tools::BoxFuture;
use serde_json::{Value, json};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, timeout};
use tracing::{debug, warn};

/// The macOS build holds the MLX worker and no ONNX worker.
#[cfg(target_os = "macos")]
pub(crate) const WORKER_PY: &str = include_str!("../tts_worker_mlx.py");
#[cfg(target_os = "macos")]
pub(crate) const WORKER_FILE: &str = "tts_worker_mlx.py";

/// The Linux build holds the ONNX worker and no MLX worker.
#[cfg(not(target_os = "macos"))]
pub(crate) const WORKER_PY: &str = include_str!("../tts_worker_onnx.py");
#[cfg(not(target_os = "macos"))]
pub(crate) const WORKER_FILE: &str = "tts_worker_onnx.py";

/// How long the worker has to send its next line of audio, or its first.
pub(crate) const REQUEST_LIMIT: Duration = Duration::from_secs(30);
/// How long the worker has to load its model.
pub(crate) const LOAD_LIMIT: Duration = Duration::from_secs(120);
/// How often the reaper looks at the idle time.
pub(crate) const TEND_EVERY: Duration = Duration::from_secs(5);
/// How long `close` waits for the worker to exit before it kills it.
const CLOSE_LIMIT: Duration = Duration::from_secs(5);

/// Which worker runs a model: the MLX worker or the ONNX worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkerKind {
    Mlx,
    Onnx,
}

impl WorkerKind {
    /// The worker this build holds.
    pub(crate) const THIS_BUILD: WorkerKind = if cfg!(target_os = "macos") {
        WorkerKind::Mlx
    } else {
        WorkerKind::Onnx
    };

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            WorkerKind::Mlx => "mlx",
            WorkerKind::Onnx => "onnx",
        }
    }

    pub(crate) fn from_word(word: &str) -> Option<Self> {
        match word {
            "mlx" => Some(WorkerKind::Mlx),
            "onnx" => Some(WorkerKind::Onnx),
            _ => None,
        }
    }
}

/// Where the audio was made, as `harold.tts.speak` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Device {
    Gpu,
    Cpu,
    AppleGpu,
}

impl Device {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Device::Gpu => "gpu",
            Device::Cpu => "cpu",
            Device::AppleGpu => "apple_gpu",
        }
    }

    fn from_word(word: &str) -> Self {
        match word {
            "gpu" => Device::Gpu,
            "apple_gpu" => Device::AppleGpu,
            _ => Device::Cpu,
        }
    }
}

/// The reasons the GPU is not used, as the `tts gpu unavailable` warn line names them.
pub(crate) const GPU_REASONS: [&str; 4] = [
    "no_gpu",
    "no_driver",
    "cuda_load_failed",
    "gpu_out_of_memory",
];

/// A reason from the worker, when it is one of the four.
fn gpu_reason(word: &str) -> Option<&'static str> {
    GPU_REASONS.iter().copied().find(|known| *known == word)
}

/// Why a request failed, as `tts failed` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Failure {
    /// The worker would not start, exited, or sent a line that is not an answer.
    WorkerFailed,
    /// The worker sent no audio within the limit.
    TimedOut,
    /// A file of the model is not on the machine.
    ModelMissing,
    /// The worker does not have the voice.
    UnknownVoice,
}

impl Failure {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Failure::WorkerFailed => "worker_failed",
            Failure::TimedOut => "timed_out",
            Failure::ModelMissing => "model_missing",
            Failure::UnknownVoice => "unknown_voice",
        }
    }
}

/// The files a worker loads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Load {
    /// An MLX model: its repository name, its pinned snapshot folder and its commit.
    Mlx {
        model: String,
        snapshot: PathBuf,
        commit: String,
    },
    /// An ONNX model and its voices file.
    Onnx {
        model_file: PathBuf,
        voices_file: PathBuf,
    },
}

impl Load {
    pub(crate) fn kind(&self) -> WorkerKind {
        match self {
            Load::Mlx { .. } => WorkerKind::Mlx,
            Load::Onnx { .. } => WorkerKind::Onnx,
        }
    }
}

/// A piece of audio: 16-bit little-endian mono PCM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Chunk {
    pub sample_rate: u32,
    pub pcm: Vec<u8>,
}

/// What the worker said when it was ready.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Ready {
    pub load_ms: u64,
    pub device: Device,
    /// Why the work runs on the CPU, when the worker says.
    pub gpu_reason: Option<&'static str>,
    pub voices: Vec<String>,
}

/// How one line ended at the worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Done {
    pub device: Device,
    pub gpu_reason: Option<&'static str>,
}

/// One loaded worker.
pub(crate) trait Session: Send + 'static {
    fn ready(&self) -> &Ready;
    /// Makes `text` in `voice`, passing each piece of audio to `out` as it comes. A closed `out`
    /// does not stop the line: the rest is read and dropped, so the worker stays in step.
    fn speak<'a>(
        &'a mut self,
        text: &'a str,
        voice: &'a str,
        out: &'a mpsc::Sender<Chunk>,
    ) -> BoxFuture<'a, Result<Done, Failure>>;
    /// Whether the worker is still running.
    fn is_alive(&mut self) -> bool;
    /// Stops the worker.
    fn close(self: Box<Self>) -> BoxFuture<'static, ()>;
}

/// Starts workers.
pub(crate) trait Launcher: Send + Sync + 'static {
    /// Starts a worker that loads `load`, with `voice` as its voice for the first line. Ends with the
    /// worker ready, or with why it is not.
    fn launch<'a>(
        &'a self,
        load: &'a Load,
        voice: &'a str,
    ) -> BoxFuture<'a, Result<Box<dyn Session>, Failure>>;
}

/// Starts the platform's worker as a child process.
pub(crate) struct ProcessLauncher {
    python: String,
    folder: PathBuf,
}

impl ProcessLauncher {
    pub(crate) fn new(python: String, folder: PathBuf) -> Self {
        Self { python, folder }
    }

    /// Writes the worker's program into the models folder (0700), as a file for the user only,
    /// never reusing one that is there.
    fn write_script(&self) -> std::io::Result<PathBuf> {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.folder)?;
        let script = self.folder.join(WORKER_FILE);
        match std::fs::remove_file(&script) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&script)?;
        file.write_all(WORKER_PY.as_bytes())?;
        Ok(script)
    }
}

/// The variables a worker is given: nothing of the user's, and no Hugging Face token.
fn worker_env() -> Vec<(&'static str, String)> {
    let var = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
    let mut vars = vec![
        (
            "PATH",
            var("PATH").unwrap_or_else(|| "/usr/bin:/bin".into()),
        ),
        ("HF_HUB_OFFLINE", "1".to_string()),
    ];
    for name in ["HOME", "HF_HOME", "HF_HUB_CACHE", "LANG", "LC_ALL"] {
        if let Some(value) = var(name) {
            vars.push((name, value));
        }
    }
    vars
}

impl Launcher for ProcessLauncher {
    fn launch<'a>(
        &'a self,
        load: &'a Load,
        voice: &'a str,
    ) -> BoxFuture<'a, Result<Box<dyn Session>, Failure>> {
        Box::pin(async move {
            if load.kind() != WorkerKind::THIS_BUILD {
                return Err(Failure::WorkerFailed);
            }
            let script = self.write_script().map_err(|_| Failure::WorkerFailed)?;
            let mut command = Command::new(&self.python);
            command.arg("-I").arg(&script);
            match load {
                Load::Mlx {
                    model,
                    snapshot,
                    commit,
                } => {
                    command
                        .args(["--model", model])
                        .arg("--snapshot")
                        .arg(snapshot)
                        .args(["--commit", commit]);
                }
                Load::Onnx {
                    model_file,
                    voices_file,
                } => {
                    command
                        .arg("--model-file")
                        .arg(model_file)
                        .arg("--voices-file")
                        .arg(voices_file);
                }
            }
            command.args(["--voice", voice]);
            let mut child = command
                .current_dir(&self.folder)
                .env_clear()
                .envs(worker_env())
                .process_group(0)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn()
                .map_err(|_| Failure::WorkerFailed)?;
            let stdin = child.stdin.take().ok_or(Failure::WorkerFailed)?;
            let stdout = child.stdout.take().ok_or(Failure::WorkerFailed)?;
            let mut lines = BufReader::new(stdout).lines();
            let ready = match timeout(LOAD_LIMIT, wait_for_ready(&mut lines)).await {
                Ok(Ok(ready)) => ready,
                Ok(Err(failure)) => {
                    let _ = child.kill().await;
                    return Err(failure);
                }
                Err(_) => {
                    let _ = child.kill().await;
                    return Err(Failure::TimedOut);
                }
            };
            Ok(Box::new(ProcessSession {
                child,
                stdin: Some(stdin),
                lines,
                ready,
                next_id: 0,
            }) as Box<dyn Session>)
        })
    }
}

/// Reads up to the first JSON line, and says what it comes to. A line that is not JSON before
/// it is a library's stray print and is skipped.
async fn wait_for_ready(lines: &mut Lines<BufReader<ChildStdout>>) -> Result<Ready, Failure> {
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(reply) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        return match reply["ready"].as_bool() {
            Some(true) => Ok(Ready {
                load_ms: reply["load_ms"].as_u64().unwrap_or(0),
                device: Device::from_word(reply["device"].as_str().unwrap_or("cpu")),
                gpu_reason: reply["gpu_reason"].as_str().and_then(gpu_reason),
                voices: reply["voices"]
                    .as_array()
                    .map(|voices| {
                        voices
                            .iter()
                            .filter_map(|voice| voice.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default(),
            }),
            Some(false) => Err(match reply["cause"].as_str() {
                Some("model_missing") => Failure::ModelMissing,
                Some("voice_missing") => Failure::UnknownVoice,
                _ => Failure::WorkerFailed,
            }),
            None => Err(Failure::WorkerFailed),
        };
    }
    Err(Failure::WorkerFailed)
}

/// A worker as a child process, spoken to over JSON lines. One request is in flight at a time,
/// so the session reads the worker's lines itself, inside `speak`.
struct ProcessSession {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Lines<BufReader<ChildStdout>>,
    ready: Ready,
    next_id: u64,
}

impl ProcessSession {
    async fn run_line(
        &mut self,
        text: &str,
        voice: &str,
        out: &mpsc::Sender<Chunk>,
    ) -> Result<Done, Failure> {
        self.next_id += 1;
        let id = self.next_id;
        let mut request =
            json!({"id": id, "op": "speak", "text": text, "voice": voice}).to_string();
        request.push('\n');
        let stdin = self.stdin.as_mut().ok_or(Failure::WorkerFailed)?;
        if stdin.write_all(request.as_bytes()).await.is_err() || stdin.flush().await.is_err() {
            return Err(Failure::WorkerFailed);
        }
        let mut listening = true;
        loop {
            let line = match timeout(REQUEST_LIMIT, self.lines.next_line()).await {
                Ok(Ok(Some(line))) => line,
                Ok(_) => return Err(Failure::WorkerFailed),
                Err(_) => {
                    let _ = self.child.start_kill();
                    return Err(Failure::TimedOut);
                }
            };
            let Ok(reply) = serde_json::from_str::<Value>(&line) else {
                // A line that is not an answer would put every later answer out of step.
                let _ = self.child.start_kill();
                return Err(Failure::WorkerFailed);
            };
            if reply["id"].as_u64() != Some(id) {
                continue;
            }
            if let Some(piece) = reply["chunk"].as_str() {
                let (Ok(pcm), Some(rate)) = (hex::decode(piece), reply["rate"].as_u64()) else {
                    let _ = self.child.start_kill();
                    return Err(Failure::WorkerFailed);
                };
                let sample_rate = u32::try_from(rate).unwrap_or(0);
                // A closed receiver: the rest of the line is read and dropped.
                if listening && out.send(Chunk { sample_rate, pcm }).await.is_err() {
                    listening = false;
                }
                continue;
            }
            if reply["done"].as_bool() == Some(true) {
                return Ok(Done {
                    device: Device::from_word(reply["device"].as_str().unwrap_or("cpu")),
                    gpu_reason: reply["gpu_reason"].as_str().and_then(gpu_reason),
                });
            }
            return Err(match reply["error"].as_str() {
                Some("voice_missing") => Failure::UnknownVoice,
                _ => Failure::WorkerFailed,
            });
        }
    }
}

impl Session for ProcessSession {
    fn ready(&self) -> &Ready {
        &self.ready
    }

    fn speak<'a>(
        &'a mut self,
        text: &'a str,
        voice: &'a str,
        out: &'a mpsc::Sender<Chunk>,
    ) -> BoxFuture<'a, Result<Done, Failure>> {
        Box::pin(self.run_line(text, voice, out))
    }

    fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    fn close(mut self: Box<Self>) -> BoxFuture<'static, ()> {
        Box::pin(async move {
            drop(self.stdin.take());
            if timeout(CLOSE_LIMIT, self.child.wait()).await.is_err() {
                let _ = self.child.kill().await;
            }
        })
    }
}

/// A request whose first piece of audio is here. The rest of the audio comes on `chunks`,
/// which closes when the line has ended; then `end` says how it ended.
pub(crate) struct Speaking {
    /// Whether this request started the worker.
    pub loaded: bool,
    /// The worker's ready line, for a request that started it.
    pub ready: Option<Ready>,
    pub first: Chunk,
    pub chunks: mpsc::Receiver<Chunk>,
    pub end: oneshot::Receiver<Result<Done, Failure>>,
}

/// A request that failed before any audio, and whether it started the worker.
pub(crate) struct Refused {
    pub failure: Failure,
    pub loaded: bool,
}

struct Live {
    session: Box<dyn Session>,
    load: Load,
}

/// Starts the worker on the first request, keeps it, and stops it when it is idle.
pub(crate) struct Supervisor {
    launcher: Arc<dyn Launcher>,
    idle_limit: Duration,
    live: Arc<tokio::sync::Mutex<Option<Live>>>,
    calls: AtomicUsize,
    last_used: Mutex<Instant>,
}

impl Supervisor {
    pub(crate) fn new(launcher: Arc<dyn Launcher>, idle_limit: Duration) -> Arc<Self> {
        Arc::new(Self {
            launcher,
            idle_limit,
            live: Arc::new(tokio::sync::Mutex::new(None)),
            calls: AtomicUsize::new(0),
            last_used: Mutex::new(Instant::now()),
        })
    }

    fn touch(&self) {
        *self
            .last_used
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Instant::now();
    }

    fn idle_for(&self) -> Duration {
        self.last_used
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .elapsed()
    }

    /// A call is open: the worker is not stopped for being idle.
    pub(crate) fn call_opened(&self) {
        self.calls.fetch_add(1, Ordering::SeqCst);
    }

    /// A call ended: the idle time starts now.
    pub(crate) fn call_ended(&self) {
        let _ = self
            .calls
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |open| {
                open.checked_sub(1)
            });
        self.touch();
    }

    /// Whether a worker is loaded now. A busy worker counts.
    #[cfg(test)]
    pub(crate) async fn is_loaded(&self) -> bool {
        match self.live.try_lock() {
            Ok(live) => live.is_some(),
            Err(_) => true,
        }
    }

    /// Starts the worker for `load` if none runs for it, without a request: a call does this
    /// while it rings, so its first line does not wait for the load. Returns the load, or why
    /// it failed.
    pub(crate) async fn warm(&self, load: &Load, voice: &str) -> Result<(), Failure> {
        let mut live = self.live.lock().await;
        self.ensure(&mut live, load, voice).await?;
        self.touch();
        Ok(())
    }

    /// The live worker for `load`: the running one, or a new one. Returns whether it is new.
    async fn ensure(
        &self,
        live: &mut Option<Live>,
        load: &Load,
        voice: &str,
    ) -> Result<bool, Failure> {
        if let Some(running) = live.as_mut() {
            if running.load == *load && running.session.is_alive() {
                return Ok(false);
            }
            if let Some(old) = live.take() {
                old.session.close().await;
            }
        }
        let session = self.launcher.launch(load, voice).await?;
        *live = Some(Live {
            session,
            load: load.clone(),
        });
        Ok(true)
    }

    /// Makes `text` in `voice` with the model `load` names. Returns when the first piece of
    /// audio is here, or the request has failed. The rest comes on `Speaking::events`.
    pub(crate) async fn speak(
        self: &Arc<Self>,
        load: &Load,
        text: String,
        voice: String,
    ) -> Result<Speaking, Refused> {
        let mut guard = Arc::clone(&self.live).lock_owned().await;
        let loaded = match self.ensure(&mut guard, load, &voice).await {
            Ok(loaded) => loaded,
            Err(failure) => {
                return Err(Refused {
                    failure,
                    loaded: true,
                });
            }
        };
        let ready = loaded
            .then(|| guard.as_ref().map(|live| live.session.ready().clone()))
            .flatten();
        let (chunks_tx, mut chunks) = mpsc::channel::<Chunk>(64);
        let (end_tx, end) = oneshot::channel();
        let supervisor = Arc::clone(self);
        // The task keeps the worker for the whole line, and sends what comes.
        tokio::spawn(async move {
            let Some(live) = guard.as_mut() else {
                return;
            };
            let result = live.session.speak(&text, &voice, &chunks_tx).await;
            drop(chunks_tx);
            supervisor.touch();
            if result.is_err() {
                // The worker failed or is out of step: stop it, so the next request starts a
                // new one.
                if let Some(dead) = guard.take() {
                    dead.session.close().await;
                }
            }
            let _ = end_tx.send(result);
        });
        match chunks.recv().await {
            Some(first) => Ok(Speaking {
                loaded,
                ready,
                first,
                chunks,
                end,
            }),
            // A line that ends with no audio is a worker that failed.
            None => Err(Refused {
                failure: match end.await {
                    Ok(Err(failure)) => failure,
                    _ => Failure::WorkerFailed,
                },
                loaded,
            }),
        }
    }

    /// Stops the worker when it has been idle for the idle time and no call is open. A worker
    /// that is making a line is not idle.
    pub(crate) async fn stop_if_idle(&self) -> bool {
        if self.calls.load(Ordering::SeqCst) > 0 || self.idle_for() < self.idle_limit {
            return false;
        }
        let Ok(mut live) = self.live.try_lock() else {
            return false;
        };
        let Some(running) = live.take() else {
            return false;
        };
        running.session.close().await;
        debug!("tts worker stopped for being idle");
        true
    }

    /// Stops the worker at once, for the shutdown or a removed model.
    pub(crate) async fn stop(&self) {
        let mut live = self.live.lock().await;
        if let Some(running) = live.take() {
            running.session.close().await;
        }
    }

    /// Looks at the idle time every `TEND_EVERY` until `shutdown` closes, then stops the
    /// worker.
    pub(crate) async fn tend(self: Arc<Self>, mut shutdown: tokio::sync::watch::Receiver<()>) {
        loop {
            tokio::select! {
                () = tokio::time::sleep(TEND_EVERY) => {
                    self.stop_if_idle().await;
                }
                _ = shutdown.changed() => break,
            }
        }
        self.stop().await;
    }
}

/// Logs that the model's files are not where the worker was told.
pub(crate) fn log_launch_failure(model: &str, failure: Failure) {
    warn!(model, reason = failure.as_str(), "tts worker did not start");
}

#[cfg(test)]
pub(crate) fn script_path(folder: &std::path::Path) -> PathBuf {
    folder.join(WORKER_FILE)
}

#[cfg(test)]
#[path = "worker_tests.rs"]
mod tests;
