//! Keeping the index current with the vault: the passes, the watcher and the hourly timer.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use tokio::sync::Notify;
use tracing::{info, warn};

use super::embed::{EmbedError, Embedder, ModelInfo};
use super::graph::runtime::GraphRuntime;
use super::graph::sync::{Desired, sync_vault};
use super::store::{IndexInfo, Store, StoreError};
use super::text::{ParsedNote, parse_note, passage_prompt, split_passages};

/// How long the vault must be quiet before a saved vault note is indexed.
pub(crate) const SETTLE: Duration = Duration::from_secs(2);
/// How often the whole vault is compared with the index.
pub(crate) const EVERY: Duration = Duration::from_secs(60 * 60);
/// Two missed hourly passes.
const STALE_AFTER: Duration = Duration::from_secs(2 * 60 * 60);

/// Why a pass failed: the words of `knowledge index failed`'s `reason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Failure {
    VaultUnreadable,
    WorkerFailed,
    StoreFailed,
    BucketUnreachable,
}

impl Failure {
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Failure::VaultUnreadable => "vault_unreadable",
            Failure::WorkerFailed => "worker_failed",
            Failure::StoreFailed => "store_failed",
            Failure::BucketUnreachable => "bucket_unreachable",
        }
    }
}

impl From<StoreError> for Failure {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::BucketUnreachable => Failure::BucketUnreachable,
            StoreError::StoreFailed => Failure::StoreFailed,
        }
    }
}

/// What the passes have done so far, for `status` and the stale check.
#[derive(Debug, Clone, Default)]
pub(crate) struct PassStatus {
    pub last_ok_ms: i64,
    pub last_ok: Option<Instant>,
    pub pending: usize,
    pub last_failure: Option<Failure>,
}

/// Why a vault note was not indexed.
enum Skip {
    /// The model took no embedding of this vault note; the others go on.
    Note,
    /// The pass cannot go on.
    Pass(Failure),
}

/// What one pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct PassCounts {
    pub notes: usize,
    pub passages: usize,
    pub removed: usize,
    pub embed_ms: u64,
}

/// One file of the vault.
struct VaultFile {
    name: String,
    path: String,
    mtime_ms: i64,
    text: String,
    hash: String,
}

pub(crate) struct Index {
    pub(crate) store: Arc<Store>,
    pub(crate) embedder: Arc<dyn Embedder>,
    vault: PathBuf,
    exclude: HashSet<String>,
    status: Mutex<PassStatus>,
    one_pass: tokio::sync::Mutex<()>,
    started: Instant,
    /// The knowledge graph's writer, when the graph is on: the pass writes vault notes and
    /// links to it.
    graph: std::sync::OnceLock<Arc<GraphRuntime>>,
}

