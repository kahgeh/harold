//! Fakes the TTS tests share: a host, a Hub, a default row kept in memory, a scratch cache and
//! data folder, and a fake worker launcher.

use super::fetch::{Env, Host, snapshot_dir};
use super::hub::{Fetcher, RemoteFile};
use super::models::*;
use super::release_download::ReleaseDownloader;
use super::worker::*;
use crate::settings::KOKORO_REVISION;
use crate::tools::BoxFuture;
use serde_json::json;
use std::net::Ipv4Addr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Notify, mpsc};
use tokio::time::timeout;

pub(crate) fn mlx(model: &str) -> Load {
    Load::Mlx {
        model: model.to_string(),
        snapshot: PathBuf::from("/cache/snapshots/x"),
        commit: "a".repeat(40),
    }
}

pub(crate) fn ready() -> Ready {
    Ready {
        load_ms: 5,
        device: Device::AppleGpu,
        gpu_reason: None,
        voices: vec!["af_heart".to_string()],
    }
}

/// What a fake session does with the next line.
#[derive(Clone, Copy)]
pub(crate) enum Next {
    Say,
    /// Sends three pieces of audio, then fails.
    SayThenFail,
    /// Sends one piece of audio, waits for `Script::release`, then sends the rest.
    Hold,
    Exit,
    Silent,
}

#[derive(Default)]
pub(crate) struct Script {
    pub(crate) launches: AtomicUsize,
    pub(crate) closed: AtomicUsize,
    /// The behaviours of the next lines, in order; `Say` when empty.
    pub(crate) lines: Mutex<Vec<Next>>,
    /// The failures of the next launches, in order.
    pub(crate) launch_failures: Mutex<Vec<Failure>>,
    /// Why the next workers say they run on the CPU, in order; none when empty.
    pub(crate) ready_gpu_reasons: Mutex<Vec<&'static str>>,
    /// Lets a line that holds go on.
    pub(crate) release: Notify,
    /// How the next lines end at the worker, in order; the Apple GPU when empty.
    pub(crate) done: Mutex<Vec<(Device, Option<&'static str>)>>,
}

pub(crate) struct FakeSession {
    pub(crate) script: Arc<Script>,
    pub(crate) alive: Arc<AtomicBool>,
    pub(crate) ready: Ready,
}

impl Session for FakeSession {
    fn ready(&self) -> &Ready {
        &self.ready
    }

    fn speak<'a>(
        &'a mut self,
        _text: &'a str,
        _voice: &'a str,
        out: &'a mpsc::Sender<Chunk>,
    ) -> BoxFuture<'a, Result<Done, Failure>> {
        Box::pin(async move {
            let next = {
                let mut lines = self.script.lines.lock().unwrap();
                if lines.is_empty() {
                    Next::Say
                } else {
                    lines.remove(0)
                }
            };
            match next {
                Next::Say => {
                    for piece in 0..3u8 {
                        let _ = out
                            .send(Chunk {
                                sample_rate: 24_000,
                                pcm: vec![piece, 0, piece, 0],
                            })
                            .await;
                    }
                    let (device, gpu_reason) = {
                        let mut done = self.script.done.lock().unwrap();
                        if done.is_empty() {
                            (Device::AppleGpu, None)
                        } else {
                            done.remove(0)
                        }
                    };
                    Ok(Done { device, gpu_reason })
                }
                Next::Hold => {
                    let _ = out
                        .send(Chunk {
                            sample_rate: 24_000,
                            pcm: vec![0, 0],
                        })
                        .await;
                    self.script.release.notified().await;
                    let _ = out
                        .send(Chunk {
                            sample_rate: 24_000,
                            pcm: vec![1, 0],
                        })
                        .await;
                    Ok(Done {
                        device: Device::AppleGpu,
                        gpu_reason: None,
                    })
                }
                Next::SayThenFail => {
                    for piece in 0..3u8 {
                        let _ = out
                            .send(Chunk {
                                sample_rate: 24_000,
                                pcm: vec![piece, 0],
                            })
                            .await;
                    }
                    self.alive.store(false, Ordering::SeqCst);
                    Err(Failure::WorkerFailed)
                }
                Next::Exit => {
                    self.alive.store(false, Ordering::SeqCst);
                    Err(Failure::WorkerFailed)
                }
                Next::Silent => {
                    tokio::time::sleep(REQUEST_LIMIT).await;
                    Err(Failure::TimedOut)
                }
            }
        })
    }

    fn is_alive(&mut self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    fn close(self: Box<Self>) -> BoxFuture<'static, ()> {
        Box::pin(async move {
            self.script.closed.fetch_add(1, Ordering::SeqCst);
        })
    }
}

pub(crate) struct FakeLauncher(pub(crate) Arc<Script>);

