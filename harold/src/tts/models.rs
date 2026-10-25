//! The TTS models of this machine: which ones there are, whether each is on the disk and
//! checked, the default model and voice, and the background work that brings the disk to the
//! list (ADR 0011's rules: pinned commits, Harold's own file check, one download at a time,
//! none on the iPhone's hotspot until a call or the user starts one).
//!
//! Kokoro is the default model. On macOS it is `mlx-community/Kokoro-82M-bf16` from Hugging Face;
//! on Linux it is the two files of the `kokoro-onnx` GitHub release. Any other model comes
//! through `add_model`, and Harold downloads it only when it is on the approved model list.

use super::approved::{self, Approved};
use super::fetch::{
    DISK_MARGIN, Env, Host, METERED_RECHECK, download_slot, is_hotspot, is_model_name, is_revision,
    snapshot_dir,
};
use super::hub::{Fetcher, RemoteFile};
use super::release_download::{self, ReleaseDownloader};
use super::worker::{Load, WorkerKind};
use crate::settings::TtsSettings;
use crate::tools::BoxFuture;
use serde_json::{Value, json};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use tokio::sync::{Notify, watch};
use tokio::time::{Instant, sleep};
use tracing::{info, warn};

/// The Kokoro model's default voice.
pub(crate) const DEFAULT_VOICE: &str = "af_heart";
/// The Kokoro model of macOS.
pub(crate) const KOKORO_MLX: &str = "mlx-community/Kokoro-82M-bf16";
/// Its size in bytes at the pinned commit.
const KOKORO_MLX_SIZE: u64 = 389_410_807;

/// The reasons a call is not given another download at once.
const NO_RETRY: [&str; 2] = ["disk_full", "not_allowed"];

/// Weight files that can run code when they load: refused.
const UNSAFE_EXTENSIONS: [&str; 6] = ["bin", "pt", "pth", "pkl", "ckpt", "pickle"];
/// Weight files that are safe to load.
const SAFE_EXTENSIONS: [&str; 3] = ["safetensors", "onnx", "gguf"];

/// Where a model comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    HuggingFace,
    GithubRelease,
}

impl Source {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Source::HuggingFace => "huggingface",
            Source::GithubRelease => "github_release",
        }
    }
}

/// Where one model stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    /// Not known yet, or being looked at.
    Checking,
    /// A download is held back by a metered network.
    Waiting,
    Downloading {
        bytes: u64,
        total: u64,
    },
    Ready,
    Failed(&'static str),
    WaitingForApproval,
}

/// One TTS model.
#[derive(Debug, Clone)]
pub(crate) struct Row {
    pub id: String,
    pub source: Source,
    pub revision: String,
    pub worker: WorkerKind,
    pub phase: Phase,
    pub size: u64,
    pub voices: Vec<String>,
    /// The user asked for it: a metered network does not hold its download back.
    pub asked: bool,
}

/// The model rows as the gRPC service shows them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelInfo {
    pub id: String,
    pub source: &'static str,
    pub revision: String,
    pub worker: WorkerKind,
    pub voices: Vec<String>,
    pub state: InfoState,
    pub reason: String,
    pub size_bytes: u64,
    pub is_default: bool,
    pub default_voice: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InfoState {
    Ready,
    Downloading,
    Failed,
    WaitingForApproval,
}

/// Where the default model stands, for the call that waits for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DefaultStatus {
    Checking,
    Waiting,
    Downloading {
        model: String,
        bytes: u64,
        total: u64,
    },
    Ready,
    Failed {
        model: String,
        reason: &'static str,
        revision: String,
        source: Source,
    },
}

/// What a request resolves to: a ready model, its worker's files and the voice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Resolved {
    pub id: String,
    pub voice: String,
    pub load: Load,
}

/// Why a request names no ready model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResolveError {
    UnknownModel,
    UnknownVoice,
    /// Being checked, held back or downloading.
    Downloading,
    /// Failed, or waiting for approval, or its files are not where the worker needs them.
    Missing(&'static str),
}

