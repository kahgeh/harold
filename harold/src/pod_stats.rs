//! Pod facts: how many of each product's build slots are busy and free, and what merges
//! to each product's base branch. Every minute Harold reads the chief's `products.toml`
//! and runs read-only `git` against each product's base checkout. It never writes there.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use config::{Config, File, FileFormat};
use opentelemetry::KeyValue;
use opentelemetry::metrics::ObservableGauge;
use serde::Deserialize;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use tracing::warn;

use crate::settings::expand_tilde;
use crate::telemetry;

/// How often the pod facts are read.
const POLL_INTERVAL: Duration = Duration::from_secs(60);

/// How long one `git` command may take.
pub(crate) const GIT_TIMEOUT: Duration = Duration::from_secs(5);

/// The slots of a product whose row names none.
const DEFAULT_SLOTS: u32 = 3;

/// The file baton writes in a worktree when it starts a worker there. A worktree holding
/// it is a busy slot.
const AGENT_PANE: &str = ".superpowers/baton/agent-pane";

/// Why a product's facts, or the registry, could not be read. Each is fixed text: the
/// `reason` field is exported.
const NO_ANSWER: &str = "git gave no answer";
const GIT_FAILED: &str = "git failed";
const REGISTRY_UNREADABLE: &str = "registry unreadable";

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// ---------------------------------------------------------------------------
// The registry
// ---------------------------------------------------------------------------

/// One product of the chief's registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Product {
    pub name: String,
    /// The base checkout.
    pub repo: PathBuf,
    /// The base branch.
    pub base: String,
    pub slots: u32,
}

#[derive(Deserialize)]
struct Registry {
    #[serde(default)]
    products: BTreeMap<String, Row>,
}

/// A `[products.<name>]` table. Its other keys, such as `test` and `notes`, are ignored.
#[derive(Deserialize)]
struct Row {
    repo: String,
    base: String,
    #[serde(default = "default_slots")]
    slots: u32,
}

fn default_slots() -> u32 {
    DEFAULT_SLOTS
}

/// The products of the registry at `path`, by name, with a leading `~/` in `repo`
/// expanded.
pub(crate) fn read_registry(path: &Path) -> Result<Vec<Product>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let registry: Registry = Config::builder()
        .add_source(File::from_str(&text, FileFormat::Toml))
        .build()
        .and_then(Config::try_deserialize)
        .map_err(|error| format!("cannot parse {}: {error}", path.display()))?;
    Ok(registry
        .products
        .into_iter()
        .map(|(name, row)| Product {
            name,
            repo: expand_tilde(row.repo.trim()).into(),
            base: row.base,
            slots: row.slots,
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Git
// ---------------------------------------------------------------------------

pub(crate) struct GitOutput {
    pub success: bool,
    pub stdout: String,
}

pub(crate) trait Git: Send + Sync {
    /// `git -C <repo> <args>` with GIT_OPTIONAL_LOCKS=0; None on a timeout (5 s) or a start failure.
    fn run(&self, repo: &Path, args: &[&str]) -> impl Future<Output = Option<GitOutput>> + Send;
}

/// The `git` on the PATH. Harold runs only read-only commands with it.
pub(crate) struct SystemGit;

impl Git for SystemGit {
    fn run(&self, repo: &Path, args: &[&str]) -> impl Future<Output = Option<GitOutput>> + Send {
        let mut command = tokio::process::Command::new("git");
        command
            .arg("-C")
            .arg(repo)
            .args(args)
            // A read must not take the index lock that a worker's own git may want.
            .env("GIT_OPTIONAL_LOCKS", "0")
            .stdin(Stdio::null())
            .kill_on_drop(true);
        with_timeout(GIT_TIMEOUT, async move {
            let output = command.output().await.ok()?;
            Some(GitOutput {
                success: output.status.success(),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            })
        })
    }
}

/// `run`'s output, or None when it takes longer than `limit`. Dropping a `SystemGit` run
/// kills its `git`.
pub(crate) async fn with_timeout(
    limit: Duration,
    run: impl Future<Output = Option<GitOutput>>,
) -> Option<GitOutput> {
    tokio::time::timeout(limit, run).await.ok().flatten()
}

/// The output of a `git` command that succeeded.
async fn git_stdout(git: &impl Git, repo: &Path, args: &[&str]) -> Result<String, &'static str> {
    let output = git.run(repo, args).await.ok_or(NO_ANSWER)?;
    if output.success {
        Ok(output.stdout)
    } else {
        Err(GIT_FAILED)
    }
}

// ---------------------------------------------------------------------------
// Slots
// ---------------------------------------------------------------------------

pub(crate) struct Slots {
    pub busy: u32,
    pub free: u32,
    /// The busy worktrees as (folder name, path), in `git worktree list` order.
    #[allow(
        dead_code,
        reason = "the spec states read the building specs in a later telemetry task"
    )]
    pub busy_worktrees: Vec<(String, PathBuf)>,
}