impl Launcher for FakeLauncher {
    fn launch<'a>(
        &'a self,
        _load: &'a Load,
        _voice: &'a str,
    ) -> BoxFuture<'a, Result<Box<dyn Session>, Failure>> {
        Box::pin(async move {
            self.0.launches.fetch_add(1, Ordering::SeqCst);
            let failure = {
                let mut failures = self.0.launch_failures.lock().unwrap();
                (!failures.is_empty()).then(|| failures.remove(0))
            };
            if let Some(failure) = failure {
                return Err(failure);
            }
            let mut ready = ready();
            {
                let mut reasons = self.0.ready_gpu_reasons.lock().unwrap();
                if !reasons.is_empty() {
                    ready.device = Device::Cpu;
                    ready.gpu_reason = Some(reasons.remove(0));
                }
            }
            Ok(Box::new(FakeSession {
                script: Arc::clone(&self.0),
                alive: Arc::new(AtomicBool::new(true)),
                ready,
            }) as Box<dyn Session>)
        })
    }
}

pub(crate) struct FakeHost {
    pub(crate) free: AtomicU64,
    pub(crate) gateway: Mutex<Option<Ipv4Addr>>,
}

impl FakeHost {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            free: AtomicU64::new(u64::MAX),
            gateway: Mutex::new(Some(Ipv4Addr::new(192, 168, 68, 1))),
        })
    }

    pub(crate) fn on_hotspot(&self) {
        *self.gateway.lock().unwrap() = Some(Ipv4Addr::new(172, 20, 10, 1));
    }
}

impl Host for FakeHost {
    fn free_bytes<'a>(&'a self, _: &'a Path) -> BoxFuture<'a, Option<u64>> {
        Box::pin(async move { Some(self.free.load(Ordering::SeqCst)) })
    }

    fn gateway(&self) -> BoxFuture<'_, Option<Ipv4Addr>> {
        Box::pin(async move { *self.gateway.lock().unwrap() })
    }
}

#[derive(Default)]
pub(crate) struct FakeHub {
    pub(crate) files: Mutex<Vec<RemoteFile>>,
    pub(crate) list_error: Mutex<Option<&'static str>>,
    pub(crate) checks: Mutex<Vec<Result<u64, &'static str>>>,
    pub(crate) downloads: Mutex<Vec<Result<u64, &'static str>>>,
    pub(crate) calls: Mutex<Vec<String>>,
    /// A download waits for this before it ends, when set.
    pub(crate) gate: Mutex<Option<Arc<Notify>>>,
    pub(crate) running: AtomicUsize,
    pub(crate) most_running: AtomicUsize,
}

impl FakeHub {
    pub(crate) fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl Fetcher for FakeHub {
    fn list_files<'a>(
        &'a self,
        repo: &'a str,
        _: &'a str,
    ) -> BoxFuture<'a, Result<Vec<RemoteFile>, &'static str>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(format!("list {repo}"));
            match *self.list_error.lock().unwrap() {
                Some(reason) => Err(reason),
                None => Ok(self.files.lock().unwrap().clone()),
            }
        })
    }

    fn check<'a>(
        &'a self,
        repo: &'a str,
        _: &'a str,
        _: &'a Path,
    ) -> BoxFuture<'a, Result<u64, &'static str>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(format!("check {repo}"));
            let mut checks = self.checks.lock().unwrap();
            if checks.is_empty() {
                Err("missing")
            } else {
                checks.remove(0)
            }
        })
    }

    fn download<'a>(
        &'a self,
        repo: &'a str,
        _: &'a str,
        _: &'a Path,
        progress: &'a (dyn Fn(u64, u64) + Send + Sync),
    ) -> BoxFuture<'a, Result<u64, &'static str>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(format!("download {repo}"));
            let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
            self.most_running.fetch_max(now, Ordering::SeqCst);
            progress(10, 100);
            let gate = self.gate.lock().unwrap().clone();
            if let Some(gate) = gate {
                gate.notified().await;
            }
            self.running.fetch_sub(1, Ordering::SeqCst);
            let mut downloads = self.downloads.lock().unwrap();
            if downloads.is_empty() {
                Ok(100)
            } else {
                downloads.remove(0)
            }
        })
    }
}

#[derive(Default)]
pub(crate) struct MemoryDefault(pub(crate) Mutex<Option<(String, String)>>);

impl DefaultRow for MemoryDefault {
    fn load(&self) -> BoxFuture<'_, Option<(String, String)>> {
        Box::pin(async move { self.0.lock().unwrap().clone() })
    }

    fn save<'a>(&'a self, model: &'a str, voice: &'a str) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            *self.0.lock().unwrap() = Some((model.to_string(), voice.to_string()));
            Ok(())
        })
    }
}

pub(crate) struct Scratch {
    pub(crate) root: PathBuf,
    pub(crate) host: Arc<FakeHost>,
    pub(crate) hub: Arc<FakeHub>,
    pub(crate) default_row: Arc<MemoryDefault>,
}