/// Why `add_model` refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AddRefusal {
    UnsupportedSource,
    InvalidRepo,
    InvalidRevision,
    NotApproved,
    UnsupportedWorker,
    UnsafeFormat,
    /// The Hub could not be asked: network, not_found or not_allowed.
    Unavailable(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemoveRefusal {
    UnknownModel,
    DefaultModel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SetDefaultRefusal {
    UnknownModel,
    UnknownVoice,
    NotReady,
    Unavailable,
}

/// The default model and voice, kept in Harold's store.
pub(crate) trait DefaultRow: Send + Sync + 'static {
    fn load(&self) -> BoxFuture<'_, Option<(String, String)>>;
    fn save<'a>(&'a self, model: &'a str, voice: &'a str) -> BoxFuture<'a, Result<(), String>>;
}

/// What the registry works with.
pub(crate) struct Deps {
    /// `tts-models/` in the data folder.
    pub folder: PathBuf,
    pub approved_list: PathBuf,
    pub env: Env,
    pub host: Arc<dyn Host>,
    pub fetcher: Arc<dyn Fetcher>,
    pub release: Arc<ReleaseDownloader>,
    pub default_row: Arc<dyn DefaultRow>,
}

#[derive(Default)]
struct Pass {
    running: bool,
    /// A request came while a pass ran: it looks again before it ends.
    again: bool,
    /// A call is waiting: a metered network no longer holds the download back.
    now: bool,
}

struct Inner {
    rows: Vec<Row>,
    default_model: String,
    default_voice: String,
}

pub(crate) struct Registry {
    deps: Deps,
    /// The built-in Kokoro model's id: it cannot be removed.
    kokoro: String,
    inner: Mutex<Inner>,
    changed: watch::Sender<u64>,
    pass: Mutex<Pass>,
    wake: Notify,
}

/// Kokoro for this build, pinned by `tts`.
pub(crate) fn kokoro_row(tts: &TtsSettings) -> Row {
    match WorkerKind::THIS_BUILD {
        WorkerKind::Mlx => {
            let valid = is_revision(&tts.model_revision);
            if !valid {
                warn!("tts model setting refused");
            }
            Row {
                id: KOKORO_MLX.to_string(),
                source: Source::HuggingFace,
                revision: tts.model_revision.clone(),
                worker: WorkerKind::Mlx,
                phase: if valid {
                    Phase::Checking
                } else {
                    Phase::Failed("not_allowed")
                },
                size: KOKORO_MLX_SIZE,
                voices: Vec::new(),
                asked: false,
            }
        }
        WorkerKind::Onnx => Row {
            id: release_download::MODEL_ID.to_string(),
            source: Source::GithubRelease,
            revision: release_download::RELEASE_TAG.to_string(),
            worker: WorkerKind::Onnx,
            phase: Phase::Checking,
            size: release_download::total_size(),
            voices: Vec::new(),
            asked: false,
        },
    }
}

/// `<org>--<name>@<revision>.json`, the record of a model's check.
pub(crate) fn record_name(id: &str, revision: &str) -> String {
    format!("{}@{revision}.json", id.replace('/', "--"))
}

impl Registry {
    /// The registry with Kokoro and the models asked for before (`requests.json`). It reads the
    /// disk and the approved list, and asks the network nothing.
    pub(crate) fn new(deps: Deps, kokoro: Row) -> Arc<Self> {
        let kokoro_id = kokoro.id.clone();
        let mut rows = vec![kokoro];
        let approved = approved::read(&deps.approved_list);
        for (repo, revision) in read_requests(&deps.folder) {
            if rows
                .iter()
                .any(|row| row.id == repo && row.revision == revision)
            {
                continue;
            }
            let phase = match approved::find(&approved, &repo, &revision) {
                None => Phase::WaitingForApproval,
                Some(entry) if entry.worker != WorkerKind::THIS_BUILD => continue,
                Some(_) => Phase::Checking,
            };
            rows.push(Row {
                id: repo,
                source: Source::HuggingFace,
                revision,
                worker: WorkerKind::THIS_BUILD,
                phase,
                size: 0,
                voices: Vec::new(),
                asked: true,
            });
        }
        Arc::new(Self {
            deps,
            inner: Mutex::new(Inner {
                rows,
                default_model: kokoro_id.clone(),
                default_voice: DEFAULT_VOICE.to_string(),
            }),
            kokoro: kokoro_id,
            changed: watch::channel(0).0,
            pass: Mutex::default(),
            wake: Notify::new(),
        })
    }