pub(crate) fn now_ms() -> i64 {
    (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
}

impl Index {
    pub(crate) fn new(
        store: Arc<Store>,
        embedder: Arc<dyn Embedder>,
        vault: PathBuf,
        exclude: &[String],
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            embedder,
            vault,
            exclude: exclude.iter().cloned().collect(),
            status: Mutex::new(PassStatus::default()),
            one_pass: tokio::sync::Mutex::new(()),
            started: Instant::now(),
            graph: std::sync::OnceLock::new(),
        })
    }

    /// Makes the pass write vault notes and links to the knowledge graph.
    pub(crate) fn set_graph(&self, graph: Arc<GraphRuntime>) {
        let _ = self.graph.set(graph);
    }

    pub(crate) fn status(&self) -> PassStatus {
        self.status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn update(&self, change: impl FnOnce(&mut PassStatus)) {
        change(&mut self.status.lock().unwrap_or_else(PoisonError::into_inner));
    }

    pub(crate) fn vault(&self) -> &Path {
        &self.vault
    }

    /// Reads the vault: every `.md` file under it that is not excluded.
    fn read_vault(&self) -> Result<Vec<VaultFile>, ()> {
        let mut files = Vec::new();
        let mut stack = vec![self.vault.clone()];
        let mut first = true;
        while let Some(dir) = stack.pop() {
            let entries = match std::fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(_) if first => return Err(()),
                Err(_) => continue,
            };
            first = false;
            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(kind) = entry.file_type() else {
                    continue;
                };
                let hidden = entry.file_name().to_string_lossy().starts_with('.');
                if hidden {
                    continue;
                }
                if kind.is_dir() {
                    stack.push(path);
                    continue;
                }
                let Ok(relative) = path.strip_prefix(&self.vault) else {
                    continue;
                };
                let relative = relative.to_string_lossy().into_owned();
                let Some(name) = relative.strip_suffix(".md") else {
                    continue;
                };
                if self.exclude.contains(&relative) {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let mtime_ms = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_millis() as i64);
                files.push(VaultFile {
                    name: name.to_string(),
                    path: relative,
                    mtime_ms,
                    hash: hex::encode(Sha256::digest(text.as_bytes())),
                    text,
                });
            }
        }
        files.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(files)
    }

    /// Compares the vault with the index: embeds new and changed vault notes and removes
    /// deleted ones. With `rebuild` the index is cleared first.
    pub(crate) async fn pass(self: &Arc<Self>, rebuild: bool) -> Result<PassCounts, Failure> {
        let _one = self.one_pass.lock().await;
        let result = self.pass_inner(rebuild).await;
        match &result {
            Ok(counts) => {
                self.update(|s| {
                    s.last_ok = Some(Instant::now());
                    s.last_ok_ms = now_ms();
                    s.pending = 0;
                    s.last_failure = None;
                });
                info!(
                    notes = counts.notes,
                    count = counts.passages,
                    removed = counts.removed,
                    embed_ms = counts.embed_ms,
                    pending = 0,
                    last_ok_age_s = 0,
                    "knowledge indexed"
                );
            }
            Err(failure) => {
                self.update(|s| s.last_failure = Some(*failure));
                warn!(reason = failure.reason(), "knowledge index failed");
            }
        }
        result
    }

    async fn pass_inner(self: &Arc<Self>, rebuild: bool) -> Result<PassCounts, Failure> {
        let this = Arc::clone(self);
        let files = tokio::task::spawn_blocking(move || this.read_vault())
            .await
            .map_err(|_| Failure::StoreFailed)?
            .map_err(|()| Failure::VaultUnreadable)?;
        let store = Arc::clone(&self.store);
        if rebuild {
            // A model that cannot load must not leave the index empty.
            self.embedder
                .ready()
                .await
                .map_err(|_: EmbedError| Failure::WorkerFailed)?;
            blocking(&store, Store::clear).await?;
        }
        let mut known = blocking(&store, Store::index_info).await?;
        // A change of model empties the index without starting the worker: every passage
        // was embedded by the model that was there before.
        if let (Some(held), Some(configured)) = (&known, self.embedder.configured_model())
            && held.model != configured
        {
            blocking(&store, Store::clear).await?;
            known = None;
        }
        let mut stored: HashMap<String, String> = blocking(&store, Store::hashes).await?;
        let changed_count = files
            .iter()
            .filter(|f| stored.get(&f.name) != Some(&f.hash))
            .count();
        let removed: Vec<String> = stored
            .keys()
            .filter(|name| !files.iter().any(|f| &f.name == *name))
            .cloned()
            .collect();
        self.update(|s| s.pending = changed_count);

        let mut counts = PassCounts::default();
        let mut skipped = 0_usize;
        if changed_count > 0 || known.is_none() && !files.is_empty() {
            let info = self
                .embedder
                .ready()
                .await
                .map_err(|_| Failure::WorkerFailed)?;
            let wanted = IndexInfo::from(&info);
            if known.as_ref().is_some_and(|known| known != &wanted) {
                // Another model, another size or other prompts: every passage is stale.
                blocking(&store, Store::clear).await?;
                stored.clear();
            }
            let current = blocking(&store, Store::index_info).await?;
            if current.as_ref() != Some(&wanted) {
                let built_at = now_ms();
                let wanted_row = wanted.clone();
                blocking(&store, move |s| s.set_index_info(&wanted_row, built_at)).await?;
            }
            for file in &files {
                if stored.get(&file.name) == Some(&file.hash) {
                    continue;
                }
                match self.index_file(file, &info).await {
                    Ok(embedded) => {
                        counts.notes += 1;
                        counts.passages += embedded.0;
                        counts.embed_ms += embedded.1;
                    }
                    // One vault note the model cannot take does not stop the others; the
                    // pass reports the failure when it is done.
                    Err(Skip::Note) => skipped += 1,
                    Err(Skip::Pass(failure)) => return Err(failure),
                }
            }
        }
        for name in &removed {
            let name = name.clone();
            blocking(&store, move |s| s.delete_note(&name)).await?;
            counts.removed += 1;
        }
        self.sync_graph(&files).await;
        if skipped > 0 {
            return Err(Failure::WorkerFailed);
        }
        Ok(counts)
    }

    /// Writes what differs between the vault and the knowledge graph. A graph that cannot take
    /// it does not fail the pass: the index does not depend on the graph.
    async fn sync_graph(&self, files: &[VaultFile]) {
        let Some(runtime) = self.graph.get().filter(|runtime| runtime.enabled()) else {
            return;
        };
        let vault: Vec<Desired> = files
            .iter()
            .map(|file| {
                let note = parse_note(&file.name, &file.text);
                Desired {
                    name: file.name.clone(),
                    title: note.title,
                    kind: note.kind,
                    links: note.links,
                }
            })
            .collect();
        let synced = match runtime.writer().await {
            Ok(graph) => sync_vault(&graph, &vault).await.map(|_| ()),
            Err(error) => Err(error),
        };
        if let Err(error) = synced {
            warn!(reason = error.reason(), "knowledge graph sync failed");
        }
    }

    /// Embeds one vault note's passages and replaces its rows. Returns the passages
    /// embedded and the milliseconds the embedding took.
    async fn index_file(&self, file: &VaultFile, info: &ModelInfo) -> Result<(usize, u64), Skip> {
        let note: ParsedNote = parse_note(&file.name, &file.text);
        let passages = split_passages(&note.body);
        let prompts: Vec<String> = passages
            .iter()
            .map(|p| passage_prompt(&note.title, p))
            .collect();
        let started = Instant::now();
        let vectors = self
            .embedder
            .embed(prompts)
            .await
            .map_err(|error| match error {
                EmbedError::Failed => Skip::Note,
                EmbedError::Unavailable(_) => Skip::Pass(Failure::WorkerFailed),
            })?;
        let embed_ms = started.elapsed().as_millis() as u64;
        if vectors.iter().any(|v| v.len() != info.dimensions) {
            return Err(Skip::Note);
        }
        let rows: Vec<(String, Vec<f32>)> = passages.into_iter().zip(vectors).collect();
        let count = rows.len();
        let (path, hash, mtime) = (file.path.clone(), file.hash.clone(), file.mtime_ms);
        blocking(&self.store, move |s| {
            s.replace_note(&note, &path, &hash, mtime, &rows)
        })
        .await
        .map_err(Skip::Pass)?;
        Ok((count, embed_ms))
    }

    /// Logs `knowledge index stale` when no pass has succeeded for more than two hours.
    pub(crate) fn check_stale(&self) {
        let status = self.status();
        let age = status
            .last_ok
            .map_or_else(|| self.started.elapsed(), |at| at.elapsed());
        if age > STALE_AFTER {
            warn!(last_ok_age_s = age.as_secs(), "knowledge index stale");
        }
    }
}

