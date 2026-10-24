//! The embedding worker's client: a Python program that keeps the model loaded and answers
//! one JSON line per request. It starts on the first request that needs it and stops after
//! it has had none for a while.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tokio::time::{Instant, timeout};
use tracing::{info, warn};

use crate::tools::BoxFuture;

/// The worker's program, written into the tool's folder when the worker starts.
pub(crate) const EMBED_WORKER_PY: &str = include_str!("../../embed_worker.py");

/// How long the worker has to load the model.
const LOAD_LIMIT: Duration = Duration::from_secs(180);
/// How long the worker has to answer one request.
const REQUEST_LIMIT: Duration = Duration::from_secs(120);
/// How long `stop` waits for the worker to exit before it kills it.
const CLOSE_LIMIT: Duration = Duration::from_secs(5);
/// How long the worker stays loaded with no request.
pub(crate) const IDLE_LIMIT: Duration = Duration::from_secs(10 * 60);
/// How often the reaper looks at the idle time.
const REAP_EVERY: Duration = Duration::from_secs(15);
/// The most texts sent in one request.
pub(crate) const BATCH: usize = 16;

/// What the embedding model is, as the index records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelInfo {
    pub runtime: String,
    pub model: String,
    pub dimensions: usize,
    /// A name for the prompts `text::passage_prompt` and `text::query_prompt` apply.
    pub prompts: String,
}

/// Why the embedding worker could not answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EmbedError {
    /// The worker is not available: `failed_to_load`, `exited` or `timed_out`.
    Unavailable(&'static str),
    /// The worker answered that this one request failed.
    Failed,
}

/// What the knowledge tool asks of the embedding model.
pub(crate) trait Embedder: Send + Sync + 'static {
    /// Starts the worker when it is not running and says what the model is.
    fn ready(&self) -> BoxFuture<'_, Result<ModelInfo, EmbedError>>;
    /// The embedding of each text, each of length 1, in order.
    fn embed(&self, texts: Vec<String>) -> BoxFuture<'_, Result<Vec<Vec<f32>>, EmbedError>>;
    /// `idle`, `loading`, `ready` or `unavailable`, for `status`.
    fn state(&self) -> &'static str;
    /// The name the model will be recorded under, known without starting the worker.
    fn configured_model(&self) -> Option<String> {
        None
    }
}

/// How to start the worker.
#[derive(Debug, Clone)]
pub(crate) struct WorkerCommand {
    pub python: String,
    pub args: Vec<String>,
    pub model_path: PathBuf,
    pub model_sha256: Option<String>,
    /// The folder the script is written into.
    pub folder: PathBuf,
}

struct Live {
    child: Child,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    info: ModelInfo,
    load_ms: u64,
    passages: u64,
    embed_ms: u64,
    last_used: Instant,
    next_id: u64,
}

#[derive(Default)]
struct Inner {
    live: Option<Live>,
}

/// The real worker, started on demand and stopped when idle.
pub(crate) struct WorkerEmbedder {
    command: WorkerCommand,
    idle_limit: Duration,
    inner: Mutex<Inner>,
    state: std::sync::atomic::AtomicU8,
}

const IDLE: u8 = 0;
const LOADING: u8 = 1;
const READY: u8 = 2;
const UNAVAILABLE: u8 = 3;

impl WorkerEmbedder {
    pub(crate) fn new(command: WorkerCommand, idle_limit: Duration) -> Arc<Self> {
        Arc::new(Self {
            command,
            idle_limit,
            inner: Mutex::new(Inner::default()),
            state: std::sync::atomic::AtomicU8::new(IDLE),
        })
    }

