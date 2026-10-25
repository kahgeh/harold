//! The two speech models the voice worker loads, Whisper and Kokoro: which commit of each
//! Harold loads, whether the Mac has it checked, and the download of a missing one at
//! start-up (ADR 0011). Harold downloads only the two repositories listed here, each pinned
//! to a commit, through `voice_download.py`, and checks every file. The voice worker never
//! downloads.

use super::Missing;
use super::answerer::{GroupGuard, kill_group};
use crate::settings::{TtsSettings, VoiceInputSettings};
use crate::tools::BoxFuture;
use serde_json::Value;
use std::net::Ipv4Addr;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{Notify, watch};
use tokio::time::{Instant, interval_at, sleep, timeout};
use tracing::{info, warn};

/// The download program, written into the speech-models folder before each run.
pub(crate) const VOICE_DOWNLOAD_PY: &str = include_str!("../voice_download.py");

pub(crate) const WHISPER: &str = "mlx-community/whisper-large-v3-turbo";
pub(crate) const KOKORO: &str = "mlx-community/Kokoro-82M-bf16";

/// The only repositories Harold downloads, and each one's size in bytes at the pinned commit
/// (for the disk check and the progress of a download).
const ALLOWED: [(&str, u64); 2] = [(WHISPER, 1_613_979_758), (KOKORO, 389_410_807)];
/// Free space needed on top of a model's size before its download starts.
const DISK_MARGIN: u64 = 1_000_000_000;
/// A run of the download program that has not finished by now is stopped.
pub(crate) const DOWNLOAD_LIMIT: Duration = Duration::from_secs(2 * 60 * 60);
/// How often the call screen's progress is read from the cache.
const PROGRESS_EVERY: Duration = Duration::from_secs(10);
/// How often a start-up download held back by a metered network looks again.
const METERED_RECHECK: Duration = Duration::from_secs(10 * 60);

/// The reasons a call is not given another download at once.
const NO_RETRY: [&str; 2] = ["disk_full", "not_allowed"];
/// What the download program may say went wrong; anything else is `failed`.
const REASONS: [&str; 5] = [
    "network",
    "not_found",
    "disk_full",
    "check_failed",
    "not_allowed",
];

/// 40 lowercase hex characters.
pub(crate) fn is_revision(text: &str) -> bool {
    text.len() == 40
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// `<org>/<name>` in letters, digits, `.`, `_` and `-`.
pub(crate) fn is_model_name(text: &str) -> bool {
    let word = |part: &str| {
        !part.is_empty()
            && !part.bytes().all(|byte| byte == b'.')
            && part
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    };
    text.split_once('/')
        .is_some_and(|(org, name)| word(org) && word(name))
}

/// What the download program and the cache are told about the Mac.
#[derive(Debug, Clone)]
pub(crate) struct Env {
    pub path: String,
    pub home: PathBuf,
    pub hf_home: Option<String>,
    pub hf_hub_cache: Option<String>,
}

impl Env {
    pub(crate) fn from_process() -> Self {
        let var = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
        Self {
            path: var("PATH").unwrap_or_else(|| "/usr/bin:/bin".to_string()),
            home: var("HOME").map(PathBuf::from).unwrap_or_default(),
            hf_home: var("HF_HOME"),
            hf_hub_cache: var("HF_HUB_CACHE"),
        }
    }

    /// `$HF_HUB_CACHE`, else `$HF_HOME/hub`, else `~/.cache/huggingface/hub`.
    pub(crate) fn cache(&self) -> PathBuf {
        if let Some(cache) = &self.hf_hub_cache {
            return PathBuf::from(cache);
        }
        if let Some(home) = &self.hf_home {
            return Path::new(home).join("hub");
        }
        self.home.join(".cache/huggingface/hub")
    }

    /// The only variables the download program is given.
    fn script_vars(&self) -> Vec<(&'static str, String)> {
        let mut vars = vec![
            ("PATH", self.path.clone()),
            ("HOME", self.home.display().to_string()),
        ];
        vars.extend(self.hf_home.clone().map(|home| ("HF_HOME", home)));
        vars.extend(
            self.hf_hub_cache
                .clone()
                .map(|cache| ("HF_HUB_CACHE", cache)),
        );
        vars
    }
}

/// `<cache>/models--<org>--<name>`.
fn repo_dir(cache: &Path, model: &str) -> PathBuf {
    cache.join(format!("models--{}", model.replace('/', "--")))
}

/// The folder of `model` at `commit`: `<cache>/models--<org>--<name>/snapshots/<commit>`.
pub(crate) fn snapshot_dir(cache: &Path, model: &str, commit: &str) -> PathBuf {
    repo_dir(cache, model).join("snapshots").join(commit)
}

/// A speech model pinned to a commit, ready to hand to the voice worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Pin {
    pub snapshot: PathBuf,
    pub commit: String,
}