    /// Uses `voice` as the default voice until the store holds a default: the voice of a
    /// hand-written Kokoro command that became the server.
    pub(crate) fn start_with_voice(&self, voice: &str) {
        if !voice.is_empty() {
            self.inner().default_voice = voice.to_string();
        }
    }

    /// Reads the default model and voice from the store, when it holds a pair that names a
    /// model this machine has.
    pub(crate) async fn load_default(&self) {
        let Some((model, voice)) = self.deps.default_row.load().await else {
            return;
        };
        let mut inner = self.inner();
        if inner.rows.iter().any(|row| row.id == model) {
            inner.default_model = model;
            inner.default_voice = voice;
        }
        drop(inner);
        self.bump();
    }

    fn inner(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn pass(&self) -> MutexGuard<'_, Pass> {
        self.pass.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn bump(&self) {
        self.changed.send_modify(|count| *count += 1);
    }

    /// Wakes when anything about the models changes.
    pub(crate) fn subscribe(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    fn set_phase(&self, id: &str, revision: &str, phase: Phase) {
        let mut inner = self.inner();
        if let Some(row) = inner
            .rows
            .iter_mut()
            .find(|row| row.id == id && row.revision == revision)
        {
            row.phase = phase;
        }
        drop(inner);
        self.bump();
    }

    /// Looks at the models in the background, and downloads one that is missing or fails its
    /// check. Called once at start-up.
    pub(crate) fn start_up(self: &Arc<Self>) {
        self.kick(false);
    }

    /// A call is starting. A download held back by a metered network starts at once, and one
    /// that failed starts again, unless it failed for lack of disk or permission.
    pub(crate) fn for_call(self: &Arc<Self>) {
        let mut retried = false;
        {
            let mut inner = self.inner();
            for row in &mut inner.rows {
                if matches!(row.phase, Phase::Failed(reason) if !NO_RETRY.contains(&reason)) {
                    row.phase = Phase::Checking;
                    retried = true;
                }
            }
        }
        if retried {
            self.bump();
        }
        self.kick(true);
    }

    /// Asks for a pass over the models; `now` is a call or the user, who will not wait for
    /// Wi-Fi.
    fn kick(self: &Arc<Self>, now: bool) {
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
        let registry = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                registry.run_pass().await;
                let mut pass = registry.pass();
                if !std::mem::take(&mut pass.again) {
                    pass.running = false;
                    pass.now = false;
                    return;
                }
            }
        });
    }

    async fn run_pass(self: &Arc<Self>) {
        loop {
            let next = {
                let inner = self.inner();
                // The default first, then the others in the order they were asked for.
                let mut rows = inner.rows.iter().filter(|row| row.phase == Phase::Checking);
                rows.clone()
                    .find(|row| row.id == inner.default_model)
                    .or_else(|| rows.next())
                    .cloned()
            };
            let Some(row) = next else {
                return;
            };
            match row.source {
                Source::HuggingFace => self.ensure_hub(&row).await,
                Source::GithubRelease => self.ensure_release(&row).await,
            }
        }
    }

    fn record_path(&self, row: &Row) -> PathBuf {
        self.deps.folder.join(record_name(&row.id, &row.revision))
    }

    /// The folder of the release files of `row`.
    fn release_folder(&self, row: &Row) -> PathBuf {
        self.deps.folder.join(row.id.replace('/', "--"))
    }

    /// The files of `row`'s record, with their sizes, when the record lists only files that are
    /// still in `folder` with those sizes. A folder read; no file is opened but the record.
    fn record_holds(&self, row: &Row, folder: &Path) -> Option<u64> {
        record_holds_at(&self.record_path(row), folder)
    }

    /// The record the speech models kept for the same model before the TTS server took it
    /// over (`speech-models/` beside `tts-models/`), when it still holds.
    fn legacy_record_holds(&self, row: &Row, folder: &Path) -> Option<u64> {
        let legacy = self
            .deps
            .folder
            .parent()?
            .join("speech-models")
            .join(record_name(&row.id, &row.revision));
        record_holds_at(&legacy, folder)
    }

    /// Copies the legacy record to `record` (mode 0600). False when it cannot be copied.
    fn adopt_legacy_record(&self, row: &Row, record: &Path) -> bool {
        let Some(speech_models) = self
            .deps
            .folder
            .parent()
            .map(|data| data.join("speech-models"))
        else {
            return false;
        };
        let legacy = speech_models.join(record_name(&row.id, &row.revision));
        let Ok(text) = std::fs::read_to_string(legacy) else {
            return false;
        };
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.deps.folder)
            .and_then(|()| {
                std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .mode(0o600)
                    .open(record)
            })
            .and_then(|mut file| std::io::Write::write_all(&mut file, text.as_bytes()))
            .is_ok()
    }

    fn ready(&self, row: &Row, folder: &Path, size: u64) {
        let voices = voices_in(folder);
        let mut inner = self.inner();
        if let Some(found) = inner
            .rows
            .iter_mut()
            .find(|found| found.id == row.id && found.revision == row.revision)
        {
            found.phase = Phase::Ready;
            if size > 0 {
                found.size = size;
            }
            if !voices.is_empty() {
                found.voices = voices;
            }
        }
        drop(inner);
        self.bump();
    }

    fn failed(&self, row: &Row, reason: &'static str) {
        warn!(model = %row.id, reason, "tts model download failed");
        self.set_phase(&row.id, &row.revision, Phase::Failed(reason));
    }

    async fn ensure_hub(self: &Arc<Self>, row: &Row) {
        let snapshot = snapshot_dir(&self.deps.env.cache(), &row.id, &row.revision);
        if let Some(total) = self.record_holds(row, &snapshot) {
            self.ready(row, &snapshot, total);
            return;
        }
        let record = self.record_path(row);
        if !record.exists() {
            // Kokoro was a speech model before the TTS server: its record still holds, and a
            // start-up after the upgrade needs no network to check it again.
            if let Some(total) = self.legacy_record_holds(row, &snapshot)
                && self.adopt_legacy_record(row, &record)
            {
                self.ready(row, &snapshot, total);
                return;
            }
            if let Err(error) = std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&self.deps.folder)
            {
                warn!(error = %error, "tts model folder not made");
                self.failed(row, "failed");
                return;
            }
            match self
                .deps
                .fetcher
                .check(&row.id, &row.revision, &record)
                .await
            {
                Ok(bytes) => {
                    self.ready(row, &snapshot, bytes);
                    return;
                }
                Err("missing") => {}
                Err(reason) => {
                    self.failed(row, reason);
                    return;
                }
            }
        }
        if self.not_enough_disk(row).await {
            self.failed(row, "disk_full");
            return;
        }
        self.wait_while_metered(row).await;
        let _slot = download_slot().lock().await;
        info!(model = %row.id, total_bytes = row.size, "tts model download started");
        self.set_phase(
            &row.id,
            &row.revision,
            Phase::Downloading {
                bytes: 0,
                total: row.size,
            },
        );
        let started = Instant::now();
        let progress = |bytes: u64, total: u64| {
            let total = if total == 0 { row.size } else { total };
            self.set_phase(&row.id, &row.revision, Phase::Downloading { bytes, total });
        };
        match self
            .deps
            .fetcher
            .download(&row.id, &row.revision, &record, &progress)
            .await
        {
            Ok(bytes) => {
                let ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                info!(model = %row.id, bytes, ms, "tts model downloaded");
                self.ready(row, &snapshot, bytes);
            }
            Err(reason) => self.failed(row, reason),
        }
    }

    async fn ensure_release(self: &Arc<Self>, row: &Row) {
        let folder = self.release_folder(row);
        if let Some(total) = self.record_holds(row, &folder) {
            self.ready(row, &folder, total);
            return;
        }
        if self.not_enough_disk(row).await {
            self.failed(row, "disk_full");
            return;
        }
        self.wait_while_metered(row).await;
        let _slot = download_slot().lock().await;
        info!(model = %row.id, total_bytes = row.size, "tts model download started");
        self.set_phase(
            &row.id,
            &row.revision,
            Phase::Downloading {
                bytes: 0,
                total: row.size,
            },
        );
        let started = Instant::now();
        let progress = |bytes: u64| {
            self.set_phase(
                &row.id,
                &row.revision,
                Phase::Downloading {
                    bytes,
                    total: row.size,
                },
            );
        };
        match self
            .deps
            .release
            .download_all(&folder, &self.record_path(row), progress)
            .await
        {
            Ok(bytes) => {
                let ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                info!(model = %row.id, bytes, ms, "tts model downloaded");
                self.ready(row, &folder, bytes);
            }
            Err(reason) => self.failed(row, reason),
        }
    }

    /// Whether the disk lacks room for `row` and the margin. An unknown size needs only the
    /// margin.
    async fn not_enough_disk(&self, row: &Row) -> bool {
        let target = match row.source {
            Source::HuggingFace => self.deps.env.cache(),
            Source::GithubRelease => self.deps.folder.clone(),
        };
        let free = self.deps.host.free_bytes(&target).await;
        free.is_some_and(|free| free < row.size + DISK_MARGIN)
    }

    /// Holds a start-up download back while the machine is on the iPhone's hotspot, looking
    /// again every 10 minutes. A call or the user ends the wait.
    async fn wait_while_metered(&self, row: &Row) {
        let mut logged = false;
        loop {
            if row.asked || self.pass().now {
                return;
            }
            let hotspot = self.deps.host.gateway().await.is_some_and(is_hotspot);
            if !hotspot {
                return;
            }
            if !logged {
                logged = true;
                info!(reason = "metered", model = %row.id, "tts model download waiting");
            }
            self.set_phase(&row.id, &row.revision, Phase::Waiting);
            let woken = self.wake.notified();
            tokio::select! {
                () = sleep(METERED_RECHECK) => {}
                () = woken => {}
            }
        }
    }

    /// The models, for `ListModels`: Kokoro first, the others in the order they were asked for.
    pub(crate) fn list(&self) -> Vec<ModelInfo> {
        let inner = self.inner();
        inner
            .rows
            .iter()
            .map(|row| {
                let is_default = row.id == inner.default_model;
                let (state, reason) = match row.phase {
                    Phase::Ready => (InfoState::Ready, ""),
                    Phase::Failed(reason) => (InfoState::Failed, reason),
                    Phase::WaitingForApproval => (InfoState::WaitingForApproval, "not_approved"),
                    Phase::Checking | Phase::Waiting | Phase::Downloading { .. } => {
                        (InfoState::Downloading, "")
                    }
                };
                ModelInfo {
                    id: row.id.clone(),
                    source: row.source.as_str(),
                    revision: row.revision.clone(),
                    worker: row.worker,
                    voices: row.voices.clone(),
                    state,
                    reason: reason.to_string(),
                    size_bytes: row.size,
                    is_default,
                    default_voice: if is_default {
                        inner.default_voice.clone()
                    } else {
                        String::new()
                    },
                }
            })
            .collect()
    }

    /// The default model and voice.
    pub(crate) fn default_pair(&self) -> (String, String) {
        let inner = self.inner();
        (inner.default_model.clone(), inner.default_voice.clone())
    }

    /// Where the default model stands.
    pub(crate) fn default_status(&self) -> DefaultStatus {
        let inner = self.inner();
        let Some(row) = inner.rows.iter().find(|row| row.id == inner.default_model) else {
            return DefaultStatus::Checking;
        };
        match row.phase {
            Phase::Ready => DefaultStatus::Ready,
            Phase::Checking => DefaultStatus::Checking,
            Phase::Waiting => DefaultStatus::Waiting,
            Phase::Downloading { bytes, total } => DefaultStatus::Downloading {
                model: row.id.clone(),
                bytes,
                total,
            },
            Phase::Failed(reason) => DefaultStatus::Failed {
                model: row.id.clone(),
                reason,
                revision: row.revision.clone(),
                source: row.source,
            },
            Phase::WaitingForApproval => DefaultStatus::Failed {
                model: row.id.clone(),
                reason: "not_allowed",
                revision: row.revision.clone(),
                source: row.source,
            },
        }
    }

    /// Resolves a request's model and voice (empty for the defaults) to a ready model.
    pub(crate) fn resolve(&self, model: &str, voice: &str) -> Result<Resolved, ResolveError> {
        let inner = self.inner();
        let id = if model.is_empty() {
            inner.default_model.as_str()
        } else {
            model
        };
        let row = inner
            .rows
            .iter()
            .find(|row| row.id == id)
            .ok_or(ResolveError::UnknownModel)?;
        match row.phase {
            Phase::Ready => {}
            Phase::Failed(reason) => return Err(ResolveError::Missing(reason)),
            Phase::WaitingForApproval => return Err(ResolveError::Missing("not_approved")),
            Phase::Checking | Phase::Waiting | Phase::Downloading { .. } => {
                return Err(ResolveError::Downloading);
            }
        }
        let voice = if !voice.is_empty() {
            voice.to_string()
        } else if row.id == inner.default_model {
            inner.default_voice.clone()
        } else {
            row.voices
                .first()
                .cloned()
                .ok_or(ResolveError::UnknownVoice)?
        };
        if !row.voices.is_empty() && !row.voices.contains(&voice) {
            return Err(ResolveError::UnknownVoice);
        }
        let load = self
            .load_of(row)
            .ok_or(ResolveError::Missing("model_missing"))?;
        Ok(Resolved {
            id: row.id.clone(),
            voice,
            load,
        })
    }

    /// The files the worker loads for `row`.
    fn load_of(&self, row: &Row) -> Option<Load> {
        match (row.worker, row.source) {
            (WorkerKind::Mlx, _) => Some(Load::Mlx {
                model: row.id.clone(),
                snapshot: snapshot_dir(&self.deps.env.cache(), &row.id, &row.revision),
                commit: row.revision.clone(),
            }),
            (WorkerKind::Onnx, Source::GithubRelease) => {
                let folder = self.release_folder(row);
                Some(Load::Onnx {
                    model_file: folder.join(release_download::MODEL_FILE),
                    voices_file: folder.join(release_download::VOICES_FILE),
                })
            }
            (WorkerKind::Onnx, Source::HuggingFace) => {
                let snapshot = snapshot_dir(&self.deps.env.cache(), &row.id, &row.revision);
                onnx_files(&snapshot)
            }
        }
    }

    /// Notes the voices a worker reported for `id`.
    pub(crate) fn note_voices(&self, id: &str, voices: &[String]) {
        if voices.is_empty() {
            return;
        }
        let mut inner = self.inner();
        let Some(row) = inner.rows.iter_mut().find(|row| row.id == id) else {
            return;
        };
        if row.voices == voices {
            return;
        }
        row.voices = voices.to_vec();
        drop(inner);
        self.bump();
    }

    /// Asks for a model by its source, repository and pinned commit. Hugging Face is the only
    /// source asked for. The model is downloaded only when it is on the approved list; a model
    /// that is not is recorded as waiting for approval.
    pub(crate) async fn add_model(
        self: &Arc<Self>,
        source: &str,
        repo: &str,
        revision: &str,
    ) -> Result<(String, Phase), AddRefusal> {
        if source != "huggingface" {
            return Err(AddRefusal::UnsupportedSource);
        }
        if !is_model_name(repo) {
            return Err(AddRefusal::InvalidRepo);
        }
        if !is_revision(revision) {
            return Err(AddRefusal::InvalidRevision);
        }
        let approved = approved::read(&self.deps.approved_list);
        let Some(entry) = approved::find(&approved, repo, revision) else {
            self.record_waiting(repo, revision);
            warn!(model = %repo, reason = "not_approved", "tts model download failed");
            return Err(AddRefusal::NotApproved);
        };
        if entry.worker != WorkerKind::THIS_BUILD {
            return Err(AddRefusal::UnsupportedWorker);
        }
        // A model that is here already is not asked for again, but a failure is tried again.
        if let Some(row) = self
            .inner()
            .rows
            .iter_mut()
            .find(|row| row.id == repo && row.revision == revision)
            && !matches!(row.phase, Phase::WaitingForApproval | Phase::Failed(_))
        {
            return Ok((row.id.clone(), row.phase));
        }
        let files = match self.deps.fetcher.list_files(repo, revision).await {
            Ok(files) => files,
            Err(reason) => {
                warn!(model = %repo, reason, "tts model download failed");
                return Err(AddRefusal::Unavailable(reason));
            }
        };
        if !has_only_safe_weights(&files) {
            warn!(model = %repo, reason = "unsafe_format", "tts model download failed");
            return Err(AddRefusal::UnsafeFormat);
        }
        let size = files.iter().map(|file| file.size).sum();
        self.accept(entry, size);
        self.kick(true);
        Ok((repo.to_string(), Phase::Checking))
    }

    /// Records a model that is not on the list, so a person sees it asked for.
    fn record_waiting(&self, repo: &str, revision: &str) {
        {
            let mut inner = self.inner();
            if inner
                .rows
                .iter()
                .any(|row| row.id == repo && row.revision == revision)
            {
                return;
            }
            inner.rows.push(Row {
                id: repo.to_string(),
                source: Source::HuggingFace,
                revision: revision.to_string(),
                worker: WorkerKind::THIS_BUILD,
                phase: Phase::WaitingForApproval,
                size: 0,
                voices: Vec::new(),
                asked: true,
            });
        }
        self.bump();
        self.write_requests();
    }

    /// Puts an approved model in the list as ready to check and download.
    fn accept(&self, entry: &Approved, size: u64) {
        {
            let mut inner = self.inner();
            match inner
                .rows
                .iter_mut()
                .find(|row| row.id == entry.repo && row.revision == entry.revision)
            {
                Some(row) => {
                    row.phase = Phase::Checking;
                    row.size = size;
                    row.asked = true;
                }
                None => inner.rows.push(Row {
                    id: entry.repo.clone(),
                    source: Source::HuggingFace,
                    revision: entry.revision.clone(),
                    worker: entry.worker,
                    phase: Phase::Checking,
                    size,
                    voices: Vec::new(),
                    asked: true,
                }),
            }
        }
        self.bump();
        self.write_requests();
    }

    /// Removes a model from the list and its record. The files stay in the Hugging Face
    /// cache, which other programs share. Kokoro and the default model cannot be removed.
    pub(crate) fn remove_model(&self, id: &str) -> Result<(), RemoveRefusal> {
        let removed = {
            let mut inner = self.inner();
            if id == self.kokoro || id == inner.default_model {
                return Err(RemoveRefusal::DefaultModel);
            }
            let before = inner.rows.len();
            let mut removed = Vec::new();
            inner.rows.retain(|row| {
                if row.id == id {
                    removed.push(row.revision.clone());
                    false
                } else {
                    true
                }
            });
            if inner.rows.len() == before {
                return Err(RemoveRefusal::UnknownModel);
            }
            removed
        };
        for revision in removed {
            let _ = std::fs::remove_file(self.deps.folder.join(record_name(id, &revision)));
        }
        self.bump();
        self.write_requests();
        Ok(())
    }

    /// Sets the default model and voice, and keeps them in the store.
    pub(crate) async fn set_default(
        &self,
        model: &str,
        voice: &str,
    ) -> Result<(), SetDefaultRefusal> {
        {
            let inner = self.inner();
            let row = inner
                .rows
                .iter()
                .find(|row| row.id == model)
                .ok_or(SetDefaultRefusal::UnknownModel)?;
            if row.phase == Phase::WaitingForApproval {
                return Err(SetDefaultRefusal::NotReady);
            }
            if voice.is_empty()
                || (!row.voices.is_empty() && !row.voices.iter().any(|v| v == voice))
            {
                return Err(SetDefaultRefusal::UnknownVoice);
            }
        }
        self.deps
            .default_row
            .save(model, voice)
            .await
            .map_err(|_| SetDefaultRefusal::Unavailable)?;
        let mut inner = self.inner();
        inner.default_model = model.to_string();
        inner.default_voice = voice.to_string();
        drop(inner);
        self.bump();
        Ok(())
    }

    /// Writes the models that were asked for (not Kokoro) to `requests.json` (0600).
    fn write_requests(&self) {
        let asked: Vec<Value> = self
            .inner()
            .rows
            .iter()
            .filter(|row| row.id != self.kokoro)
            .map(|row| json!({"repo": row.id, "revision": row.revision}))
            .collect();
        let folder = &self.deps.folder;
        if std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(folder)
            .is_err()
        {
            return;
        }
        let temporary = folder.join("requests.json.tmp");
        let written = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temporary)
            .and_then(|mut file| {
                std::io::Write::write_all(&mut file, Value::Array(asked).to_string().as_bytes())
            })
            .and_then(|()| std::fs::rename(&temporary, folder.join("requests.json")));
        if written.is_err() {
            warn!("tts model requests not written");
        }
    }

    /// What the phone shows about the default model, when it did not come: the command that
    /// downloads it by hand.
    pub(crate) fn hand_command(&self, python: &str, model: &str, revision: &str) -> Option<String> {
        (is_model_name(model) && is_revision(revision)).then(|| {
            format!(
                "{python} -c \"from huggingface_hub import snapshot_download; snapshot_download('{model}', revision='{revision}', token=False)\""
            )
        })
    }
}