    /// Stops the worker after `idle_limit` with no request, and at shutdown.
    pub(crate) fn spawn_reaper(
        self: &Arc<Self>,
        mut shutdown: tokio::sync::watch::Receiver<()>,
    ) -> tokio::task::JoinHandle<()> {
        let worker = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(REAP_EVERY) => worker.stop_if_idle().await,
                    _ = shutdown.changed() => {
                        worker.stop().await;
                        return;
                    }
                }
            }
        })
    }

    async fn stop_if_idle(&self) {
        let mut inner = self.inner.lock().await;
        let idle = inner
            .live
            .as_ref()
            .is_some_and(|live| live.last_used.elapsed() >= self.idle_limit);
        if idle {
            Self::stop_live(&mut inner, &self.state).await;
        }
    }

    /// Stops the worker now.
    pub(crate) async fn stop(&self) {
        let mut inner = self.inner.lock().await;
        Self::stop_live(&mut inner, &self.state).await;
    }

    async fn stop_live(inner: &mut Inner, state: &std::sync::atomic::AtomicU8) {
        let Some(mut live) = inner.live.take() else {
            return;
        };
        let rss_mb = live.child.id().and_then(resident_mb).unwrap_or(0);
        let per_passage = live.embed_ms.checked_div(live.passages).unwrap_or(0);
        drop(live.stdin);
        if timeout(CLOSE_LIMIT, live.child.wait()).await.is_err() {
            let _ = live.child.kill().await;
        }
        info!(
            load_ms = live.load_ms,
            embed_ms_per_passage = per_passage,
            passages = live.passages,
            rss_mb,
            "knowledge worker stopped"
        );
        state.store(IDLE, std::sync::atomic::Ordering::Relaxed);
    }

    async fn start_live(&self) -> Result<Live, EmbedError> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let fail = |reason: &'static str| {
            warn!(reason, "knowledge worker exited");
            EmbedError::Unavailable(reason)
        };
        let script = self.command.folder.join("embed_worker.py");
        let written = std::fs::create_dir_all(&self.command.folder)
            .and_then(|()| match std::fs::remove_file(&script) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
                _ => Ok(()),
            })
            .and_then(|()| {
                std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&script)
                    .and_then(|mut file| file.write_all(EMBED_WORKER_PY.as_bytes()))
            });
        if written.is_err() {
            return Err(fail("failed_to_load"));
        }
        let mut worker = Command::new(&self.command.python);
        worker
            .args(&self.command.args)
            .arg("-I")
            .arg(&script)
            .arg("--model")
            .arg(&self.command.model_path);
        if let Some(hash) = &self.command.model_sha256 {
            worker.arg("--sha256").arg(hash);
        }
        let mut child = worker
            .current_dir(&self.command.folder)
            .env("HF_HUB_OFFLINE", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| fail("failed_to_load"))?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            return Err(fail("failed_to_load"));
        };
        let mut lines = BufReader::new(stdout).lines();
        let ready = match timeout(LOAD_LIMIT, lines.next_line()).await {
            Ok(Ok(Some(line))) => serde_json::from_str::<Value>(&line).ok(),
            _ => None,
        };
        let Some(ready) = ready.filter(|r| r["ready"] == true) else {
            let _ = child.kill().await;
            return Err(fail("failed_to_load"));
        };
        let dimensions = ready["dimensions"].as_u64().unwrap_or(0) as usize;
        let model = self
            .command
            .model_path
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        Ok(Live {
            child,
            stdin,
            lines,
            info: ModelInfo {
                runtime: ready["runtime"].as_str().unwrap_or("unknown").to_string(),
                model,
                dimensions,
                prompts: "embeddinggemma".into(),
            },
            load_ms: ready["load_ms"].as_u64().unwrap_or(0),
            passages: 0,
            embed_ms: 0,
            last_used: Instant::now(),
            next_id: 1,
        })
    }

    async fn ensure<'a>(&self, inner: &'a mut Inner) -> Result<&'a mut Live, EmbedError> {
        use std::sync::atomic::Ordering::Relaxed;
        if inner.live.is_none() {
            self.state.store(LOADING, Relaxed);
            match self.start_live().await {
                Ok(live) => {
                    inner.live = Some(live);
                }
                Err(error) => {
                    self.state.store(UNAVAILABLE, Relaxed);
                    return Err(error);
                }
            }
        }
        self.state.store(READY, Relaxed);
        inner.live.as_mut().ok_or(EmbedError::Failed)
    }

    async fn embed_inner(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, EmbedError> {
        let mut inner = self.inner.lock().await;
        let mut out = Vec::with_capacity(texts.len());
        for batch in texts.chunks(BATCH) {
            let result = {
                let live = self.ensure(&mut inner).await?;
                request(live, batch).await
            };
            match result {
                Ok(mut vectors) => out.append(&mut vectors),
                Err(error) => {
                    if let EmbedError::Unavailable(reason) = error {
                        warn!(reason, "knowledge worker exited");
                        if let Some(mut live) = inner.live.take() {
                            let _ = live.child.kill().await;
                        }
                        self.state
                            .store(UNAVAILABLE, std::sync::atomic::Ordering::Relaxed);
                    }
                    return Err(error);
                }
            }
        }
        Ok(out)
    }
}