/// The pin for `model` at `revision` when both are valid and the model is one of the two
/// Harold downloads.
pub(crate) fn pin(model: &str, revision: &str, env: &Env) -> Option<Pin> {
    managed(model, revision)?;
    Some(Pin {
        snapshot: snapshot_dir(&env.cache(), model, revision),
        commit: revision.to_string(),
    })
}

/// A model Harold downloads and checks.
#[derive(Debug, Clone)]
struct Managed {
    model: String,
    revision: String,
    size: u64,
}

fn managed(model: &str, revision: &str) -> Option<Managed> {
    let (_, size) = ALLOWED.iter().find(|(name, _)| *name == model)?;
    (is_model_name(model) && is_revision(revision)).then(|| Managed {
        model: model.to_string(),
        revision: revision.to_string(),
        size: *size,
    })
}

/// One of the two models as the settings name it.
#[derive(Debug, Clone)]
enum Want {
    Managed(Managed),
    /// Outside the list, or with a setting that is refused: never downloaded, never checked.
    Unmanaged {
        model: String,
        revision: String,
    },
}

/// Where the speech models stand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Status {
    /// Not known yet, or being looked at.
    Checking,
    /// Every model is on the Mac and checked.
    Ready,
    /// A start-up download is held back by a metered network.
    Waiting,
    Downloading {
        model: String,
        bytes: u64,
        total: u64,
    },
    Failed(Missing),
}

impl Status {
    /// How far a download has come, in whole percent.
    pub(crate) fn percent(&self) -> Option<u8> {
        let Status::Downloading { bytes, total, .. } = self else {
            return None;
        };
        let done = bytes.saturating_mul(100) / (*total).max(1);
        Some(u8::try_from(done.min(100)).unwrap_or(100))
    }
}

/// What the Mac says about itself: free disk space and the default route's gateway.
pub(crate) trait Host: Send + Sync + 'static {
    /// The bytes free on the disk holding `dir`.
    fn free_bytes<'a>(&'a self, dir: &'a Path) -> BoxFuture<'a, Option<u64>>;
    /// The default route's gateway.
    fn gateway(&self) -> BoxFuture<'_, Option<Ipv4Addr>>;
}

/// The Mac's own answers, from `df` and `route`.
pub(crate) struct SystemHost;

impl Host for SystemHost {
    fn free_bytes<'a>(&'a self, dir: &'a Path) -> BoxFuture<'a, Option<u64>> {
        Box::pin(async move {
            let mut existing = dir;
            while !existing.exists() {
                existing = existing.parent()?;
            }
            let output = Command::new("/bin/df")
                .arg("-Pk")
                .arg(existing)
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output()
                .await
                .ok()?;
            let text = String::from_utf8_lossy(&output.stdout);
            let available: u64 = text
                .lines()
                .nth(1)?
                .split_whitespace()
                .nth(3)?
                .parse()
                .ok()?;
            Some(available.saturating_mul(1024))
        })
    }