/// The total size of the files of the record at `record`, when it lists only files that are
/// still in `folder` with those sizes. A folder read; no file is opened but the record.
fn record_holds_at(record: &Path, folder: &Path) -> Option<u64> {
    let text = std::fs::read_to_string(record).ok()?;
    let record = serde_json::from_str::<Value>(&text).ok()?;
    let files = record["files"]
        .as_array()
        .filter(|files| !files.is_empty())?;
    let mut total = 0;
    for file in files {
        let (path, size) = (file["path"].as_str()?, file["size"].as_u64()?);
        let plain = Path::new(path)
            .components()
            .all(|part| matches!(part, Component::Normal(_)));
        let meta = std::fs::metadata(folder.join(path)).ok()?;
        if !plain || !meta.is_file() || meta.len() != size {
            return None;
        }
        total += size;
    }
    Some(total)
}

/// The models asked for before: `(repo, revision)` pairs from `requests.json`.
fn read_requests(folder: &Path) -> Vec<(String, String)> {
    let Ok(text) = std::fs::read_to_string(folder.join("requests.json")) else {
        return Vec::new();
    };
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let repo = item["repo"].as_str()?;
            let revision = item["revision"].as_str()?;
            (is_model_name(repo) && is_revision(revision))
                .then(|| (repo.to_string(), revision.to_string()))
        })
        .collect()
}