impl Scratch {
    pub(crate) fn new() -> Self {
        let root = std::env::temp_dir().join(format!("harold-tts-models-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("cache")).unwrap();
        Self {
            root,
            host: FakeHost::new(),
            hub: Arc::new(FakeHub::default()),
            default_row: Arc::new(MemoryDefault::default()),
        }
    }

    pub(crate) fn folder(&self) -> PathBuf {
        self.root.join("tts-models")
    }

    pub(crate) fn cache(&self) -> PathBuf {
        self.root.join("cache")
    }

    pub(crate) fn approved_path(&self) -> PathBuf {
        self.root.join("tts-models.toml")
    }

    pub(crate) fn approve(&self, repo: &str, revision: &str, worker: &str) {
        let text = format!(
            "[[model]]\nsource = \"huggingface\"\nrepo = \"{repo}\"\nrevision = \"{revision}\"\nworker = \"{worker}\"\nlicence = \"MIT\"\naudit = \"row 1\"\n"
        );
        let mut existing = std::fs::read_to_string(self.approved_path()).unwrap_or_default();
        existing.push_str(&text);
        std::fs::write(self.approved_path(), existing).unwrap();
        std::fs::set_permissions(self.approved_path(), std::fs::Permissions::from_mode(0o600))
            .unwrap();
    }

    pub(crate) fn deps(&self) -> Deps {
        Deps {
            folder: self.folder(),
            approved_list: self.approved_path(),
            env: Env {
                path: "/usr/bin:/bin".into(),
                home: self.root.clone(),
                hf_home: None,
                hf_hub_cache: Some(self.cache().display().to_string()),
            },
            host: self.host.clone(),
            fetcher: self.hub.clone(),
            release: Arc::new(ReleaseDownloader::production()),
            default_row: self.default_row.clone(),
        }
    }

    pub(crate) fn registry(&self) -> Arc<Registry> {
        Registry::new(self.deps(), self.kokoro())
    }

    /// Kokoro as a Hugging Face model on this build's worker.
    pub(crate) fn kokoro(&self) -> Row {
        Row {
            id: KOKORO_MLX.to_string(),
            source: Source::HuggingFace,
            revision: KOKORO_REVISION.to_string(),
            worker: WorkerKind::THIS_BUILD,
            phase: Phase::Checking,
            size: 100,
            voices: Vec::new(),
            asked: false,
        }
    }

    /// A snapshot of `model` with a file of `size` bytes, and a record that lists it.
    pub(crate) fn cached(&self, model: &str, revision: &str, size: usize) {
        let snapshot = snapshot_dir(&self.cache(), model, revision);
        std::fs::create_dir_all(snapshot.join("voices")).unwrap();
        std::fs::write(snapshot.join("model.safetensors"), vec![0u8; size]).unwrap();
        std::fs::write(snapshot.join("voices/af_heart.safetensors"), b"v").unwrap();
        std::fs::write(snapshot.join("voices/am_adam.safetensors"), b"v").unwrap();
        std::fs::create_dir_all(self.folder()).unwrap();
        let record = json!({
            "model": model,
            "commit": revision,
            "files": [
                {"path": "model.safetensors", "size": size},
                {"path": "voices/af_heart.safetensors", "size": 1},
                {"path": "voices/am_adam.safetensors", "size": 1},
            ],
        });
        std::fs::write(
            self.folder().join(record_name(model, revision)),
            record.to_string(),
        )
        .unwrap();
    }

    pub(crate) fn safe_files(&self) {
        *self.hub.files.lock().unwrap() = vec![
            RemoteFile {
                path: "model.safetensors".into(),
                size: 90,
            },
            RemoteFile {
                path: "config.json".into(),
                size: 10,
            },
        ];
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Waits until `done` holds for the registry, with a limit.
pub(crate) async fn until(registry: &Arc<Registry>, done: impl Fn(&Registry) -> bool) {
    let mut changes = registry.subscribe();
    timeout(Duration::from_secs(10), async {
        while !done(registry) {
            if changes.changed().await.is_err() {
                return;
            }
        }
    })
    .await
    .expect("the registry never reached the state");
}

/// A server with Kokoro ready, over a fake worker. The model has an id of its own, so a test
/// can find its spans.
pub(crate) struct Fixture {
    pub(crate) scratch: Scratch,
    pub(crate) server: Arc<super::TtsServer>,
    pub(crate) script: Arc<Script>,
    pub(crate) model: String,
}

pub(crate) async fn fixture() -> Fixture {
    let scratch = Scratch::new();
    let model = format!(
        "org/kokoro-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    );
    let mut kokoro = scratch.kokoro();
    kokoro.id = model.clone();
    scratch.cached(&model, crate::settings::KOKORO_REVISION, 100);
    let registry = Registry::new(scratch.deps(), kokoro);
    registry.start_up();
    until(&registry, |registry| {
        registry.default_status() == DefaultStatus::Ready
    })
    .await;
    let script = Arc::new(Script::default());
    let supervisor = Supervisor::new(
        Arc::new(FakeLauncher(Arc::clone(&script))),
        Duration::from_secs(10 * 60),
    );
    let server = super::TtsServer::new(registry, supervisor);
    Fixture {
        scratch,
        server,
        script,
        model,
    }
}