    fn gateway(&self) -> BoxFuture<'_, Option<Ipv4Addr>> {
        Box::pin(async move {
            let output = Command::new("/sbin/route")
                .args(["-n", "get", "default"])
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output()
                .await
                .ok()?;
            let text = String::from_utf8_lossy(&output.stdout);
            text.lines()
                .find_map(|line| line.trim().strip_prefix("gateway:"))
                .and_then(|gateway| gateway.trim().parse().ok())
        })
    }
}

/// The iPhone Personal Hotspot's own range, `172.20.10.0/28`.
pub(crate) fn is_hotspot(gateway: Ipv4Addr) -> bool {
    let [a, b, c, d] = gateway.octets();
    (a, b, c) == (172, 20, 10) && d < 16
}

#[derive(Default)]
struct Pass {
    running: bool,
    /// A request came while a pass ran: it looks again before it ends.
    again: bool,
    /// A call is waiting: a metered network no longer holds the download back.
    now: bool,
}

/// The speech models: what the settings want, what the Mac has, and the background work that
/// brings the second to the first.
pub(crate) struct SpeechModels {
    wants: Vec<Want>,
    python: String,
    folder: PathBuf,
    env: Env,
    host: Arc<dyn Host>,
    status: watch::Sender<Status>,
    pass: Mutex<Pass>,
    wake: Notify,
    /// A test's models do no work: the asks made of them are noted here, and a test sets
    /// the status.
    #[cfg(test)]
    asked: Option<Mutex<Vec<&'static str>>>,
}

impl SpeechModels {
    /// The models `voice_input` and `tts` name, each with its commit. A setting that is
    /// refused is logged and its model is not managed.
    pub(crate) fn new(
        voice_input: &VoiceInputSettings,
        tts: &TtsSettings,
        folder: PathBuf,
        env: Env,
        host: Arc<dyn Host>,
    ) -> Arc<Self> {
        let whisper = voice_input.whisper_model().unwrap_or(WHISPER);
        let kokoro = super::worker::tts_model(tts);
        let wants = [
            (whisper.to_string(), voice_input.model_revision.clone()),
            (kokoro, tts.model_revision.clone()),
        ]
        .into_iter()
        .map(|(model, revision)| {
            if let Some(managed) = managed(&model, &revision) {
                return Want::Managed(managed);
            }
            if !is_model_name(&model) || !is_revision(&revision) {
                warn!("speech model setting refused");
            }
            Want::Unmanaged { model, revision }
        })
        .collect();
        Arc::new(Self {
            wants,
            python: voice_input.command.clone(),
            folder,
            env,
            host,
            status: watch::channel(Status::Checking).0,
            pass: Mutex::default(),
            wake: Notify::new(),
            #[cfg(test)]
            asked: None,
        })
    }

    /// Models for a test of a call: they do no work, only note what they are asked, and
    /// start in `status`.
    #[cfg(test)]
    pub(crate) fn stubbed(status: Status) -> Arc<Self> {
        Arc::new(Self {
            wants: Vec::new(),
            python: "/venv/bin/python".into(),
            folder: PathBuf::new(),
            env: Env {
                path: String::new(),
                home: PathBuf::new(),
                hf_home: None,
                hf_hub_cache: None,
            },
            host: Arc::new(SystemHost),
            status: watch::channel(status).0,
            pass: Mutex::default(),
            wake: Notify::new(),
            asked: Some(Mutex::default()),
        })
    }