/// Whether the commit's weight files are all safe to load, and there is at least one.
fn has_only_safe_weights(files: &[RemoteFile]) -> bool {
    let extension = |file: &RemoteFile| {
        Path::new(&file.path)
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
    };
    let mut safe = false;
    for file in files {
        let Some(extension) = extension(file) else {
            continue;
        };
        if UNSAFE_EXTENSIONS.contains(&extension.as_str()) {
            return false;
        }
        safe |= SAFE_EXTENSIONS.contains(&extension.as_str());
    }
    safe
}

/// The voices a model's folder holds: the names of its `voices/*.safetensors` files.
fn voices_in(folder: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(folder.join("voices")) else {
        return Vec::new();
    };
    let mut voices: Vec<String> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            (path.extension()? == "safetensors")
                .then(|| path.file_stem()?.to_str().map(str::to_string))
                .flatten()
        })
        .collect();
    voices.sort();
    voices
}

/// The one `.onnx` file and the one voices file in the snapshot of an ONNX model.
fn onnx_files(snapshot: &Path) -> Option<Load> {
    let names: Vec<PathBuf> = std::fs::read_dir(snapshot)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .collect();
    let only = |keep: &dyn Fn(&str) -> bool| {
        let mut found = names.iter().filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(keep)
        });
        let first = found.next()?.clone();
        found.next().is_none().then_some(first)
    };
    Some(Load::Onnx {
        model_file: only(&|name| name.ends_with(".onnx"))?,
        voices_file: only(&|name| name.starts_with("voices"))?,
    })
}

#[cfg(test)]
#[path = "models_tests.rs"]
mod tests;

/// A listed file, for a test.
#[cfg(test)]
#[allow(non_snake_case)]
pub(crate) fn RemoteFile_for_test(path: &str) -> RemoteFile {
    RemoteFile {
        path: path.to_string(),
        size: 1,
    }
}