/// The product's busy and free slots. A busy slot is a worktree, other than the base
/// checkout, that holds the agent-pane file. Free slots never go below 0.
pub(crate) async fn slots(git: &impl Git, product: &Product) -> Result<Slots, &'static str> {
    let listing = git_stdout(git, &product.repo, &["worktree", "list", "--porcelain"]).await?;
    let base = canonical(&product.repo).await;
    let mut busy_worktrees = Vec::new();
    for path in listing
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .map(PathBuf::from)
    {
        if canonical(&path).await == base || !is_file(&path.join(AGENT_PANE)).await {
            continue;
        }
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        busy_worktrees.push((name, path));
    }
    let busy = u32::try_from(busy_worktrees.len()).unwrap_or(u32::MAX);
    Ok(Slots {
        busy,
        free: product.slots.saturating_sub(busy),
        busy_worktrees,
    })
}

async fn canonical(path: &Path) -> PathBuf {
    tokio::fs::canonicalize(path)
        .await
        .unwrap_or_else(|_| path.to_path_buf())
}

async fn is_file(path: &Path) -> bool {
    tokio::fs::metadata(path)
        .await
        .is_ok_and(|metadata| metadata.is_file())
}

// ---------------------------------------------------------------------------
// Merges
// ---------------------------------------------------------------------------

/// One move of the base branch's tip that contains the old tip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Merge {
    /// `<id>` when the reflog subject is `merge <id>: ...`.
    pub spec_id: Option<String>,
    pub added: u64,
    pub removed: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TipMove {
    Unchanged,
    Merges(Vec<Merge>),
    /// The tip moved to a commit that does not contain the old one. Nothing is counted.
    Rewritten,
}

/// One entry of a branch's reflog, newest first.
pub(crate) struct ReflogEntry {
    /// The commit the branch moved to.
    pub hash: String,
    /// The committer time of that commit, in seconds since the epoch (`%ct`).
    #[allow(
        dead_code,
        reason = "the spec states read merge times in a later telemetry task"
    )]
    pub time: i64,
    /// The reflog subject, such as `merge chief-voice: Fast-forward`.
    pub subject: String,
}

/// The reflog of `refs/heads/<base>`, newest first. A branch without a reflog has no
/// entries.
pub(crate) async fn read_reflog(
    git: &impl Git,
    product: &Product,
) -> Result<Vec<ReflogEntry>, &'static str> {
    let branch = format!("refs/heads/{}", product.base);
    let text = git_stdout(
        git,
        &product.repo,
        &["reflog", "show", "--format=%H%x09%ct%x09%gs", &branch],
    )
    .await?;
    Ok(text
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(3, '\t');
            let hash = fields.next()?.to_owned();
            let time = fields.next()?.parse().ok()?;
            let subject = fields.next().unwrap_or_default().to_owned();
            Some(ReflogEntry {
                hash,
                time,
                subject,
            })
        })
        .collect())
}

/// `<id>` when `subject` is `merge <id>: ...`.
pub(crate) fn spec_id(subject: &str) -> Option<String> {
    let (id, _) = subject.strip_prefix("merge ")?.split_once(':')?;
    let id = id.trim();
    (!id.is_empty()).then(|| id.to_owned())
}

/// The numbers before `insertion` and `deletion` in `git diff --shortstat` output, each
/// 0 when missing.
pub(crate) fn shortstat_lines(text: &str) -> (u64, u64) {
    let (mut added, mut removed) = (0, 0);
    for part in text.split(',') {
        let Some((number, rest)) = part.trim().split_once(' ') else {
            continue;
        };
        let Ok(number) = number.parse() else {
            continue;
        };
        if rest.starts_with("insertion") {
            added = number;
        } else if rest.starts_with("deletion") {
            removed = number;
        }
    }
    (added, removed)
}