async fn request(live: &mut Live, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
    let id = live.next_id;
    live.next_id += 1;
    let line = format!("{}\n", json!({"id": id, "texts": texts}));
    let started = Instant::now();
    let answer = timeout(REQUEST_LIMIT, async {
        live.stdin.write_all(line.as_bytes()).await.ok()?;
        live.stdin.flush().await.ok()?;
        live.lines.next_line().await.ok()?
    })
    .await;
    let line = match answer {
        Err(_) => return Err(EmbedError::Unavailable("timed_out")),
        Ok(None) => return Err(EmbedError::Unavailable("exited")),
        Ok(Some(line)) => line,
    };
    let value: Value =
        serde_json::from_str(&line).map_err(|_| EmbedError::Unavailable("exited"))?;
    if value["id"].as_u64() != Some(id) || value.get("error").is_some() {
        return Err(EmbedError::Failed);
    }
    let vectors: Option<Vec<Vec<f32>>> = value["embeddings"].as_array().map(|all| {
        all.iter()
            .map(|one| {
                one.as_array()
                    .map(|v| v.iter().map(|x| x.as_f64().unwrap_or(0.0) as f32).collect())
                    .unwrap_or_default()
            })
            .collect()
    });
    let vectors = vectors.ok_or(EmbedError::Failed)?;
    if vectors.len() != texts.len() || vectors.iter().any(|v| v.len() != live.info.dimensions) {
        return Err(EmbedError::Failed);
    }
    live.passages += texts.len() as u64;
    live.embed_ms += started.elapsed().as_millis() as u64;
    live.last_used = Instant::now();
    Ok(vectors)
}

/// The resident memory of a process in MB, read with `ps`.
fn resident_mb(pid: u32) -> Option<u64> {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let kb: u64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
    Some(kb / 1024)
}

impl Embedder for WorkerEmbedder {
    fn ready(&self) -> BoxFuture<'_, Result<ModelInfo, EmbedError>> {
        Box::pin(async move {
            let mut inner = self.inner.lock().await;
            let live = self.ensure(&mut inner).await?;
            live.last_used = Instant::now();
            Ok(live.info.clone())
        })
    }

    fn embed(&self, texts: Vec<String>) -> BoxFuture<'_, Result<Vec<Vec<f32>>, EmbedError>> {
        Box::pin(self.embed_inner(texts))
    }

    fn configured_model(&self) -> Option<String> {
        self.command
            .model_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
    }

    fn state(&self) -> &'static str {
        use std::sync::atomic::Ordering::Relaxed;
        match self.state.load(Relaxed) {
            LOADING => "loading",
            READY => "ready",
            UNAVAILABLE => "unavailable",
            _ => "idle",
        }
    }
}

#[cfg(test)]
#[path = "embed_tests.rs"]
mod tests;