/// Runs a database call on a blocking thread.
pub(crate) async fn blocking<T, F>(store: &Arc<Store>, call: F) -> Result<T, Failure>
where
    T: Send + 'static,
    F: FnOnce(&Store) -> Result<T, StoreError> + Send + 'static,
{
    let store = Arc::clone(store);
    tokio::task::spawn_blocking(move || call(&store))
        .await
        .map_err(|_| Failure::StoreFailed)?
        .map_err(Failure::from)
}

/// The watcher and the timer: a pass at start, one for each settled change of the vault, and
/// one every hour. Ends when `shutdown` closes.
pub(crate) async fn run(index: Arc<Index>, mut shutdown: tokio::sync::watch::Receiver<()>) {
    let changed = Arc::new(Notify::new());
    let _watcher = watch_vault(index.vault(), &changed);
    let mut timer = tokio::time::interval_at(tokio::time::Instant::now() + EVERY, EVERY);
    let _ = index.pass(false).await;
    loop {
        tokio::select! {
            _ = shutdown.changed() => return,
            _ = timer.tick() => {
                let _ = index.pass(false).await;
                index.check_stale();
            }
            _ = changed.notified() => {
                // Settle: wait until the vault has been quiet for SETTLE.
                loop {
                    tokio::select! {
                        _ = shutdown.changed() => return,
                        _ = tokio::time::sleep(SETTLE) => break,
                        _ = changed.notified() => {}
                    }
                }
                let _ = index.pass(false).await;
            }
        }
    }
}

fn watch_vault(vault: &Path, changed: &Arc<Notify>) -> Option<notify::RecommendedWatcher> {
    use notify::{EventKind, RecursiveMode, Watcher};
    let signal = Arc::clone(changed);
    let mut watcher =
        notify::recommended_watcher(move |event: Result<notify::Event, notify::Error>| {
            let Ok(event) = event else { return };
            if matches!(event.kind, EventKind::Access(_)) {
                return;
            }
            if event
                .paths
                .iter()
                .any(|p| p.extension().is_some_and(|e| e == "md") || p.extension().is_none())
            {
                signal.notify_one();
            }
        })
        .map_err(|_| warn!("knowledge vault watcher could not start; the hourly pass still runs"))
        .ok()?;
    watcher
        .watch(vault, RecursiveMode::Recursive)
        .map_err(|_| {
            warn!("knowledge vault watcher could not watch the vault; the hourly pass still runs")
        })
        .ok()?;
    Some(watcher)
}

#[cfg(test)]
#[path = "index_tests.rs"]
mod tests;