/// The last known tip of each product's base branch.
#[derive(Default)]
pub(crate) struct Tips(HashMap<String, String>);

impl Tips {
    /// First call for a product records its tip and reports Unchanged (start-up counts nothing).
    /// When reading fails, the known tip stays, so the next poll counts the whole move.
    pub(crate) async fn observe(
        &mut self,
        git: &impl Git,
        product: &Product,
    ) -> Result<TipMove, &'static str> {
        let branch = format!("refs/heads/{}", product.base);
        let tip = git_stdout(git, &product.repo, &["rev-parse", "--verify", &branch])
            .await?
            .trim()
            .to_owned();
        let Some(old) = self.0.get(&product.name).cloned() else {
            self.0.insert(product.name.clone(), tip);
            return Ok(TipMove::Unchanged);
        };
        if old == tip {
            return Ok(TipMove::Unchanged);
        }
        let contains_old = git
            .run(&product.repo, &["merge-base", "--is-ancestor", &old, &tip])
            .await
            .ok_or(NO_ANSWER)?
            .success;
        let moved = if contains_old {
            TipMove::Merges(merges(git, product, &old, &tip).await?)
        } else {
            warn!(pod.product = %product.name, "base branch rewritten; not counted");
            TipMove::Rewritten
        };
        self.0.insert(product.name.clone(), tip);
        Ok(moved)
    }
}

/// The merges from `old` to `new`: one for each reflog entry from just after `old` up to
/// `new`, or one from `old` to `new` with no spec when the reflog does not hold both.
async fn merges(
    git: &impl Git,
    product: &Product,
    old: &str,
    new: &str,
) -> Result<Vec<Merge>, &'static str> {
    let reflog = read_reflog(git, product).await?;
    let newest = reflog.iter().position(|entry| entry.hash == new);
    let oldest = newest.and_then(|newest| {
        let after = reflog[newest..]
            .iter()
            .position(|entry| entry.hash == old)?;
        Some(newest + after)
    });
    let steps: Vec<(&str, Option<String>)> = match (newest, oldest) {
        (Some(newest), Some(oldest)) if newest < oldest => reflog[newest..oldest]
            .iter()
            .rev()
            .map(|entry| (entry.hash.as_str(), spec_id(&entry.subject)))
            .collect(),
        _ => vec![(new, None)],
    };
    let mut merges = Vec::new();
    let mut from = old;
    for (to, spec_id) in steps {
        if to == from {
            continue;
        }
        let shortstat = git_stdout(git, &product.repo, &["diff", "--shortstat", from, to]).await?;
        let (added, removed) = shortstat_lines(&shortstat);
        merges.push(Merge {
            spec_id,
            added,
            removed,
        });
        from = to;
    }
    Ok(merges)
}