    /// What a test's models were asked, in order: `for_call` or `model_missing:<model>`.
    #[cfg(test)]
    pub(crate) fn asked(&self) -> Vec<&'static str> {
        self.asked
            .as_ref()
            .map(|asked| asked.lock().unwrap().clone())
            .unwrap_or_default()
    }

    /// Sets the status of a test's models.
    #[cfg(test)]
    pub(crate) fn set_status(&self, status: Status) {
        self.set(status);
    }

    #[cfg(test)]
    fn note(&self, ask: &'static str) -> bool {
        let Some(asked) = &self.asked else {
            return false;
        };
        asked.lock().unwrap().push(ask);
        true
    }

    pub(crate) fn status(&self) -> watch::Receiver<Status> {
        self.status.subscribe()
    }

    fn pass(&self) -> MutexGuard<'_, Pass> {
        self.pass.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn set(&self, status: Status) {
        self.status.send_replace(status);
    }

    /// Looks at the models in the background, and downloads one that is missing or fails its
    /// check. Called once at start-up.
    pub(crate) fn start_up(self: &Arc<Self>) {
        self.request(false);
    }

    /// A call has been tapped. A download held back by a metered network starts at once, and
    /// one that failed starts again, unless it failed for lack of disk or permission.
    pub(crate) fn for_call(self: &Arc<Self>) {
        #[cfg(test)]
        if self.note("for_call") {
            return;
        }
        let status = self.status.borrow().clone();
        match status {
            // A failure that can be mended: try again, without waiting for Wi-Fi.
            Status::Failed(missing) if self.retry_on_call(&missing) => {
                self.set(Status::Checking);
                self.request(true);
            }
            // A pass that is going, or waiting for Wi-Fi, goes on at once.
            _ => {
                let mut pass = self.pass();
                if pass.running {
                    pass.now = true;
                    drop(pass);
                    self.wake.notify_one();
                }
            }
        }
    }

    /// Whether a call tapped after `missing` tries again. Not after a full disk or a download
    /// that is not allowed, which end the call at once, unless the model is not one Harold
    /// manages: the user may have fetched it by hand from the notice.
    fn retry_on_call(&self, missing: &Missing) -> bool {
        if !NO_RETRY.contains(&missing.reason) {
            return true;
        }
        !self
            .wants
            .iter()
            .any(|want| matches!(want, Want::Managed(m) if m.model == missing.model))
    }

    /// The voice worker could not find `model`: look at it again, and download it if it is
    /// one Harold manages. `call_waiting` is a call that waits for it, which a metered network
    /// no longer holds back.
    pub(crate) fn model_missing(self: &Arc<Self>, model: &str, call_waiting: bool) {
        #[cfg(test)]
        if self.note("model_missing") {
            self.set(Status::Checking);
            return;
        }
        let managed = self
            .wants
            .iter()
            .any(|want| matches!(want, Want::Managed(m) if m.model == model));
        if managed {
            self.set(Status::Checking);
            self.request(call_waiting);
            return;
        }
        self.set(Status::Failed(self.missing(model, "not_allowed")));
    }

    /// What the phone shows about `model`, which did not come.
    pub(crate) fn missing(&self, model: &str, reason: &'static str) -> Missing {
        let revision = self.revision_of(model);
        let revision = revision.as_str();
        let command = (is_model_name(model) && is_revision(revision)).then(|| {
            format!(
                "{} -c \"from huggingface_hub import snapshot_download; snapshot_download('{model}', revision='{revision}', token=False)\"",
                self.python
            )
        });
        Missing {
            model: model.to_string(),
            reason,
            command,
        }
    }

    /// The revision `model` is pinned to.
    fn revision_of(&self, model: &str) -> String {
        self.wants
            .iter()
            .find_map(|want| match want {
                Want::Managed(m) if m.model == model => Some(m.revision.clone()),
                Want::Unmanaged { model: m, revision } if m == model => Some(revision.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }

    /// Asks for a pass over the models; `now` is a call that will not wait for Wi-Fi.
    fn request(self: &Arc<Self>, now: bool) {
        {
            let mut pass = self.pass();
            pass.now |= now;
            if pass.running {
                pass.again = true;
                drop(pass);
                self.wake.notify_one();
                return;
            }
            pass.running = true;
        }
        let models = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                models.run_pass().await;
                let mut pass = models.pass();
                if !std::mem::take(&mut pass.again) {
                    pass.running = false;
                    pass.now = false;
                    return;
                }
            }
        });
    }

    async fn run_pass(self: &Arc<Self>) {
        self.set(Status::Checking);
        for want in &self.wants {
            let Want::Managed(managed) = want else {
                continue;
            };
            self.set(Status::Checking);
            if self.record_holds(managed) {
                continue;
            }
            if let Err(reason) = self.settle(managed).await {
                let missing = self.missing(&managed.model, reason);
                self.set(Status::Failed(missing));
                return;
            }
        }
        self.set(Status::Ready);
    }

    fn record_path(&self, managed: &Managed) -> PathBuf {
        let name = managed.model.replace('/', "--");
        self.folder
            .join(format!("{name}@{}.json", managed.revision))
    }

    /// Whether the record of the last check lists only files that are still in the snapshot
    /// with their sizes. A folder read; no file is opened but the record.
    fn record_holds(&self, managed: &Managed) -> bool {
        let Ok(text) = std::fs::read_to_string(self.record_path(managed)) else {
            return false;
        };
        let Ok(record) = serde_json::from_str::<Value>(&text) else {
            return false;
        };
        let Some(files) = record["files"].as_array().filter(|files| !files.is_empty()) else {
            return false;
        };
        let snapshot = snapshot_dir(&self.env.cache(), &managed.model, &managed.revision);
        files.iter().all(|file| {
            let (Some(path), Some(size)) = (file["path"].as_str(), file["size"].as_u64()) else {
                return false;
            };
            let plain = Path::new(path)
                .components()
                .all(|part| matches!(part, Component::Normal(_)));
            plain
                && std::fs::metadata(snapshot.join(path))
                    .is_ok_and(|meta| meta.is_file() && meta.len() == size)
        })
    }

    /// Brings `managed` to a checked state: checks what is cached when no record was made,
    /// and downloads what is missing or wrong.
    async fn settle(self: &Arc<Self>, managed: &Managed) -> Result<(), &'static str> {
        if self.write_script().is_err() {
            return Err(self.not_downloaded(managed, "failed"));
        }
        if !self.record_path(managed).exists() {
            match self.run_script(managed, true).await {
                Outcome::Done(_) => return Ok(()),
                Outcome::Failed("missing") => {}
                Outcome::Failed(reason) => return Err(self.not_downloaded(managed, reason)),
            }
        }
        self.download(managed).await
    }

    fn not_downloaded(&self, managed: &Managed, reason: &'static str) -> &'static str {
        warn!(model = %managed.model, reason, "speech model not downloaded");
        reason
    }

    async fn download(self: &Arc<Self>, managed: &Managed) -> Result<(), &'static str> {
        let cache = self.env.cache();
        let free = self.host.free_bytes(&cache).await;
        if free.is_some_and(|free| free < managed.size + DISK_MARGIN) {
            return Err(self.not_downloaded(managed, "disk_full"));
        }
        self.wait_while_metered().await;
        info!(model = %managed.model, total_bytes = managed.size, "speech model download started");
        self.set(Status::Downloading {
            model: managed.model.clone(),
            bytes: 0,
            total: managed.size,
        });
        let started = Instant::now();
        match self.run_script(managed, false).await {
            Outcome::Done(bytes) => {
                let ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                info!(model = %managed.model, bytes, ms, "speech model downloaded");
                Ok(())
            }
            Outcome::Failed(reason) => Err(self.not_downloaded(managed, reason)),
        }
    }

    /// Holds a start-up download back while the Mac is on the iPhone's hotspot, looking again
    /// every 10 minutes. A call that is tapped ends the wait.
    async fn wait_while_metered(&self) {
        let mut logged = false;
        loop {
            if self.pass().now {
                return;
            }
            let hotspot = self.host.gateway().await.is_some_and(is_hotspot);
            if !hotspot {
                return;
            }
            if !logged {
                logged = true;
                info!(reason = "metered", "speech model download waiting");
            }
            self.set(Status::Waiting);
            let woken = self.wake.notified();
            tokio::select! {
                () = sleep(METERED_RECHECK) => {}
                () = woken => {}
            }
        }
    }

    /// Writes the download program into the speech-models folder (0700), as a file for the
    /// user only, never reusing one that is there.
    fn write_script(&self) -> std::io::Result<()> {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.folder)?;
        std::fs::set_permissions(&self.folder, std::fs::Permissions::from_mode(0o700))?;
        let script = self.folder.join("voice_download.py");
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
        std::io::Write::write_all(&mut file, VOICE_DOWNLOAD_PY.as_bytes())
    }

    /// Runs the download program on `managed`, in check mode or not, and follows its output.
    /// A run past `DOWNLOAD_LIMIT`, or a Harold that stops, ends the program's whole process
    /// group.
    async fn run_script(&self, managed: &Managed, check: bool) -> Outcome {
        let spawned = Command::new(&self.python)
            .arg("-I")
            .arg(self.folder.join("voice_download.py"))
            .arg(&managed.model)
            .arg(&managed.revision)
            .arg(self.record_path(managed))
            .args(check.then_some("--check"))
            .env_clear()
            .envs(self.env.script_vars())
            .current_dir(&self.folder)
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn();
        let Ok(mut child) = spawned else {
            return Outcome::Failed("failed");
        };
        let mut guard = GroupGuard(child.id());
        let Some(stdout) = child.stdout.take() else {
            return Outcome::Failed("failed");
        };
        let outcome = timeout(DOWNLOAD_LIMIT, self.follow(managed, stdout, check)).await;
        if let Some(group) = guard.0.take() {
            kill_group(group).await;
        }
        let _ = child.wait().await;
        outcome.unwrap_or(Outcome::Failed("failed"))
    }

    /// Reads the program's lines until it ends, and every `PROGRESS_EVERY` shows how many bytes
    /// the cache holds for the model.
    async fn follow(
        &self,
        managed: &Managed,
        stdout: tokio::process::ChildStdout,
        check: bool,
    ) -> Outcome {
        let mut lines = BufReader::new(stdout).lines();
        let mut total = managed.size;
        let blobs = repo_dir(&self.env.cache(), &managed.model).join("blobs");
        let mut ticker = interval_at(Instant::now() + PROGRESS_EVERY, PROGRESS_EVERY);
        let mut outcome = None;
        loop {
            tokio::select! {
                line = lines.next_line() => {
                    let Ok(Some(line)) = line else { break };
                    let Ok(reply) = serde_json::from_str::<Value>(&line) else { continue };
                    if let Some(size) = reply["total"].as_u64() {
                        total = size;
                    } else if reply["done"].as_bool() == Some(true) {
                        outcome = Some(Outcome::Done(reply["bytes"].as_u64().unwrap_or(total)));
                    } else if reply["done"].as_bool() == Some(false) {
                        let said = reply["reason"].as_str().unwrap_or_default();
                        let reason = if check && said == "missing" {
                            "missing"
                        } else {
                            REASONS.iter().copied().find(|known| *known == said).unwrap_or("failed")
                        };
                        outcome = Some(Outcome::Failed(reason));
                    }
                }
                _ = ticker.tick(), if !check => {
                    self.set(Status::Downloading {
                        model: managed.model.clone(),
                        bytes: folder_bytes(&blobs),
                        total,
                    });
                }
            }
        }
        outcome.unwrap_or(Outcome::Failed("failed"))
    }
}

/// How a run of the download program ended.
enum Outcome {
    Done(u64),
    Failed(&'static str),
}

/// The bytes of the files in `dir`, partial downloads included.
fn folder_bytes(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter_map(|entry| entry.metadata().ok())
        .filter(|meta| meta.is_file())
        .map(|meta| meta.len())
        .sum()
}

#[cfg(test)]
#[path = "models_tests.rs"]
mod tests;