fn record_merges(product: &str, merges: &[Merge]) {
    let instruments = telemetry::instruments();
    for merge in merges {
        let mut attributes = vec![KeyValue::new("pod.product", product.to_owned())];
        if let Some(spec_id) = &merge.spec_id {
            attributes.push(KeyValue::new("spec.id", spec_id.clone()));
        }
        instruments.pod_merges.add(1, &attributes);
        for (change, lines) in [("added", merge.added), ("removed", merge.removed)] {
            instruments.pod_merged_lines.add(
                lines,
                &[
                    KeyValue::new("pod.product", product.to_owned()),
                    KeyValue::new("change", change),
                ],
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Polls
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SlotCounts {
    pub busy: u32,
    pub free: u32,
}

/// What the latest poll read, for the gauges. A product the poll could not read is left
/// out, so no gauge reports an old count.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct PodSnapshot {
    /// Slot counts by product.
    pub slots: BTreeMap<String, SlotCounts>,
}

pub(crate) struct PodStats<G: Git> {
    registry: PathBuf,
    git: G,
    tips: Tips,
    snapshot: Arc<Mutex<PodSnapshot>>,
    /// Whether the registry's failure has been logged since it was last read.
    registry_unreadable: bool,
    /// The products whose failure has been logged since they last worked.
    unavailable: HashSet<String>,
}

impl<G: Git> PodStats<G> {
    pub(crate) fn new(registry: PathBuf, git: G, snapshot: Arc<Mutex<PodSnapshot>>) -> Self {
        Self {
            registry,
            git,
            tips: Tips::default(),
            snapshot,
            registry_unreadable: false,
            unavailable: HashSet::new(),
        }
    }

    /// Reads the registry and every product once, records the merges and replaces the
    /// snapshot. A product that fails is skipped and logged once until it works again.
    pub(crate) async fn poll(&mut self) {
        let products = match read_registry(&self.registry) {
            Ok(products) => {
                self.registry_unreadable = false;
                products
            }
            Err(error) => {
                if !self.registry_unreadable {
                    self.registry_unreadable = true;
                    warn!(reason = REGISTRY_UNREADABLE, error = %error, "pod facts unavailable");
                }
                *lock(&self.snapshot) = PodSnapshot::default();
                return;
            }
        };
        let mut snapshot = PodSnapshot::default();
        for product in &products {
            match self.poll_product(product).await {
                Ok(counts) => {
                    self.unavailable.remove(&product.name);
                    snapshot.slots.insert(product.name.clone(), counts);
                }
                Err(reason) => {
                    if self.unavailable.insert(product.name.clone()) {
                        warn!(pod.product = %product.name, reason, "pod facts unavailable");
                    }
                }
            }
        }
        *lock(&self.snapshot) = snapshot;
    }

    async fn poll_product(&mut self, product: &Product) -> Result<SlotCounts, &'static str> {
        let slots = slots(&self.git, product).await?;
        if let TipMove::Merges(merges) = self.tips.observe(&self.git, product).await? {
            record_merges(&product.name, &merges);
        }
        Ok(SlotCounts {
            busy: slots.busy,
            free: slots.free,
        })
    }
}

// ---------------------------------------------------------------------------
// The task
// ---------------------------------------------------------------------------

struct Gauges {
    snapshot: Arc<Mutex<PodSnapshot>>,
    _busy: ObservableGauge<u64>,
    _free: ObservableGauge<u64>,
}

static GAUGES: OnceLock<Gauges> = OnceLock::new();

/// The snapshot the `pod.slots.busy` and `pod.slots.free` gauges read. The first call
/// registers the gauges.
pub(crate) fn gauge_snapshot() -> Arc<Mutex<PodSnapshot>> {
    let gauges = GAUGES.get_or_init(|| {
        let snapshot = Arc::new(Mutex::new(PodSnapshot::default()));
        let meter = telemetry::meter();
        let gauge = |name: &'static str, count: fn(&SlotCounts) -> u32| {
            let snapshot = Arc::clone(&snapshot);
            meter
                .u64_observable_gauge(name)
                .with_unit("{slot}")
                .with_callback(move |observer| {
                    for (product, counts) in &lock(&snapshot).slots {
                        observer.observe(
                            u64::from(count(counts)),
                            &[KeyValue::new("pod.product", product.clone())],
                        );
                    }
                })
                .build()
        };
        Gauges {
            _busy: gauge("pod.slots.busy", |counts| counts.busy),
            _free: gauge("pod.slots.free", |counts| counts.free),
            snapshot: Arc::clone(&snapshot),
        }
    });
    Arc::clone(&gauges.snapshot)
}

/// Polls now and then every minute until shutdown. Shutdown also ends a poll under way:
/// its commands only read, and dropping them kills their `git`.
async fn run<G: Git>(mut stats: PodStats<G>, mut shutdown: watch::Receiver<()>) {
    let mut ticker = tokio::time::interval(POLL_INTERVAL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => break,
            _ = ticker.tick() => {}
        }
        tokio::select! {
            biased;
            _ = shutdown.changed() => break,
            () = stats.poll() => {}
        }
    }
}

/// Starts reading the pods of the registry at `registry`.
pub(crate) fn start(registry: PathBuf, shutdown: watch::Receiver<()>) -> JoinHandle<()> {
    let stats = PodStats::new(registry, SystemGit, gauge_snapshot());
    tokio::spawn(run(stats, shutdown))
}

#[cfg(test)]
#[path = "pod_stats_tests.rs"]
mod tests;
