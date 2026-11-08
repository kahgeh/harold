//! Pod facts: how many of each product's build slots are busy and free, what merges to
//! each product's base branch, each product's specs by state, with their size, effort and
//! the time of each step, and the pods and agents that run. Every minute Harold reads the
//! chief's `products.toml`, runs read-only `git` against each product's base checkout,
//! reads its pod folder and takes the agent list. It writes one file, the counted tips
//! beside Harold's store, and never writes in the pod folder.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use config::{Config, File, FileFormat};
use opentelemetry::KeyValue;
use opentelemetry::metrics::ObservableGauge;
use serde::Deserialize;
use time::{Date, Month, OffsetDateTime, Time, UtcOffset};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use tracing::{info, warn};

use crate::agent::domain::EffectiveAgentState;
use crate::agent::snapshot::AgentSnapshotHub;
use crate::outbound::left_out::same_folder;
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
pub(crate) const AGENT_PANE: &str = ".superpowers/baton/agent-pane";

/// The pod folder of a base checkout, which holds the product's specs.
pub(crate) const POD_FOLDER: &str = ".superpowers/pod";

/// Why a product's facts, or the registry, could not be read. Each is fixed text: the
/// `reason` field is exported.
pub(crate) const NO_ANSWER: &str = "git gave no answer";
const GIT_FAILED: &str = "git failed";
const REGISTRY_UNREADABLE: &str = "registry unreadable";
const POD_UNREADABLE: &str = "pod folder unreadable";
const TIPS_UNWRITABLE: &str = "tip file unwritable";

/// How long Harold keeps asking whether a spec worktree that went had its commit merged
/// to the base branch, when git cannot yet say so.
const GONE_RETRY: time::Duration = time::Duration::hours(1);

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
    /// The registry marks the product with `paused = true`.
    pub paused: bool,
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
    #[serde(default)]
    paused: bool,
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
            paused: row.paused,
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
pub(crate) struct SystemGit {
    /// How long one command may take.
    limit: Duration,
}

impl SystemGit {
    pub(crate) fn with_limit(limit: Duration) -> Self {
        Self { limit }
    }
}

impl Default for SystemGit {
    fn default() -> Self {
        Self::with_limit(GIT_TIMEOUT)
    }
}

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
        with_timeout(self.limit, async move {
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
    /// The busy worktrees, in `git worktree list` order.
    pub busy_worktrees: Vec<BusyWorktree>,
}

/// A worktree holding the agent-pane file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BusyWorktree {
    /// The folder name, which is the spec id.
    pub name: String,
    pub path: PathBuf,
    /// The commit checked out there (the `HEAD <sha>` line), when listed.
    pub head: Option<String>,
}

/// The product's busy and free slots. A busy slot is a worktree, other than the base
/// checkout, that holds the agent-pane file. Free slots never go below 0.
pub(crate) async fn slots(git: &impl Git, product: &Product) -> Result<Slots, &'static str> {
    let listing = git_stdout(git, &product.repo, &["worktree", "list", "--porcelain"]).await?;
    let base = canonical(&product.repo).await;
    let mut busy_worktrees = Vec::new();
    for (path, head) in worktrees(&listing) {
        if canonical(&path).await == base || !is_file(&path.join(AGENT_PANE)).await {
            continue;
        }
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        busy_worktrees.push(BusyWorktree { name, path, head });
    }
    let busy = u32::try_from(busy_worktrees.len()).unwrap_or(u32::MAX);
    Ok(Slots {
        busy,
        free: product.slots.saturating_sub(busy),
        busy_worktrees,
    })
}

/// The (path, HEAD commit) of each worktree in `git worktree list --porcelain` output.
fn worktrees(listing: &str) -> Vec<(PathBuf, Option<String>)> {
    let mut worktrees: Vec<(PathBuf, Option<String>)> = Vec::new();
    for line in listing.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            worktrees.push((PathBuf::from(path), None));
        } else if let (Some(head), Some((_, current))) =
            (line.strip_prefix("HEAD "), worktrees.last_mut())
        {
            *current = Some(head.trim().to_owned());
        }
    }
    worktrees
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
    /// `<name>` when the reflog subject is `merge <name>: ...`.
    pub name: Option<String>,
    /// The index in `History::reflog` of the merge's entry. None for the one merge, with
    /// no name, counted when the reflog does not hold both tips.
    pub entry: Option<usize>,
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
    /// When the branch moved, in seconds since the epoch: the reflog entry's own time.
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
        &[
            "reflog",
            "show",
            "--date=unix",
            "--format=%H%x09%gd%x09%gs",
            &branch,
        ],
    )
    .await?;
    Ok(text
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(3, '\t');
            let hash = fields.next()?.to_owned();
            let time = reflog_time(fields.next()?)?;
            let subject = fields.next().unwrap_or_default().to_owned();
            Some(ReflogEntry {
                hash,
                time,
                subject,
            })
        })
        .collect())
}

/// The time in a reflog selector printed with `--date=unix`, such as the
/// `1791989197` of `main@{1791989197}`.
pub(crate) fn reflog_time(selector: &str) -> Option<i64> {
    let (_, braced) = selector.rsplit_once("@{")?;
    braced.strip_suffix('}')?.parse().ok()
}

/// The base branch's tip and its reflog, read once a poll and shared by the merges and
/// the specs.
pub(crate) struct History {
    pub tip: String,
    /// Newest first. Read after the tip, so it holds the tip.
    pub reflog: Vec<ReflogEntry>,
}

pub(crate) async fn read_history(
    git: &impl Git,
    product: &Product,
) -> Result<History, &'static str> {
    let branch = format!("refs/heads/{}", product.base);
    let tip = git_stdout(git, &product.repo, &["rev-parse", "--verify", &branch])
        .await?
        .trim()
        .to_owned();
    let reflog = read_reflog(git, product).await?;
    Ok(History { tip, reflog })
}

/// `<name>` when `subject` is `merge <name>: ...`.
pub(crate) fn merge_name(subject: &str) -> Option<String> {
    let (id, _) = subject.strip_prefix("merge ")?.split_once(':')?;
    let id = id.trim();
    (!id.is_empty()).then(|| id.to_owned())
}

/// The spec a merge counts under, and the stage of that spec it merged, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SpecRef {
    pub id: String,
    /// The `1b` of a merge named `<id>-1b`.
    pub stage: Option<String>,
}

/// Whether `text` is a stage: digits and then at most one lowercase letter, such as `2`
/// or `1b`.
fn is_stage(text: &str) -> bool {
    let digits = text.trim_end_matches(|c: char| c.is_ascii_lowercase());
    let letters = text.len() - digits.len();
    letters <= 1 && !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
}

/// The spec a merge named `name` counts under. A name that is a spec's id is that spec; a
/// name `<id>-<stage>` where `<id>` is a spec is that spec's stage; any other name
/// stands for itself.
pub(crate) fn spec_ref(name: &str, is_spec: impl Fn(&str) -> bool) -> SpecRef {
    if !is_spec(name)
        && let Some((id, stage)) = name.rsplit_once('-')
        && is_stage(stage)
        && is_spec(id)
    {
        return SpecRef {
            id: id.to_owned(),
            stage: Some(stage.to_owned()),
        };
    }
    SpecRef {
        id: name.to_owned(),
        stage: None,
    }
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

/// The tip file beside the store at `store_path`: `<bundle>/data/pod/tips`.
pub(crate) fn tips_file(store_path: &Path) -> PathBuf {
    store_path
        .parent()
        .unwrap_or(Path::new("."))
        .join("pod")
        .join("tips")
}

/// The name of the notices file in Harold's data folder.
const NOTICES_FILE: &str = "pod-notices.json";

/// The pod notices: the keys of the pod-file faults Harold has warned about, each with
/// the time of its warning, kept in `pod-notices.json` in Harold's data folder so a
/// restart does not warn again. The file is a JSON object of key to RFC 3339 time.
pub(crate) struct PodNotices {
    file: PathBuf,
    warned: BTreeMap<String, String>,
    /// Whether `warned` differs from the file.
    dirty: bool,
    /// Whether a failed save has been logged since the last save that worked.
    save_failed: bool,
}

impl PodNotices {
    /// The notices in `file`. A missing file gives none. A file that cannot be read or
    /// is not a JSON object of strings gives none, logs one `pod notices unreadable` line
    /// and is replaced at the next save.
    pub(crate) fn load(file: PathBuf) -> Self {
        let (warned, dirty) = match read_notices(&file) {
            Ok(warned) => (warned, false),
            Err(reason) => {
                warn!(reason, "pod notices unreadable");
                (BTreeMap::new(), true)
            }
        };
        Self {
            file,
            warned,
            dirty,
            save_failed: false,
        }
    }

    pub(crate) fn contains(&self, key: &str) -> bool {
        self.warned.contains_key(key)
    }

    /// Records `key` as warned about at `at`.
    pub(crate) fn insert(&mut self, key: String, at: OffsetDateTime) {
        self.warned.insert(key, rfc3339(at));
        self.dirty = true;
    }

    /// Removes the keys of `product` that are not in `keep`. The keys of other products
    /// stay.
    pub(crate) fn retain_product(&mut self, product: &str, keep: &HashSet<String>) {
        let prefix = format!("{product}\0");
        let before = self.warned.len();
        self.warned
            .retain(|key, _| !key.starts_with(&prefix) || keep.contains(key));
        if self.warned.len() != before {
            self.dirty = true;
        }
    }

    /// Writes the notices to the file when they differ from it: a temporary file with
    /// mode 0600, then a rename. A failure logs one `pod notices not saved` line, with
    /// `reason` `permission` or `io`, until a save works. The next call tries again.
    pub(crate) fn save(&mut self) {
        if !self.dirty {
            return;
        }
        match write_notices(&self.file, &self.warned) {
            Ok(()) => {
                self.dirty = false;
                self.save_failed = false;
            }
            Err(error) => {
                if !self.save_failed {
                    self.save_failed = true;
                    let reason = if error.kind() == std::io::ErrorKind::PermissionDenied {
                        "permission"
                    } else {
                        "io"
                    };
                    warn!(reason, error = %error, "pod notices not saved");
                }
            }
        }
    }
}

/// The notices in `file`, or the reason it cannot be used. A missing file holds none.
fn read_notices(file: &Path) -> Result<BTreeMap<String, String>, &'static str> {
    let text = match std::fs::read_to_string(file) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(BTreeMap::new());
        }
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            return Err("permission");
        }
        Err(error) if error.kind() == std::io::ErrorKind::InvalidData => return Err("parse"),
        Err(_) => return Err("io"),
    };
    serde_json::from_str(&text).map_err(|_| "parse")
}

fn write_notices(file: &Path, warned: &BTreeMap<String, String>) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let bytes = serde_json::to_vec_pretty(warned)?;
    let temp = file.with_extension("json.tmp");
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temp)?;
    // A temporary file left by an earlier run keeps its mode, so the mode is set again.
    out.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    out.write_all(&bytes)?;
    out.sync_all()?;
    std::fs::rename(&temp, file)
}

/// The tip of each product's base branch at its last counted poll, kept in the tip file
/// as one `<product> <hash>` line each so a start can count the merges made while Harold
/// was stopped.
pub(crate) struct Tips {
    file: PathBuf,
    counted: BTreeMap<String, String>,
    /// The products whose tip is as the file had it at the start and has not been saved
    /// since.
    from_file: HashSet<String>,
}

impl Tips {
    /// The tips saved in `file`. A line that does not read as `<product> <hash>` is
    /// skipped, and a file that is missing or unreadable gives no tips.
    pub(crate) fn load(file: PathBuf) -> Self {
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        let counted: BTreeMap<String, String> = text
            .lines()
            .filter_map(|line| line.rsplit_once(' '))
            .filter(|(product, tip)| !product.is_empty() && !tip.is_empty())
            .map(|(product, tip)| (product.to_owned(), tip.to_owned()))
            .collect();
        let from_file = counted.keys().cloned().collect();
        Self {
            file,
            counted,
            from_file,
        }
    }

    /// The tip counted for `product`, if any.
    pub(crate) fn counted_tip(&self, product: &str) -> Option<&str> {
        self.counted.get(product).map(String::as_str)
    }

    /// What the move from the counted tip to the current one holds, changing nothing: a
    /// product with no counted tip has none to count from. A tip from the file that the
    /// reflog does not hold before the current tip is not counted from, as the branch was
    /// rewritten or its reflog lost while Harold was stopped. When reading fails, the
    /// counted tip stays, so the next poll counts the whole move.
    pub(crate) async fn moved(
        &self,
        git: &impl Git,
        product: &Product,
        history: &History,
    ) -> Result<TipMove, &'static str> {
        let Some(old) = self.counted.get(&product.name) else {
            return Ok(TipMove::Unchanged);
        };
        let tip = &history.tip;
        if old == tip {
            return Ok(TipMove::Unchanged);
        }
        if self.from_file.contains(&product.name) {
            let reflog = &history.reflog;
            let held = reflog
                .iter()
                .position(|entry| entry.hash == *tip)
                .is_some_and(|newest| reflog[newest..].iter().any(|entry| entry.hash == *old));
            if !held {
                return Ok(TipMove::Unchanged);
            }
        }
        let (merges, rewritten) = merges(git, product, old, tip, &history.reflog).await?;
        Ok(if merges.is_empty() && rewritten {
            TipMove::Rewritten
        } else {
            TipMove::Merges(merges)
        })
    }

    /// Counts `tip` as the product's, writing the whole tip file first when it differs
    /// from the one stored. A failed write changes nothing.
    pub(crate) fn save(&mut self, product: &str, tip: &str) -> Result<(), &'static str> {
        if self.counted.get(product).map(String::as_str) != Some(tip) {
            let mut counted = self.counted.clone();
            counted.insert(product.to_owned(), tip.to_owned());
            write_tips(&self.file, &counted).map_err(|_| TIPS_UNWRITABLE)?;
            self.counted = counted;
        }
        self.from_file.remove(product);
        Ok(())
    }
}

/// Writes `counted` to `file` through `tips.tmp` beside it, so a reader never sees half a
/// file. Only the user can read it.
fn write_tips(file: &Path, counted: &BTreeMap<String, String>) -> std::io::Result<()> {
    if let Some(folder) = file.parent() {
        std::fs::create_dir_all(folder)?;
    }
    let text: String = counted
        .iter()
        .map(|(product, tip)| format!("{product} {tip}\n"))
        .collect();
    let temp = file.with_extension("tmp");
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temp)?;
    out.write_all(text.as_bytes())?;
    out.sync_all()?;
    std::fs::rename(&temp, file)
}

/// The merges from `old` to `new`: one for each reflog entry from just after `old` up to
/// `new`, each with its name and entry, or one from `old` to `new` with neither when the
/// reflog does not hold both.
/// Each step counts only when its new commit contains its old one; a step that does not,
/// such as a rewrite, is logged and skipped. Also says whether any step was skipped.
async fn merges(
    git: &impl Git,
    product: &Product,
    old: &str,
    new: &str,
    reflog: &[ReflogEntry],
) -> Result<(Vec<Merge>, bool), &'static str> {
    let newest = reflog.iter().position(|entry| entry.hash == new);
    let oldest = newest.and_then(|newest| {
        let after = reflog[newest..]
            .iter()
            .position(|entry| entry.hash == old)?;
        Some(newest + after)
    });
    let steps: Vec<(&str, Option<String>, Option<usize>)> = match (newest, oldest) {
        (Some(newest), Some(oldest)) if newest < oldest => reflog[newest..oldest]
            .iter()
            .enumerate()
            .rev()
            .map(|(offset, entry)| {
                (
                    entry.hash.as_str(),
                    merge_name(&entry.subject),
                    Some(newest + offset),
                )
            })
            .collect(),
        _ => vec![(new, None, None)],
    };
    let mut merges = Vec::new();
    let mut rewritten = false;
    let mut from = old;
    for (to, name, entry) in steps {
        if to == from {
            continue;
        }
        let contains_from = git
            .run(&product.repo, &["merge-base", "--is-ancestor", from, to])
            .await
            .ok_or(NO_ANSWER)?
            .success;
        if !contains_from {
            warn!(pod.product = %product.name, "base branch rewritten; not counted");
            rewritten = true;
            from = to;
            continue;
        }
        let shortstat = git_stdout(git, &product.repo, &["diff", "--shortstat", from, to]).await?;
        let (added, removed) = shortstat_lines(&shortstat);
        merges.push(Merge {
            name,
            entry,
            added,
            removed,
        });
        from = to;
    }
    Ok((merges, rewritten))
}

/// Counts one merge, under its spec and stage when it has them.
fn record_merge(product: &str, merge: &Merge, spec: Option<&SpecRef>) {
    let instruments = telemetry::instruments();
    let mut attributes = vec![KeyValue::new("pod.product", product.to_owned())];
    if let Some(spec) = spec {
        attributes.push(KeyValue::new("spec.id", spec.id.clone()));
        if let Some(stage) = &spec.stage {
            attributes.push(KeyValue::new("spec.stage", stage.clone()));
        }
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

// ---------------------------------------------------------------------------
// Specs
// ---------------------------------------------------------------------------

/// How long a merge without a merged file waits before Harold treats it as the end of the
/// spec, and a stage merge waits before Harold logs that its next stage is late.
const MERGE_WAIT: time::Duration = time::Duration::days(7);

/// How long a spec that completed or was withdrawn stays on the effort gauge: the longest
/// window of the dashboard cards.
const EFFORT_WINDOW: time::Duration = time::Duration::days(30);

/// The state of one spec. A later state wins: a completed spec is completed even though its
/// approval file remains, and a withdrawn spec is withdrawn even if it merged into another.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum SpecState {
    #[default]
    Open,
    Approved,
    InProgress,
    Completed,
    Withdrawn,
}

impl SpecState {
    /// Every state, earliest first.
    pub(crate) const ALL: [SpecState; 5] = [
        SpecState::Open,
        SpecState::Approved,
        SpecState::InProgress,
        SpecState::Completed,
        SpecState::Withdrawn,
    ];

    /// The `state` attribute's value.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            SpecState::Open => "open",
            SpecState::Approved => "approved",
            SpecState::InProgress => "in_progress",
            SpecState::Completed => "completed",
            SpecState::Withdrawn => "withdrawn",
        }
    }
}

/// How far a withdrawn spec got, from the files that exist.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reached {
    Open,
    Approved,
    InProgress,
}

impl Reached {
    /// The `withdrawn.reached` attribute's value.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Reached::Open => "open",
            Reached::Approved => "approved",
            Reached::InProgress => "in_progress",
        }
    }
}

/// Why a spec was withdrawn, and how far it got.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Withdrawal {
    /// `rejected`, `replaced`, `overtaken` or `merged_into`.
    pub reason: &'static str,
    pub reached: Reached,
}

/// What the pod folder and the base branch say about one spec.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SpecFacts {
    pub id: String,
    pub state: SpecState,
    /// When the spec entered `state`.
    pub state_at: OffsetDateTime,
    /// The creation time of `specs/<id>.md`: when the spec became open.
    pub open_at: Option<OffsetDateTime>,
    /// The creation time of `approvals/<id>.md`.
    pub approved_at: Option<OffsetDateTime>,
    /// `started:` of `started/<id>.md`. None when the spec has no readable started file.
    pub started_at: Option<OffsetDateTime>,
    /// When the spec completed, from its merged file or the older sources.
    pub merged_at: Option<OffsetDateTime>,
    /// `commit:` of `merged/<id>.md`. None when the spec has no readable merged file.
    pub merged_commit: Option<String>,
    /// Set for a withdrawn spec.
    pub withdrawn: Option<Withdrawal>,
    /// The words of `specs/<id>.md`, leaving out drawn lines.
    pub words: u64,
    /// The entries of `specs/<id>.revisions.md` that are not the first draft.
    pub revisions: u32,
    /// The revisions tagged `direction`.
    pub direction_revisions: u32,
    /// The entries tagged `architect`.
    pub architect_rounds: u32,
    /// The seconds a build worker was on the spec: from `started:` (or the agent-pane
    /// file's time) to the end of the spec, or to the poll.
    pub build_worker_seconds: i64,
    /// The seconds of build work after a `direction` or `defect` revision: from the
    /// earliest one dated after `started_at` to the end of the spec, or to the poll.
    pub redo_seconds: i64,
    /// The revisions, with any tag but `first`, dated after the approval.
    pub revised_after_approval: u32,
    /// The size from the `**Size.**` line of the spec.
    pub size: SpecSize,
    /// The kind from the `**Kind.**` line of the spec.
    pub kind: SpecKind,
    /// The `confirmed:` time of `confirmed/<id>.md`, which the evidence check writes when
    /// the evidence of a fix is present.
    pub confirmed_at: Option<OffsetDateTime>,
    /// From `evidence/<id>.md`: `Some(true)` when its state is `missing`, `Some(false)` for
    /// `waiting` and `present`. None when the spec has no readable evidence file.
    pub evidence_missing: Option<bool>,
    /// True when the first line of the spec is `Waiting for your approval.`.
    pub awaiting: bool,
    /// False when `approvals/<id>.md` names (`spec-sha256:`) another text than the spec
    /// has now. True when it names the current text or has no such line.
    pub approval_current: bool,
    /// The lines of `reviews/<id>.md`. None when the spec has no readable reviews file.
    pub reviews: Option<Vec<ReviewLine>>,
    /// The `ready:` time of `ready/<id>.md`, written once when the build is done and waits
    /// for its merge. None when the spec has no readable ready file.
    pub ready_at: Option<OffsetDateTime>,
}

impl Default for SpecFacts {
    /// An open spec with no id, no files and no effort, entered at the epoch. A test sets
    /// the fields it reads.
    fn default() -> Self {
        Self {
            id: String::new(),
            state: SpecState::Open,
            state_at: OffsetDateTime::UNIX_EPOCH,
            open_at: None,
            approved_at: None,
            started_at: None,
            merged_at: None,
            merged_commit: None,
            withdrawn: None,
            words: 0,
            revisions: 0,
            direction_revisions: 0,
            architect_rounds: 0,
            build_worker_seconds: 0,
            redo_seconds: 0,
            revised_after_approval: 0,
            size: SpecSize::Unsized,
            kind: SpecKind::Unkinded,
            confirmed_at: None,
            evidence_missing: None,
            awaiting: false,
            approval_current: true,
            reviews: None,
            ready_at: None,
        }
    }
}

/// The size a spec states on its `**Size.**` line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SpecSize {
    Small,
    Medium,
    Large,
    /// The spec has no size line, or the word is not a size.
    Unsized,
}

impl SpecSize {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Small => "small",
            Self::Medium => "medium",
            Self::Large => "large",
            Self::Unsized => "unsized",
        }
    }
}

/// The kind a spec states on its `**Kind.**` line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SpecKind {
    /// A product feature, which counts its uses.
    Feature,
    /// A bug fix, which a confirmed file confirms.
    Fix,
    /// Work that gives no use count.
    Other,
    /// The spec has no kind line, or the word is not a kind.
    Unkinded,
}

impl SpecKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Feature => "feature",
            Self::Fix => "fix",
            Self::Other => "other",
            Self::Unkinded => "unkinded",
        }
    }
}

/// The kind from the first line that starts with `**Kind.**`. The word is compared in
/// lower case. Text after the word is ignored.
pub(crate) fn spec_kind(markdown: &str) -> SpecKind {
    let Some(rest) = markdown
        .lines()
        .find_map(|line| line.strip_prefix("**Kind.**"))
    else {
        return SpecKind::Unkinded;
    };
    let word = rest
        .split(|c: char| !c.is_alphanumeric())
        .find(|part| !part.is_empty())
        .unwrap_or_default()
        .to_lowercase();
    match word.as_str() {
        "feature" => SpecKind::Feature,
        "fix" => SpecKind::Fix,
        "other" => SpecKind::Other,
        _ => SpecKind::Unkinded,
    }
}

/// The size from the first line that starts with `**Size.**`. The word is compared in
/// lower case. Text after the word is ignored.
pub(crate) fn spec_size(markdown: &str) -> SpecSize {
    let Some(rest) = markdown
        .lines()
        .find_map(|line| line.strip_prefix("**Size.**"))
    else {
        return SpecSize::Unsized;
    };
    let word = rest
        .split(|c: char| !c.is_alphanumeric())
        .find(|part| !part.is_empty())
        .unwrap_or_default()
        .to_lowercase();
    match word.as_str() {
        "small" => SpecSize::Small,
        "medium" => SpecSize::Medium,
        "large" => SpecSize::Large,
        _ => SpecSize::Unsized,
    }
}

/// True when the first line of the spec, trimmed, is `Waiting for your approval.`. One
/// leading byte-order mark (U+FEFF) is not part of the line.
pub(crate) fn spec_awaiting(markdown: &str) -> bool {
    let markdown = markdown.strip_prefix('\u{feff}').unwrap_or(markdown);
    markdown
        .lines()
        .next()
        .is_some_and(|line| line.trim() == "Waiting for your approval.")
}

/// True when the approval has no `spec-sha256:` line, or when that line (trimmed, in lower
/// case) equals the sha-256 of the spec text.
pub(crate) fn approval_is_current(approval: Option<&str>, spec_text: Option<&str>) -> bool {
    let Some(named) = approval.and_then(|text| {
        text.lines()
            .find_map(|line| line.trim().strip_prefix("spec-sha256:"))
    }) else {
        return true;
    };
    let Some(spec_text) = spec_text else {
        return true;
    };
    let current = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(spec_text.as_bytes()));
    named.trim().to_lowercase() == current
}

/// A fact about a spec that Harold logs once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Notice {
    /// A pod file that does not parse. `file` is its name inside the pod folder and
    /// `content` is what it holds, so a changed file is logged again.
    Unreadable {
        id: String,
        file: String,
        content: String,
    },
    /// A spec with a started file whose last merge has no stage, is over 7 days old and
    /// has no merged file. Harold completed it.
    CompletedWithoutFile {
        id: String,
        merged_at: OffsetDateTime,
        commit: Option<String>,
    },
    /// A spec with a started file whose last stage merged over 7 days ago and has no
    /// merged file.
    StageMergedWithoutFile { id: String, stage: String },
}

/// What the specs of one pod folder hold, and what to log about them.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct SpecRead {
    pub specs: Vec<SpecFacts>,
    pub notices: Vec<Notice>,
}

/// The creation time of the file at `path`, or its modification time where the file
/// system keeps no creation time. None when there is no such file.
pub(crate) fn file_time(path: &Path, offset: UtcOffset) -> Option<OffsetDateTime> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    let time = metadata.created().or_else(|_| metadata.modified()).ok()?;
    Some(OffsetDateTime::from(time).to_offset(offset))
}

/// The draft time of a spec: the open time. It is the time of the first `## ` heading of
/// the revisions file, in `offset`, when that heading has a clock time. Else it is the
/// birth time of the spec file. It never takes a later heading. An earlier birth time wins.
pub(crate) fn draft_time(
    revisions: Option<&str>,
    birth: Option<OffsetDateTime>,
    offset: UtcOffset,
) -> Option<OffsetDateTime> {
    let first = revisions
        .and_then(|text| text.lines().find_map(|line| line.strip_prefix("## ")))
        .filter(|heading| {
            heading
                .split(',')
                .next()
                .is_some_and(|stamp| stamp.split_whitespace().count() == 2)
        })
        .and_then(|heading| heading_time(heading, offset));
    match (first, birth) {
        (Some(first), Some(birth)) => Some(first.min(birth)),
        (first, birth) => first.or(birth),
    }
}

/// The text of the file at `path`, or None when it cannot be read.
fn read_text(path: &Path) -> Option<String> {
    std::fs::read(path)
        .ok()
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

/// What a pod file holds: nothing when it is absent, its facts when it parses, or `Bad`
/// when it exists and does not.
enum PodFile<T> {
    Absent,
    Read(T),
    Bad(Notice),
}

impl<T> PodFile<T> {
    /// Splits the file into its facts and the notice about it, if any.
    fn split(self) -> (Option<T>, Option<Notice>) {
        match self {
            PodFile::Read(value) => (Some(value), None),
            PodFile::Bad(notice) => (None, Some(notice)),
            PodFile::Absent => (None, None),
        }
    }
}

/// The `key: value` lines of a pod file. The first line of a key wins and a line without a
/// colon, such as the title, is skipped.
pub(crate) fn key_values(text: &str) -> HashMap<&str, &str> {
    let mut pairs = HashMap::new();
    for line in text.lines() {
        if let Some((key, value)) = line.split_once(':') {
            pairs.entry(key.trim()).or_insert_with(|| value.trim());
        }
    }
    pairs
}

pub(crate) fn parse_time(text: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339).ok()
}

/// The file `<folder>/<id>.md` of the pod folder `pod`, parsed by `parse`.
fn read_pod_file<T>(
    pod: &Path,
    folder: &str,
    id: &str,
    parse: impl Fn(&HashMap<&str, &str>) -> Option<T>,
) -> PodFile<T> {
    read_pod_text(pod, folder, id, |text| parse(&key_values(text)))
}

/// Like `read_pod_file`, for a file whose keys repeat, so `parse` reads the whole text.
fn read_pod_text<T>(
    pod: &Path,
    folder: &str,
    id: &str,
    parse: impl Fn(&str) -> Option<T>,
) -> PodFile<T> {
    let name = format!("{folder}/{id}.md");
    let path = pod.join(&name);
    if !path.exists() {
        return PodFile::Absent;
    }
    let bad = |content: String| {
        PodFile::Bad(Notice::Unreadable {
            id: id.to_owned(),
            file: name.clone(),
            content,
        })
    };
    let Some(text) = read_text(&path) else {
        return bad(String::new());
    };
    match parse(&text) {
        Some(value) => PodFile::Read(value),
        None => bad(text),
    }
}

/// One line of `reviews/<id>.md`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReviewLine {
    /// `sent:` the SPEC line went to the architect.
    Sent(OffsetDateTime),
    /// `verdict:` the answer came back: true for `ok`, false for `revise`.
    Verdict(OffsetDateTime, bool),
    /// `skipped:` the chief approved the spec with no architect review.
    Skipped(OffsetDateTime),
}

impl ReviewLine {
    pub(crate) fn at(self) -> OffsetDateTime {
        match self {
            Self::Sent(at) | Self::Verdict(at, _) | Self::Skipped(at) => at,
        }
    }
}

/// Reads `reviews/<id>.md`: an optional `# ` title, then one line each of
/// `sent: <time> sha256:<hex>`, `verdict: <time> ok|revise` and `skipped: <time>`. A blank
/// line is skipped. None when a line does not parse, when a `verdict:` does not follow a
/// `sent:`, when a `sent:` follows a `sent:` with no verdict, or when the file has no line.
/// A `skipped:` after an open `sent:` closes that review, as a `verdict:` does.
pub(crate) fn parse_reviews(text: &str) -> Option<Vec<ReviewLine>> {
    let mut lines = Vec::new();
    let mut in_review = false;
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, rest) = line.split_once(':')?;
        let mut words = rest.split_whitespace();
        let at = parse_time(words.next()?)?;
        let parsed = match (key, in_review) {
            ("sent", false) => {
                words.next()?.strip_prefix("sha256:")?;
                in_review = true;
                ReviewLine::Sent(at)
            }
            ("verdict", true) => {
                let ok = match words.next()? {
                    "ok" => true,
                    "revise" => false,
                    _ => return None,
                };
                in_review = false;
                ReviewLine::Verdict(at, ok)
            }
            ("skipped", _) => {
                in_review = false;
                ReviewLine::Skipped(at)
            }
            _ => return None,
        };
        lines.push(parsed);
    }
    (!lines.is_empty()).then_some(lines)
}

/// The three waits of a spec with a reviews file, in seconds, as `(phase, seconds)`:
/// `spec_writing`, `architect_review` and `approval_wait`. They add up to the time from
/// `open` to `approved`. None when a wait is negative or the last line is a `sent:`.
pub(crate) fn review_phases(
    open: OffsetDateTime,
    approved: OffsetDateTime,
    lines: &[ReviewLine],
) -> Option<[(&'static str, f64); 3]> {
    let (mut writing, mut review) = (0.0, 0.0);
    let (mut cursor, mut sent_at) = (open, None);
    for line in lines {
        let at = line.at();
        match line {
            ReviewLine::Sent(_) => {
                writing += (at - cursor).as_seconds_f64();
                sent_at = Some(at);
            }
            ReviewLine::Skipped(_) => match sent_at.take() {
                Some(sent) => review += (at - sent).as_seconds_f64(),
                None => writing += (at - cursor).as_seconds_f64(),
            },
            ReviewLine::Verdict(..) => review += (at - sent_at.take()?).as_seconds_f64(),
        }
        cursor = at;
    }
    let waiting = (approved - cursor).as_seconds_f64();
    (sent_at.is_none() && writing >= 0.0 && review >= 0.0 && waiting >= 0.0).then_some([
        ("spec_writing", writing),
        ("architect_review", review),
        ("approval_wait", waiting),
    ])
}

/// The reasons a withdrawn file may give.
const WITHDRAWN_REASONS: [&str; 4] = ["rejected", "replaced", "overtaken", "merged_into"];

struct WithdrawnFile {
    at: OffsetDateTime,
    reason: &'static str,
}

fn parse_withdrawn(pairs: &HashMap<&str, &str>) -> Option<WithdrawnFile> {
    let at = parse_time(pairs.get("withdrawn")?)?;
    let given = pairs.get("reason")?;
    let reason = WITHDRAWN_REASONS
        .into_iter()
        .find(|reason| reason == given)?;
    Some(WithdrawnFile { at, reason })
}

fn parse_confirmed(pairs: &HashMap<&str, &str>) -> Option<OffsetDateTime> {
    parse_time(pairs.get("confirmed")?)
}

fn parse_started(pairs: &HashMap<&str, &str>) -> Option<OffsetDateTime> {
    parse_time(pairs.get("started")?)
}

struct MergedFile {
    at: OffsetDateTime,
    commit: String,
}

/// The `ready:` time of `ready/<id>.md`. The file needs a `commit:` line with a full hash.
fn parse_ready(pairs: &HashMap<&str, &str>) -> Option<OffsetDateTime> {
    let at = parse_time(pairs.get("ready")?)?;
    let commit = pairs.get("commit")?;
    let is_sha = commit.len() >= 40 && commit.bytes().all(|b| b.is_ascii_hexdigit());
    is_sha.then_some(at)
}

fn parse_merged(pairs: &HashMap<&str, &str>) -> Option<MergedFile> {
    let at = parse_time(pairs.get("merged")?)?;
    let commit = pairs.get("commit")?;
    let is_sha = commit.len() >= 40 && commit.bytes().all(|b| b.is_ascii_hexdigit());
    is_sha.then(|| MergedFile {
        at,
        commit: (*commit).to_owned(),
    })
}

/// The `state:` of `evidence/<id>.md` as the missing flag: true for `missing`, false for
/// `waiting` and `present`, none for any other word.
fn parse_evidence_missing(pairs: &HashMap<&str, &str>) -> Option<bool> {
    match *pairs.get("state")? {
        "missing" => Some(true),
        "waiting" | "present" => Some(false),
        _ => None,
    }
}

/// The merges of one spec that the older sources show: the newest merge without a stage
/// and the newest stage merge.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct MergeSeen {
    whole: Option<OffsetDateTime>,
    /// The commit of the reflog entry that gives `whole`. A gone worktree has none.
    whole_commit: Option<String>,
    stage: Option<(OffsetDateTime, String)>,
}

/// The merges the reflog and the gone worktrees show, by spec id. `is_spec` says which
/// names are specs.
fn merges_seen(
    reflog: &[ReflogEntry],
    gone: &HashMap<String, OffsetDateTime>,
    is_spec: impl Fn(&str) -> bool,
) -> HashMap<String, MergeSeen> {
    let mut seen: HashMap<String, MergeSeen> = HashMap::new();
    for entry in reflog {
        let (Some(name), Ok(time)) = (
            merge_name(&entry.subject),
            OffsetDateTime::from_unix_timestamp(entry.time),
        ) else {
            continue;
        };
        let spec = spec_ref(&name, &is_spec);
        let merges = seen.entry(spec.id).or_default();
        match spec.stage {
            None => {
                if merges.whole.is_none_or(|whole| time > whole) {
                    merges.whole = Some(time);
                    merges.whole_commit = Some(entry.hash.clone());
                }
            }
            Some(stage) => {
                if merges
                    .stage
                    .as_ref()
                    .is_none_or(|(newest, _)| time > *newest)
                {
                    merges.stage = Some((time, stage));
                }
            }
        }
    }
    for (id, time) in gone {
        let merges = seen.entry(id.clone()).or_default();
        if merges.whole.is_none_or(|whole| *time > whole) {
            merges.whole = Some(*time);
            merges.whole_commit = None;
        }
    }
    seen
}

/// What the poll knows about the worktrees and the base branch, for the specs.
pub(crate) struct Sources<'a> {
    /// The busy worktrees: folder name -> the creation time of the agent-pane file.
    pub busy: &'a HashMap<String, OffsetDateTime>,
    pub reflog: &'a [ReflogEntry],
    /// The specs seen merged by their worktree going: id -> when.
    pub gone: &'a HashMap<String, OffsetDateTime>,
    pub now: OffsetDateTime,
    pub offset: UtcOffset,
}

/// The specs of the pod folder `pod`, by id. Reads <repo>/.superpowers/pod/ (never
/// writes). A spec is a `specs/<id>.md` file: an id with no spec file is not a spec, and
/// neither is a file with a second dot, such as `<id>.revisions.md`. A pod without a
/// `specs` folder has no specs; one whose `specs` folder cannot be read is an error.
pub(crate) fn read_specs(pod: &Path, sources: &Sources<'_>) -> Result<SpecRead, &'static str> {
    let entries = match std::fs::read_dir(pod.join("specs")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(SpecRead::default());
        }
        Err(_) => return Err(POD_UNREADABLE),
    };
    let mut ids: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension()? != "md" {
                return None;
            }
            let id = path.file_stem()?.to_str()?;
            // Spec ids are kebab-case: `<id>.revisions.md` and the like are not specs.
            if id.contains('.') {
                return None;
            }
            Some(id.to_owned())
        })
        .collect();
    ids.sort();
    let is_spec = |name: &str| ids.iter().any(|id| id == name);
    let merges = merges_seen(sources.reflog, sources.gone, is_spec);
    // A worktree of a stage, named `<id>-<stage>`, is busy for its spec.
    let mut busy: HashMap<String, OffsetDateTime> = HashMap::new();
    for (name, time) in sources.busy {
        let id = spec_ref(name, is_spec).id;
        let slot = busy.entry(id).or_insert(*time);
        *slot = (*slot).min(*time);
    }
    let rejected = read_text(&pod.join("rejections.md"))
        .map(|text| rejections(&text, sources.offset))
        .unwrap_or_default();
    let mut read = SpecRead::default();
    for id in &ids {
        let context = SpecContext {
            pod,
            sources,
            merges: merges.get(id),
            busy: busy.get(id).copied(),
            rejected: rejected.get(id).copied(),
        };
        if let Some((facts, notices)) = spec_facts(id, &context) {
            read.specs.push(facts);
            read.notices.extend(notices);
        }
    }
    Ok(read)
}

struct SpecContext<'a> {
    pod: &'a Path,
    sources: &'a Sources<'a>,
    merges: Option<&'a MergeSeen>,
    /// When the agent-pane file of a busy worktree for the spec was created.
    busy: Option<OffsetDateTime>,
    /// The rejection date, from `rejections.md`.
    rejected: Option<OffsetDateTime>,
}

fn spec_facts(id: &str, context: &SpecContext<'_>) -> Option<(SpecFacts, Vec<Notice>)> {
    let SpecContext {
        pod,
        sources,
        merges,
        busy,
        rejected,
    } = context;
    let (now, offset) = (sources.now, sources.offset);
    let spec = pod.join("specs").join(format!("{id}.md"));
    let birth = file_time(&spec, offset)?;
    let revisions_text = read_text(&pod.join("specs").join(format!("{id}.revisions.md")));
    let open_at = draft_time(revisions_text.as_deref(), Some(birth), offset)?;
    let approved_at = file_time(&pod.join("approvals").join(format!("{id}.md")), offset);
    let (withdrawn_file, withdrawn_notice) =
        read_pod_file(pod, "withdrawn", id, parse_withdrawn).split();
    let (started_file, started_notice) = read_pod_file(pod, "started", id, parse_started).split();
    let (merged_file, merged_notice) = read_pod_file(pod, "merged", id, parse_merged).split();
    let (confirmed_file, confirmed_notice) =
        read_pod_file(pod, "confirmed", id, parse_confirmed).split();
    let (evidence_file, evidence_notice) =
        read_pod_file(pod, "evidence", id, parse_evidence_missing).split();
    let (reviews_file, reviews_notice) = read_pod_text(pod, "reviews", id, parse_reviews).split();
    let (ready_file, ready_notice) = read_pod_file(pod, "ready", id, parse_ready).split();
    let mut notices: Vec<Notice> = [
        reviews_notice,
        ready_notice,
        withdrawn_notice,
        started_notice,
        merged_notice,
        confirmed_notice,
        evidence_notice,
    ]
    .into_iter()
    .flatten()
    .collect();
    let withdrawn = withdrawn_file.map(|file| (file.at, file.reason));
    let started = started_file.map(|at| at.to_offset(offset));
    let merged = merged_file.map(|file| (file.at.to_offset(offset), file.commit));
    let withdrawn = withdrawn
        .map(|(at, reason)| (at.to_offset(offset), reason))
        .or(rejected.map(|at| (at, "rejected")));
    let default_merges = MergeSeen::default();
    let seen = merges.unwrap_or(&default_merges);
    let waiting_worker = busy.is_some();
    let completed = match &merged {
        Some((at, _)) => Some(*at),
        None if started.is_some() => {
            let completed = seen.whole.filter(|whole| {
                now - *whole > MERGE_WAIT
                    && !waiting_worker
                    && seen.stage.as_ref().is_none_or(|(stage, _)| stage <= whole)
            });
            if withdrawn.is_none() {
                if let Some(merged_at) = completed {
                    notices.push(Notice::CompletedWithoutFile {
                        id: id.to_owned(),
                        merged_at,
                        commit: seen.whole_commit.clone(),
                    });
                } else if let Some((stage_at, stage)) = &seen.stage
                    && now - *stage_at > MERGE_WAIT
                    && !waiting_worker
                    && seen.whole.is_none_or(|whole| whole < *stage_at)
                {
                    notices.push(Notice::StageMergedWithoutFile {
                        id: id.to_owned(),
                        stage: stage.clone(),
                    });
                }
            }
            completed.map(|at| at.to_offset(offset))
        }
        None => seen.whole.map(|at| at.to_offset(offset)),
    };
    let in_progress_at = started.or(*busy);
    let (state, state_at) = if let Some((at, _)) = withdrawn {
        (SpecState::Withdrawn, at)
    } else if let Some(at) = completed {
        (SpecState::Completed, at)
    } else if let Some(at) = in_progress_at {
        (SpecState::InProgress, at)
    } else if let Some(at) = approved_at {
        (SpecState::Approved, at)
    } else {
        (SpecState::Open, open_at)
    };
    let reached = if in_progress_at.is_some() {
        Reached::InProgress
    } else if approved_at.is_some() {
        Reached::Approved
    } else {
        Reached::Open
    };
    let worked_to = withdrawn.map(|(at, _)| at).or(completed).unwrap_or(now);
    let build_worker_seconds =
        in_progress_at.map_or(0, |from| (worked_to - from).whole_seconds().max(0));
    let marks = revisions_text
        .as_deref()
        .map(revision_marks)
        .unwrap_or_default();
    let entries = revisions_text
        .as_deref()
        .map(|text| revision_entries(text, offset))
        .unwrap_or_default();
    let spec_text = read_text(&spec);
    let words = spec_text.as_deref().map_or(0, spec_words);
    let size = spec_text.as_deref().map_or(SpecSize::Unsized, spec_size);
    let kind = spec_text.as_deref().map_or(SpecKind::Unkinded, spec_kind);
    let awaiting = spec_text.as_deref().is_some_and(spec_awaiting);
    let approval_text = read_text(&pod.join("approvals").join(format!("{id}.md")));
    let approval_current = approval_is_current(approval_text.as_deref(), spec_text.as_deref());
    let facts = SpecFacts {
        id: id.to_owned(),
        state,
        state_at,
        open_at: Some(open_at),
        approved_at,
        started_at: started,
        merged_at: completed,
        merged_commit: merged.map(|(_, commit)| commit),
        withdrawn: withdrawn.map(|(_, reason)| Withdrawal { reason, reached }),
        words,
        revisions: marks.revisions,
        direction_revisions: marks.direction,
        architect_rounds: marks.architect,
        build_worker_seconds,
        redo_seconds: redo_seconds(&entries, started, worked_to),
        revised_after_approval: revised_after_approval(&entries, approved_at),
        size,
        kind,
        confirmed_at: confirmed_file,
        evidence_missing: evidence_file,
        awaiting,
        approval_current,
        reviews: reviews_file,
        ready_at: ready_file.map(|at| at.to_offset(offset)),
    };
    Some((facts, notices))
}

/// The words of lines not starting with '<', which hold drawn charts.
pub(crate) fn spec_words(markdown: &str) -> u64 {
    let words = markdown
        .lines()
        .filter(|line| !line.trim_start().starts_with('<'))
        .map(|line| line.split_whitespace().count())
        .sum::<usize>();
    u64::try_from(words).unwrap_or(u64::MAX)
}

/// What the `## ` headings of a revisions file count.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RevisionMarks {
    /// The entries that are not the first draft.
    pub revisions: u32,
    /// The revisions tagged `direction`.
    pub direction: u32,
    /// The entries tagged `architect`.
    pub architect: u32,
}

/// Counts the entries of a revisions file. Each `## ` heading is an entry, and the tags
/// are the comma-separated words in its last parenthesis: `first` (the first draft, not a
/// revision), `direction`, `defect` and `architect`. A heading with no tags is a revision
/// of unknown class.
pub(crate) fn revision_marks(text: &str) -> RevisionMarks {
    let mut marks = RevisionMarks::default();
    for heading in text.lines().filter(|line| line.starts_with("## ")) {
        let tags: Vec<&str> = heading
            .rsplit_once('(')
            .and_then(|(_, rest)| rest.split_once(')'))
            .map(|(inside, _)| inside.split(',').map(str::trim).collect())
            .unwrap_or_default();
        let has = |tag: &str| tags.contains(&tag);
        if !has("first") {
            marks.revisions += 1;
        }
        if has("direction") && !has("first") {
            marks.direction += 1;
        }
        if has("architect") {
            marks.architect += 1;
        }
    }
    marks
}

/// One `## ` heading of a revisions file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RevisionEntry {
    /// The time in the heading. A date-only heading gives that date's local midnight.
    /// None when the date or the time does not parse.
    pub at: Option<OffsetDateTime>,
    /// The comma-separated words in the last parenthesis of the heading.
    pub tags: Vec<String>,
}

/// Reads each `## ` heading of a revisions file as one entry. The format is
/// `## YYYY-MM-DD HH:MM, rev N (tags)`. The time is optional. The tags are listed in
/// `docs/references/telemetry/revision-tags.md`.
pub(crate) fn revision_entries(text: &str, offset: UtcOffset) -> Vec<RevisionEntry> {
    text.lines()
        .filter_map(|line| line.strip_prefix("## "))
        .map(|heading| RevisionEntry {
            at: heading_time(heading, offset),
            tags: heading
                .rsplit_once('(')
                .and_then(|(_, rest)| rest.split_once(')'))
                .map(|(inside, _)| inside.split(',').map(|tag| tag.trim().to_owned()).collect())
                .unwrap_or_default(),
        })
        .collect()
}

/// The time at the start of a heading: `YYYY-MM-DD` or `YYYY-MM-DD HH:MM`, then a comma.
fn heading_time(heading: &str, offset: UtcOffset) -> Option<OffsetDateTime> {
    let mut words = heading.split(',').next()?.split_whitespace();
    let date = parse_date(words.next()?)?;
    let time = match words.next() {
        None => Time::MIDNIGHT,
        Some(clock) => {
            let (hour, minute) = clock.split_once(':')?;
            Time::from_hms(hour.parse().ok()?, minute.parse().ok()?, 0).ok()?
        }
    };
    words
        .next()
        .is_none()
        .then(|| date.with_time(time).assume_offset(offset))
}

/// The seconds of redo: from the earliest entry tagged `direction` or `defect` and dated
/// after `started`, to `end`. An entry with no time is never redo. A spec that did not
/// start has no redo.
pub(crate) fn redo_seconds(
    entries: &[RevisionEntry],
    started: Option<OffsetDateTime>,
    end: OffsetDateTime,
) -> i64 {
    let Some(started) = started else {
        return 0;
    };
    entries
        .iter()
        .filter(|entry| {
            entry
                .tags
                .iter()
                .any(|tag| tag == "direction" || tag == "defect")
        })
        .filter_map(|entry| entry.at)
        .filter(|at| *at > started)
        .min()
        .map_or(0, |from| (end - from).whole_seconds().max(0))
}

/// The entries not tagged `first` and dated after the approval. Zero when the spec has
/// no approval.
pub(crate) fn revised_after_approval(
    entries: &[RevisionEntry],
    approved_at: Option<OffsetDateTime>,
) -> u32 {
    let Some(approved) = approved_at else {
        return 0;
    };
    let count = entries
        .iter()
        .filter(|entry| !entry.tags.iter().any(|tag| tag == "first"))
        .filter(|entry| entry.at.is_some_and(|at| at > approved))
        .count();
    u32::try_from(count).unwrap_or(u32::MAX)
}

/// The withdrawn specs of `rejections.md`: each line `- <id>, <YYYY-MM-DD>, <reason>`
/// gives `<id>` -> local midnight of that date. A line without a valid date is skipped.
pub(crate) fn rejections(text: &str, offset: UtcOffset) -> HashMap<String, OffsetDateTime> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.strip_prefix("- ")?.split(',');
            let id = fields.next()?.trim();
            let date = parse_date(fields.next()?.trim())?;
            (!id.is_empty()).then(|| {
                (
                    id.to_owned(),
                    date.with_time(Time::MIDNIGHT).assume_offset(offset),
                )
            })
        })
        .collect()
}

/// The date `YYYY-MM-DD`.
fn parse_date(text: &str) -> Option<Date> {
    let mut parts = text.split('-');
    let year = parts.next()?.parse().ok()?;
    let month = Month::try_from(parts.next()?.parse::<u8>().ok()?).ok()?;
    let day = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Date::from_calendar_date(year, month, day).ok()
}

/// One change of a spec's state seen by `SpecBook`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SpecChange {
    pub id: String,
    /// The state at the poll before. None for a spec that was not there.
    pub before: Option<SpecState>,
    pub state: SpecState,
}

/// The last known state of each product's specs.
#[derive(Default)]
pub(crate) struct SpecBook {
    known: HashMap<(String, String), SpecState>,
    baselined: HashSet<String>,
}

impl SpecBook {
    /// The first call per product records states and reports nothing. Later calls log one
    /// line per change and return the changes.
    pub(crate) fn observe(&mut self, product: &str, specs: &[SpecFacts]) -> Vec<SpecChange> {
        let first = self.baselined.insert(product.to_owned());
        // A spec whose file went is forgotten, so it counts again if it comes back.
        self.known
            .retain(|(known, id), _| known != product || specs.iter().any(|spec| &spec.id == id));
        let mut changes = Vec::new();
        for spec in specs {
            let before = self
                .known
                .insert((product.to_owned(), spec.id.clone()), spec.state);
            if first || before == Some(spec.state) {
                continue;
            }
            info!(
                target: "harold::pod",
                {
                    pod.product = %product,
                    spec.id = %spec.id,
                    state = spec.state.as_str(),
                    spec.state_at = %rfc3339(spec.state_at),
                    spec.words = spec.words,
                    spec.revisions = spec.revisions,
                },
                "spec state changed"
            );
            changes.push(SpecChange {
                id: spec.id.clone(),
                before,
                state: spec.state,
            });
        }
        changes
    }
}

fn rfc3339(time: OffsetDateTime) -> String {
    time.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// Whether the merge at `reflog[entry]` is the oldest in the reflog for spec `id`: no
/// older entry (a higher index) names a merge that counts under `id`.
fn first_merge(
    reflog: &[ReflogEntry],
    entry: usize,
    id: &str,
    is_spec: impl Fn(&str) -> bool,
) -> bool {
    !reflog[entry + 1..].iter().any(|older| {
        merge_name(&older.subject).is_some_and(|name| spec_ref(&name, &is_spec).id == id)
    })
}

/// Records the steps of a spec that completed at `completed_at`: open to approved and
/// approved to started, only when `first` is set, as a spec takes them once however many
/// merges it has, and started to completed. A step missing a time, or with a negative
/// length, is skipped.
fn record_lead_time(
    product: &str,
    spec: &SpecRef,
    facts: &SpecFacts,
    completed_at: OffsetDateTime,
    first: bool,
) {
    // A ready time outside the build gives neither ready phase, so the two always add up.
    let ready_at = facts
        .started_at
        .zip(facts.ready_at)
        .and_then(|(started, ready)| (started <= ready && ready <= completed_at).then_some(ready));
    let phases = [
        ("open_to_approved", first, facts.open_at, facts.approved_at),
        (
            "approved_to_started",
            first,
            facts.approved_at,
            facts.started_at,
        ),
        (
            "started_to_completed",
            true,
            facts.started_at,
            Some(completed_at),
        ),
        // The build splits at the ready time, at the first merge only: the ready file
        // holds the first ready time, so a later merge has no split.
        ("started_to_ready", first, facts.started_at, ready_at),
        ("ready_to_completed", first, ready_at, Some(completed_at)),
    ];
    let mut waits: Vec<(&str, f64)> = phases
        .into_iter()
        .filter_map(|(phase, wanted, from, to)| {
            let (true, Some(from), Some(to)) = (wanted, from, to) else {
                return None;
            };
            Some((phase, (to - from).as_seconds_f64()))
        })
        .collect();
    if let (true, Some(open), Some(approved), Some(lines)) =
        (first, facts.open_at, facts.approved_at, &facts.reviews)
    {
        waits.extend(review_phases(open, approved, lines).into_iter().flatten());
    }
    for (phase, seconds) in waits {
        if seconds < 0.0 {
            continue;
        }
        let mut attributes = vec![
            KeyValue::new("pod.product", product.to_owned()),
            KeyValue::new("phase", phase),
            KeyValue::new("spec.revisions", i64::from(facts.revisions)),
            KeyValue::new("spec.id", spec.id.clone()),
        ];
        if let Some(stage) = &spec.stage {
            attributes.push(KeyValue::new("spec.stage", stage.clone()));
        }
        telemetry::instruments()
            .pod_spec_lead_time
            .record(seconds, &attributes);
    }
}

/// The specs the effort gauge reports at `now`: those not completed or withdrawn, and
/// those that entered one of those states in the last 30 days.
pub(crate) fn effort_specs(specs: &[SpecFacts], now: OffsetDateTime) -> Vec<&SpecFacts> {
    specs
        .iter()
        .filter(|spec| {
            !matches!(spec.state, SpecState::Completed | SpecState::Withdrawn)
                || now - spec.state_at <= EFFORT_WINDOW
        })
        .collect()
}

/// The `measure` and value pairs of one spec on the effort gauge.
pub(crate) fn effort_measures(spec: &SpecFacts) -> [(&'static str, f64); 4] {
    [
        ("revisions", f64::from(spec.revisions)),
        ("direction_revisions", f64::from(spec.direction_revisions)),
        ("architect_rounds", f64::from(spec.architect_rounds)),
        (
            "build_worker_hours",
            spec.build_worker_seconds as f64 / 3600.0,
        ),
    ]
}

/// The `measure` and value pairs of one spec that follow `effort_measures` on the
/// effort gauge: the redo time in hours and the revisions after the approval.
pub(crate) fn extra_effort_measures(spec: &SpecFacts) -> [(&'static str, f64); 2] {
    [
        ("redo_hours", spec.redo_seconds as f64 / 3600.0),
        (
            "revised_after_approval",
            f64::from(spec.revised_after_approval),
        ),
    ]
}

/// The earliest time of the files in `merged/`.
pub(crate) fn first_merged_file(pod: &Path, offset: UtcOffset) -> Option<OffsetDateTime> {
    std::fs::read_dir(pod.join("merged"))
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "md"))
        .filter_map(|path| file_time(&path, offset))
        .min()
}

/// The `list` value of the ideas: the backlog items with no spec file.
const IDEA_LIST: &str = "idea";

/// The idea count of each product, as the backlog tool last reported it. It is one shared
/// snapshot for the process, as the spec says. A product that leaves the registry keeps its
/// last count until Harold restarts.
static IDEA_COUNTS: Mutex<BTreeMap<String, u32>> = Mutex::new(BTreeMap::new());

/// What the backlog tool tells pod stats about the ideas. The backlog tool keeps its own
/// storage and makes no instrument: pod stats owns `pod.inventory.items` and
/// `pod.spec.lead_time`.
pub(crate) trait IdeaSignals: Send + Sync {
    /// Sets the number of backlog items of `product` that have no spec.
    fn set_idea_count(&self, product: &str, count: u32);
    /// Records the phase `idea_to_open` of the spec `id`: `wait_s` seconds from the first
    /// entry of the item in the backlog to the draft time of its spec.
    fn record_idea_opened(&self, product: &str, id: &str, wait_s: f64);
}

/// The handle that pod stats gives the backlog tool. It holds no store.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PodSignals {
    live: bool,
}

impl PodSignals {
    /// The handle that reaches the pod instruments.
    pub(crate) fn new() -> Self {
        Self { live: true }
    }

    /// A handle that drops every call, for a tool test that runs without pod stats.
    #[cfg(test)]
    pub(crate) fn none() -> Self {
        Self { live: false }
    }
}

impl IdeaSignals for PodSignals {
    fn set_idea_count(&self, product: &str, count: u32) {
        if self.live {
            lock(&IDEA_COUNTS).insert(product.to_owned(), count);
        }
    }

    fn record_idea_opened(&self, product: &str, id: &str, wait_s: f64) {
        if !self.live {
            return;
        }
        telemetry::instruments().pod_spec_lead_time.record(
            wait_s,
            &[
                KeyValue::new("pod.product", product.to_owned()),
                KeyValue::new("phase", "idea_to_open"),
                KeyValue::new("spec.id", id.to_owned()),
            ],
        );
    }
}

/// The gauge of the specs in a list of work waiting, and its unit.
pub(crate) const INVENTORY_ITEMS: (&str, &str) = ("pod.inventory.items", "{spec}");
/// The gauge of the age of the oldest spec in a list of work waiting, and its unit.
pub(crate) const INVENTORY_OLDEST_HOURS: (&str, &str) = ("pod.inventory.oldest_hours", "h");
/// The gauge of the sum of the ages of the specs in a list of work waiting, and its unit.
pub(crate) const INVENTORY_ITEM_DAYS: (&str, &str) = ("pod.inventory.item_days", "d");

/// The lists of work waiting, in the order the gauges report them.
const WORK_LISTS: [&str; 6] = [
    "awaiting_approval",
    "approved_not_started",
    "in_progress",
    "in_spec_writing",
    "in_architect_review",
    "ready_not_merged",
];

/// What one list of work waiting holds.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct ListStats {
    /// The specs in the list.
    pub items: u64,
    /// The age of the oldest spec, in hours.
    pub oldest_hours: f64,
    /// The sum of the ages of the specs, in days.
    pub item_days: f64,
}

/// The count, the oldest age and the summed age of the specs whose ages are `ages`.
/// An age below 0 counts as 0.
pub(crate) fn list_stats(ages: &[time::Duration]) -> ListStats {
    let seconds = |age: &time::Duration| age.whole_seconds().max(0) as f64;
    ListStats {
        items: ages.len() as u64,
        oldest_hours: ages.iter().map(seconds).fold(0.0, f64::max) / 3600.0,
        item_days: ages.iter().map(seconds).sum::<f64>() / 86400.0,
    }
}

/// The free slots with work waiting, and the three lists of work waiting, at `now`.
/// The free slots count only when an approved spec that is not started exists. An approval
/// before `cutoff` (the earliest merged file) is history and counts in neither; with no
/// `cutoff`, every approval counts.
pub(crate) fn work_waiting(
    specs: &[SpecFacts],
    free_slots: u32,
    cutoff: Option<OffsetDateTime>,
    now: OffsetDateTime,
) -> (u32, BTreeMap<&'static str, ListStats>) {
    let ages = |keep: &dyn Fn(&SpecFacts) -> Option<OffsetDateTime>| -> Vec<time::Duration> {
        specs
            .iter()
            .filter_map(keep)
            .map(|since| now - since)
            .collect()
    };
    // A stale approval (one that names an older text) awaits approval again.
    let approved_since = |spec: &SpecFacts, current: bool| {
        let approved_at = spec.approved_at?;
        (spec.state == SpecState::Approved
            && spec.approval_current == current
            && cutoff.is_none_or(|cutoff| approved_at >= cutoff))
        .then_some(approved_at)
    };
    let awaiting_since = |spec: &SpecFacts| {
        if spec.state == SpecState::Open && spec.awaiting {
            return spec.open_at;
        }
        approved_since(spec, false)
    };
    // Each spec that awaits approval is in one stage, by the last line of its reviews file.
    let awaiting = ages(&awaiting_since);
    let in_stage = |wanted: fn(&ReviewLine) -> bool| {
        ages(&|spec| {
            let last = spec.reviews.as_ref()?.last()?;
            wanted(last).then(|| awaiting_since(spec)).flatten()
        })
    };
    let writing = in_stage(|line| matches!(line, ReviewLine::Verdict(_, false)));
    let review = in_stage(|line| matches!(line, ReviewLine::Sent(_)));
    let approved = ages(&|spec| approved_since(spec, true));
    let in_progress = ages(&|spec| (spec.state == SpecState::InProgress).then_some(spec.state_at));
    let ready = ages(&|spec| {
        (spec.state == SpecState::InProgress)
            .then_some(spec.ready_at)
            .flatten()
    });
    let idle = if approved.is_empty() { 0 } else { free_slots };
    let lists = BTreeMap::from([
        (WORK_LISTS[0], list_stats(&awaiting)),
        (WORK_LISTS[1], list_stats(&approved)),
        (WORK_LISTS[2], list_stats(&in_progress)),
        (WORK_LISTS[3], list_stats(&writing)),
        (WORK_LISTS[4], list_stats(&review)),
        (WORK_LISTS[5], list_stats(&ready)),
    ]);
    (idle, lists)
}

// ---------------------------------------------------------------------------
// Polls
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SlotCounts {
    pub busy: u32,
    pub free: u32,
}

/// Live agents by role and, for a pod's roles, product.
pub(crate) type AgentCounts = BTreeMap<(Role, Option<String>), u64>;

/// Live agents by role, product and state.
pub(crate) type AgentStateCounts = BTreeMap<(Role, Option<String>, EffectiveAgentState), u64>;

/// The six states of an agent, in the order of ADR 0024.
const ALL_AGENT_STATES: [EffectiveAgentState; 6] = [
    EffectiveAgentState::Working,
    EffectiveAgentState::Blocked,
    EffectiveAgentState::Done,
    EffectiveAgentState::Idle,
    EffectiveAgentState::Error,
    EffectiveAgentState::Unknown,
];

/// The part an agent plays, as the `role` attribute of `pod.agents` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Role {
    Chief,
    Pnem,
    PodCoordination,
    BuildWorker,
    Other,
}

impl Role {
    /// The roles of a pod, which carry `pod.product`.
    const POD: [Role; 3] = [Role::Pnem, Role::PodCoordination, Role::BuildWorker];

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Role::Chief => "chief",
            Role::Pnem => "pnem",
            Role::PodCoordination => "pod_coordination",
            Role::BuildWorker => "build_worker",
            Role::Other => "other",
        }
    }
}

/// An agent pane that is live: its pane id and its folder. Nothing else of the agent
/// snapshot is read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LivePane {
    pub pane_id: String,
    pub folder: String,
    /// The agent's effective state.
    pub state: EffectiveAgentState,
}

/// Lists the live agent panes.
pub(crate) type AgentSource = Arc<dyn Fn() -> Vec<LivePane> + Send + Sync>;

/// What the latest poll read, for the gauges. A product the poll could not read is left
/// out, so no gauge reports an old count.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct PodSnapshot {
    /// Slot counts by product.
    pub slots: BTreeMap<String, SlotCounts>,
    /// Specs by product.
    pub specs: BTreeMap<String, Vec<SpecFacts>>,
    /// Whether each registered product's pod runs.
    pub pods: BTreeMap<String, bool>,
    /// The live agents by role and, for a pod's roles, product.
    pub agents: AgentCounts,
    /// The live agents by role, product (for a pod's roles) and state. Every pod role of
    /// every product, the chief and `other` have a count for all six states, 0 included.
    pub agent_states: AgentStateCounts,
    /// The free slots with work waiting by product: the free slots when a spec is approved
    /// and not started, else 0.
    pub idle_with_work: BTreeMap<String, u32>,
    /// The work waiting by product and list: `awaiting_approval`, `approved_not_started`
    /// and `in_progress`.
    pub inventory: BTreeMap<String, BTreeMap<&'static str, ListStats>>,
    /// Every live pane of the poll with its role, product and spec id. `pod.agents` counts
    /// the same panes, so the two agree.
    pub panes: HashMap<String, PaneIdentity>,
}

/// What a live pane is: its role and, for a pod's roles, its product. A build worker also
/// has the id of the spec it builds, when a `started` file names its worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PaneIdentity {
    pub role: Role,
    pub product: Option<String>,
    pub spec_id: Option<String>,
}

/// The identity of a pane in the last poll. A pane that the poll did not see is `other`,
/// with no product and no spec.
pub(crate) fn pane_identity(pane_id: &str) -> PaneIdentity {
    let snapshot = gauge_snapshot();
    let snapshot = lock(&snapshot);
    snapshot
        .panes
        .get(pane_id)
        .cloned()
        .unwrap_or(PaneIdentity {
            role: Role::Other,
            product: None,
            spec_id: None,
        })
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
    /// The products whose merges and specs could not be read, logged since they last
    /// were.
    history_unavailable: HashSet<String>,
    specs: SpecBook,
    /// The busy worktrees of each product at its last poll: folder name -> HEAD commit.
    busy_before: HashMap<String, HashMap<String, Option<String>>>,
    /// The spec worktrees of each product that went and are waiting for git to say their
    /// commit is on the base branch: id -> what was seen.
    gone: HashMap<String, HashMap<String, Gone>>,
    /// The specs of each product seen merged by their worktree going: id -> when.
    gone_merged: HashMap<String, HashMap<String, OffsetDateTime>>,
    /// The Mac's UTC offset, for rejection dates and the times logged.
    offset: UtcOffset,
    /// The products whose merge counters have been recorded from 0.
    counted_from_zero: HashSet<String>,
    /// The live agent panes, taken once a poll.
    agents: AgentSource,
    /// The chief's folder, from `[notify] chief`.
    chief: Option<String>,
    /// The warnings about a spec's merges that were logged, so each is logged once, also
    /// across restarts.
    warned: PodNotices,
    /// The pod files that did not parse and were logged: (product, file) -> what the file
    /// held then.
    unreadable: HashMap<(String, String), String>,
}

/// A spec worktree that went.
#[derive(Debug, Clone)]
struct Gone {
    /// The commit it had checked out.
    head: String,
    /// When Harold saw it gone: its merge time if its commit is on the base branch.
    seen_at: OffsetDateTime,
}

impl<G: Git> PodStats<G> {
    pub(crate) fn new(
        registry: PathBuf,
        tips_file: PathBuf,
        git: G,
        snapshot: Arc<Mutex<PodSnapshot>>,
        offset: UtcOffset,
        agents: AgentSource,
        chief: Option<String>,
    ) -> Self {
        // The tip file is `<data folder>/pod/tips`.
        let data_folder = tips_file
            .parent()
            .and_then(Path::parent)
            .unwrap_or(Path::new("."));
        let warned = PodNotices::load(data_folder.join(NOTICES_FILE));
        Self {
            registry,
            git,
            tips: Tips::load(tips_file),
            snapshot,
            registry_unreadable: false,
            unavailable: HashSet::new(),
            history_unavailable: HashSet::new(),
            specs: SpecBook::default(),
            busy_before: HashMap::new(),
            gone: HashMap::new(),
            gone_merged: HashMap::new(),
            offset,
            counted_from_zero: HashSet::new(),
            agents,
            chief,
            warned,
            unreadable: HashMap::new(),
        }
    }

    /// Reads the registry and every product once, records the merges and replaces the
    /// snapshot. A product that fails is skipped and logged once until it works again. A
    /// product whose slots were read but whose merges or specs were not keeps its slots;
    /// its merges and specs wait for the next poll.
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
        let live = (self.agents)();
        let mut claimed: HashMap<String, (Role, Option<String>)> = HashMap::new();
        let mut specs_of_panes: HashMap<String, String> = HashMap::new();
        for product in &products {
            // HyperDX reads a running total's first point as 0, so each product's merge
            // counters start at 0 before its first merge can be counted.
            if self.counted_from_zero.insert(product.name.clone()) {
                telemetry::register_merge_counters(&product.name);
            }
            let running = claim_pod_panes(product, &live, &mut claimed);
            snapshot.pods.insert(product.name.clone(), running);
            let slots = match slots(&self.git, product).await {
                Ok(slots) => {
                    self.unavailable.remove(&product.name);
                    slots
                }
                Err(reason) => {
                    if self.unavailable.insert(product.name.clone()) {
                        warn!(pod.product = %product.name, reason, "pod facts unavailable");
                    }
                    continue;
                }
            };
            snapshot.slots.insert(
                product.name.clone(),
                SlotCounts {
                    busy: slots.busy,
                    free: slots.free,
                },
            );
            claim_build_workers(product, &slots, &live, &mut claimed, &mut specs_of_panes);
            match self.poll_history(product, &slots).await {
                Ok(specs) => {
                    self.history_unavailable.remove(&product.name);
                    let cutoff = first_merged_file(&product.repo.join(POD_FOLDER), self.offset);
                    let (idle, lists) =
                        work_waiting(&specs, slots.free, cutoff, OffsetDateTime::now_utc());
                    snapshot.idle_with_work.insert(product.name.clone(), idle);
                    snapshot.inventory.insert(product.name.clone(), lists);
                    snapshot.specs.insert(product.name.clone(), specs);
                }
                Err(reason) => {
                    if self.history_unavailable.insert(product.name.clone()) {
                        warn!(pod.product = %product.name, reason, "pod merges and specs unavailable");
                    }
                }
            }
        }
        let claimed = self.claim_the_rest(&live, claimed);
        (snapshot.agents, snapshot.agent_states) = tally(&products, &live, &claimed);
        snapshot.panes = claimed
            .into_iter()
            .map(|(pane_id, (role, product))| {
                let spec_id = specs_of_panes.remove(&pane_id);
                (
                    pane_id,
                    PaneIdentity {
                        role,
                        product,
                        spec_id,
                    },
                )
            })
            .collect();
        *lock(&self.snapshot) = snapshot;
    }

    /// Adds the live panes the pods did not claim: the chief's pane or panes as `Chief`,
    /// and the rest as `Other`. It returns the claimed panes with their role and product.
    fn claim_the_rest(
        &self,
        live: &[LivePane],
        mut claimed: HashMap<String, (Role, Option<String>)>,
    ) -> HashMap<String, (Role, Option<String>)> {
        for pane in live {
            let role = match &self.chief {
                Some(chief) if same_folder(&pane.folder, chief) => Role::Chief,
                _ => Role::Other,
            };
            claimed.entry(pane.pane_id.clone()).or_insert((role, None));
        }
        claimed
    }
}

/// The live agents by role and state, from the claimed panes.
fn tally(
    products: &[Product],
    live: &[LivePane],
    claimed: &HashMap<String, (Role, Option<String>)>,
) -> (AgentCounts, AgentStateCounts) {
    let mut counts = BTreeMap::new();
    for product in products {
        for role in Role::POD {
            counts.insert((role, Some(product.name.clone())), 0);
        }
    }
    counts.insert((Role::Chief, None), 0);
    counts.insert((Role::Other, None), 0);
    let mut by_state = BTreeMap::new();
    for (role, product) in counts.keys() {
        for state in ALL_AGENT_STATES {
            by_state.insert((*role, product.clone(), state), 0);
        }
    }
    for (pane_id, key) in claimed {
        if let Some(pane) = live.iter().find(|pane| pane.pane_id == *pane_id) {
            let (role, product) = key.clone();
            *by_state.entry((role, product, pane.state)).or_insert(0) += 1;
        }
        *counts.entry(key.clone()).or_insert(0) += 1;
    }
    (counts, by_state)
}

impl<G: Git> PodStats<G> {
    /// Reads the product's specs and records its merges, each under the spec it names
    /// (and its stage) and with that merge's lead times, from one read of the base
    /// branch's tip and reflog. Specs are read first and the tip is saved before anything
    /// is recorded, so a pod folder or tip file that cannot be read or written leaves the
    /// tip where it was and the merges are counted by a later poll.
    async fn poll_history(
        &mut self,
        product: &Product,
        slots: &Slots,
    ) -> Result<Vec<SpecFacts>, &'static str> {
        let history = read_history(&self.git, product).await?;
        let pod = product.repo.join(POD_FOLDER);
        let now = OffsetDateTime::now_utc();
        // Where the base branch keeps no reflog, a spec merges when its worktree goes
        // after its commit was merged.
        self.see_worktrees_go(product, slots, &pod, history.reflog.is_empty(), now)
            .await;
        let gone = self
            .gone_merged
            .get(&product.name)
            .cloned()
            .unwrap_or_default();
        let busy = slots
            .busy_worktrees
            .iter()
            .filter_map(|worktree| {
                let created = file_time(&worktree.path.join(AGENT_PANE), self.offset)?;
                Some((worktree.name.clone(), created))
            })
            .collect();
        let SpecRead { specs, notices } = read_specs(
            &pod,
            &Sources {
                busy: &busy,
                reflog: &history.reflog,
                gone: &gone,
                now,
                offset: self.offset,
            },
        )?;
        let old_tip = self.tips.counted_tip(&product.name).map(str::to_owned);
        let moved = self.tips.moved(&self.git, product, &history).await?;
        // The tip is saved before the merges are recorded: a crash between the two loses
        // that poll's counts but never counts them twice.
        self.tips.save(&product.name, &history.tip)?;
        let changes = self.specs.observe(&product.name, &specs);
        let cutoff = first_merged_file(&product.repo.join(POD_FOLDER), self.offset);
        self.log_notices(&product.name, &notices, &specs, cutoff);
        if let TipMove::Merges(merges) = &moved {
            let is_spec = |id: &str| specs.iter().any(|spec| spec.id == id);
            for merge in merges {
                let spec = merge.name.as_deref().map(|name| spec_ref(name, is_spec));
                record_merge(&product.name, merge, spec.as_ref());
                let (Some(spec), Some(entry)) = (spec, merge.entry) else {
                    continue;
                };
                // A spec with a started or merged file gets its steps from the merged
                // file, not from each merge.
                let Some(facts) = specs.iter().find(|facts| {
                    facts.id == spec.id
                        && facts.approved_at.is_some()
                        && facts.started_at.is_none()
                        && facts.merged_commit.is_none()
                }) else {
                    continue;
                };
                let Ok(merged_at) = OffsetDateTime::from_unix_timestamp(history.reflog[entry].time)
                else {
                    continue;
                };
                let first = first_merge(&history.reflog, entry, &spec.id, is_spec);
                record_lead_time(&product.name, &spec, facts, merged_at, first);
            }
        }
        self.record_file_lead_times(product, &specs, &changes, &moved, old_tip, &history.tip)
            .await;
        Ok(specs)
    }

    /// Records the steps of each spec that completed by its merged file in this poll: a
    /// spec whose commit is on the base branch and not in the counted tip before this poll,
    /// or one that changed to `completed` between two polls. A product's first poll
    /// records none, nor does a spec already completed when Harold started.
    async fn record_file_lead_times(
        &self,
        product: &Product,
        specs: &[SpecFacts],
        changes: &[SpecChange],
        moved: &TipMove,
        old_tip: Option<String>,
        tip: &str,
    ) {
        for facts in specs {
            let (SpecState::Completed, Some(commit), Some(merged_at)) =
                (facts.state, &facts.merged_commit, facts.merged_at)
            else {
                continue;
            };
            let just_completed = changes.iter().any(|change| {
                change.id == facts.id
                    && change.state == SpecState::Completed
                    && change
                        .before
                        .is_some_and(|before| before != SpecState::Completed)
            });
            let new_commit = match (moved, &old_tip) {
                (TipMove::Merges(_), Some(old)) => {
                    !self.is_ancestor(product, commit, old).await
                        && self.is_ancestor(product, commit, tip).await
                }
                _ => false,
            };
            if !(new_commit || just_completed && self.is_ancestor(product, commit, tip).await) {
                continue;
            }
            let spec = SpecRef {
                id: facts.id.clone(),
                stage: None,
            };
            record_lead_time(&product.name, &spec, facts, merged_at, true);
        }
    }

    /// Whether `commit` is on `branch_tip`. A `git` that gives no answer says no.
    async fn is_ancestor(&self, product: &Product, commit: &str, branch_tip: &str) -> bool {
        self.git
            .run(
                &product.repo,
                &["merge-base", "--is-ancestor", commit, branch_tip],
            )
            .await
            .is_some_and(|output| output.success)
    }

    /// Logs each large spec of `product` that is approved and not started, once, also
    /// across restarts. Its key is kept in the pod notices and added to `warned_now`. An
    /// approval before `cutoff` (the earliest merged file) is history and is not logged;
    /// with no `cutoff`, every approval counts.
    fn log_large_specs(
        &mut self,
        product: &str,
        specs: &[SpecFacts],
        cutoff: Option<OffsetDateTime>,
        warned_now: &mut HashSet<String>,
    ) {
        for spec in specs {
            let approved = spec.state == SpecState::Approved
                && spec
                    .approved_at
                    .is_some_and(|at| cutoff.is_none_or(|cutoff| at >= cutoff));
            if spec.size != SpecSize::Large || !approved {
                continue;
            }
            let key = format!("{product}\0large\0{}", spec.id);
            warned_now.insert(key.clone());
            if !self.warned.contains(&key) {
                self.warned.insert(key, OffsetDateTime::now_utc());
                warn!(
                    pod.product = %product,
                    spec.id = %spec.id,
                    "large spec approved"
                );
            }
        }
    }

    /// Logs what the specs of `product` show about merges and unreadable files, once each.
    /// A merge warning is kept in the pod notices until the specs of `product` no longer
    /// show it, so it is logged again only if it comes back. This runs only for a product
    /// whose specs were read, so an unavailable product keeps its keys. A large approved
    /// spec is warned about here too, so its key follows the same rule.
    fn log_notices(
        &mut self,
        product: &str,
        notices: &[Notice],
        specs: &[SpecFacts],
        cutoff: Option<OffsetDateTime>,
    ) {
        let mut unreadable_now: HashSet<&str> = HashSet::new();
        let mut warned_now: HashSet<String> = HashSet::new();
        let now = OffsetDateTime::now_utc();
        for notice in notices {
            match notice {
                Notice::Unreadable { id, file, content } => {
                    unreadable_now.insert(file);
                    let key = (product.to_owned(), file.clone());
                    if self.unreadable.get(&key) == Some(content) {
                        continue;
                    }
                    self.unreadable.insert(key, content.clone());
                    warn!(
                        pod.product = %product,
                        spec.id = %id,
                        file = %file,
                        "pod file unreadable"
                    );
                }
                Notice::CompletedWithoutFile {
                    id,
                    merged_at,
                    commit,
                } => {
                    let key = format!("{product}\0completed\0{id}");
                    warned_now.insert(key.clone());
                    if !self.warned.contains(&key) {
                        self.warned.insert(key, now);
                        warn!(
                            pod.product = %product,
                            spec.id = %id,
                            merged_at = %rfc3339(*merged_at),
                            commit = %commit.as_deref().unwrap_or("unknown"),
                            "spec completed without merged file"
                        );
                    }
                }
                Notice::StageMergedWithoutFile { id, stage } => {
                    let key = format!("{product}\0stage\0{id}\0{stage}");
                    warned_now.insert(key.clone());
                    if !self.warned.contains(&key) {
                        self.warned.insert(key, now);
                        warn!(
                            pod.product = %product,
                            spec.id = %id,
                            spec.stage = %stage,
                            "spec stage merged without merged file"
                        );
                    }
                }
            }
        }
        self.log_large_specs(product, specs, cutoff, &mut warned_now);
        // A file that parses again, or went, is logged again if it fails again.
        self.unreadable
            .retain(|(known, file), _| known != product || unreadable_now.contains(file.as_str()));
        // A spec that got its merged file, or went, is warned about again if the gap
        // comes back.
        self.warned.retain_product(product, &warned_now);
        self.warned.save();
    }

    /// Notes the busy worktrees and, when `check` is set, waits on each spec worktree that
    /// went since the last poll: once `git merge-base --is-ancestor <its commit> <base>`
    /// succeeds, the spec counts as merged at the time Harold saw the worktree gone. The
    /// branch itself is usually deleted straight after its worktree, so its commit is
    /// what is checked. A worktree git has not placed on the base branch within an hour
    /// is logged once and not counted.
    async fn see_worktrees_go(
        &mut self,
        product: &Product,
        slots: &Slots,
        pod: &Path,
        check: bool,
        now: OffsetDateTime,
    ) {
        let busy: HashMap<String, Option<String>> = slots
            .busy_worktrees
            .iter()
            .map(|worktree| (worktree.name.clone(), worktree.head.clone()))
            .collect();
        let before = self
            .busy_before
            .insert(product.name.clone(), busy.clone())
            .unwrap_or_default();
        if !check {
            self.gone.remove(&product.name);
            return;
        }
        let gone = self.gone.entry(product.name.clone()).or_default();
        for (id, head) in before {
            if busy.contains_key(&id) || !is_file(&pod.join("specs").join(format!("{id}.md"))).await
            {
                continue;
            }
            let Some(head) = head else {
                info!(
                    pod.product = %product.name,
                    spec.id = %id,
                    "worktree removed without its commit on the base branch; spec not counted as merged"
                );
                continue;
            };
            gone.insert(id, Gone { head, seen_at: now });
        }
        let base = format!("refs/heads/{}", product.base);
        let waiting: Vec<(String, Gone)> = gone
            .iter()
            .map(|(id, gone)| (id.clone(), gone.clone()))
            .collect();
        for (id, waiting) in waiting {
            let merged = self
                .git
                .run(
                    &product.repo,
                    &["merge-base", "--is-ancestor", &waiting.head, &base],
                )
                .await
                .is_some_and(|output| output.success);
            if merged {
                self.gone_merged
                    .entry(product.name.clone())
                    .or_default()
                    .insert(id.clone(), waiting.seen_at);
            } else if now - waiting.seen_at < GONE_RETRY {
                continue;
            } else {
                info!(
                    pod.product = %product.name,
                    spec.id = %id,
                    "worktree removed without its commit on the base branch; spec not counted as merged"
                );
            }
            if let Some(gone) = self.gone.get_mut(&product.name) {
                gone.remove(&id);
            }
        }
    }
}

/// The pane ids in an `agent-panes` or agent-pane file: the first word of each line that
/// has one.
fn pane_ids(text: &str) -> Vec<&str> {
    text.lines()
        .filter_map(|line| line.split_whitespace().next())
        .collect()
}

/// The pod panes that the products' `agent-panes` files name, in product order: line one
/// is pod coordination, line two the PnEM. A pane may be dead: this does not check it.
pub(crate) fn pod_pane_roles(products: &[Product]) -> Vec<(String, Role, String)> {
    let mut roles = Vec::new();
    for product in products {
        let Some(text) = read_text(&product.repo.join(POD_FOLDER).join("agent-panes")) else {
            continue;
        };
        for (pane, role) in pane_ids(&text)
            .into_iter()
            .zip([Role::PodCoordination, Role::Pnem])
        {
            roles.push((pane.to_owned(), role, product.name.clone()));
        }
    }
    roles
}

/// Claims the live panes that the product's `agent-panes` file names: the first is pod
/// coordination, the second the PnEM. Returns whether any of them is live, which is
/// whether the pod runs.
fn claim_pod_panes(
    product: &Product,
    live: &[LivePane],
    claimed: &mut HashMap<String, (Role, Option<String>)>,
) -> bool {
    let mut running = false;
    for (pane, role, name) in pod_pane_roles(std::slice::from_ref(product)) {
        if live.iter().any(|live| live.pane_id == pane) {
            running = true;
            claimed.entry(pane).or_insert((role, Some(name)));
        }
    }
    running
}

/// The id of the spec whose `started/<id>.md` file names `worktree` in its `worktree:`
/// pair, which holds the folder name or the path. A worktree without such a file has no spec id.
fn build_worker_spec(pod: &Path, worktree: &BusyWorktree) -> Option<String> {
    let wanted = worktree.path.to_string_lossy();
    let wanted_real = std::fs::canonicalize(&worktree.path).ok();
    let mut names: Vec<_> = std::fs::read_dir(pod.join("started"))
        .ok()?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
        .collect();
    names.sort();
    names.into_iter().find_map(|path| {
        let text = read_text(&path)?;
        let named = *key_values(&text).get("worktree")?;
        let same = named == worktree.name
            || same_folder(&wanted, named)
            || wanted_real.is_some() && std::fs::canonicalize(named).ok() == wanted_real;
        same.then(|| path.file_stem()?.to_str().map(str::to_owned))?
    })
}

/// Claims the live panes that the busy worktrees of `product` name in their agent-pane
/// file. A pane a pod file claimed before stays with its role.
fn claim_build_workers(
    product: &Product,
    slots: &Slots,
    live: &[LivePane],
    claimed: &mut HashMap<String, (Role, Option<String>)>,
    specs_of_panes: &mut HashMap<String, String>,
) {
    for worktree in &slots.busy_worktrees {
        let Some(text) = read_text(&worktree.path.join(AGENT_PANE)) else {
            continue;
        };
        for pane in pane_ids(&text).into_iter().take(1) {
            let is_live = live.iter().any(|live| live.pane_id == pane);
            if !is_live || claimed.contains_key(pane) {
                continue;
            }
            claimed.insert(
                pane.to_owned(),
                (Role::BuildWorker, Some(product.name.clone())),
            );
            if let Some(id) = build_worker_spec(&product.repo.join(POD_FOLDER), worktree) {
                specs_of_panes.insert(pane.to_owned(), id);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The task
// ---------------------------------------------------------------------------

struct Gauges {
    snapshot: Arc<Mutex<PodSnapshot>>,
    _busy: ObservableGauge<u64>,
    _free: ObservableGauge<u64>,
    _specs: ObservableGauge<u64>,
    _words: ObservableGauge<u64>,
    _effort: ObservableGauge<f64>,
    _confirmed: ObservableGauge<f64>,
    _evidence_missing: ObservableGauge<u64>,
    _pods: ObservableGauge<u64>,
    _agents: ObservableGauge<u64>,
    _idle_with_work: ObservableGauge<u64>,
    _inventory_items: ObservableGauge<u64>,
    _inventory_oldest_hours: ObservableGauge<f64>,
    _inventory_item_days: ObservableGauge<f64>,
}

static GAUGES: OnceLock<Gauges> = OnceLock::new();

/// The snapshot the `pod.slots.busy`, `pod.slots.free`, `pod.specs`, `pod.spec.words`,
/// `pod.spec.effort`, `pod.pods`, `pod.agents`, `pod.slots.idle_with_work` (the free slots
/// with work waiting) and `pod.inventory.items`, `pod.inventory.oldest_hours` and
/// `pod.inventory.item_days` (the work waiting, by list) gauges read. The first call
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
        let specs = Arc::clone(&snapshot);
        let words = Arc::clone(&snapshot);
        let effort = Arc::clone(&snapshot);
        let confirmed = Arc::clone(&snapshot);
        let evidence = Arc::clone(&snapshot);
        let pods = Arc::clone(&snapshot);
        let agents = Arc::clone(&snapshot);
        let idle = Arc::clone(&snapshot);
        let work_waiting_snapshot = |pick: fn(&ListStats) -> f64| {
            let snapshot = Arc::clone(&snapshot);
            move |observer: &dyn Fn(f64, &[KeyValue])| {
                for (product, lists) in &lock(&snapshot).inventory {
                    for (list, stats) in lists {
                        observer(
                            pick(stats),
                            &[
                                KeyValue::new("pod.product", product.clone()),
                                KeyValue::new("list", *list),
                            ],
                        );
                    }
                }
            }
        };
        let items = work_waiting_snapshot(|stats| stats.items as f64);
        let oldest = work_waiting_snapshot(|stats| stats.oldest_hours);
        let item_days = work_waiting_snapshot(|stats| stats.item_days);
        Gauges {
            _busy: gauge("pod.slots.busy", |counts| counts.busy),
            _free: gauge("pod.slots.free", |counts| counts.free),
            // Every state of every product, 0 included, so a card can read 0.
            _specs: meter
                .u64_observable_gauge("pod.specs")
                .with_unit("{spec}")
                .with_callback(move |observer| {
                    for (product, specs) in &lock(&specs).specs {
                        for state in SpecState::ALL {
                            let count = specs.iter().filter(|spec| spec.state == state).count();
                            observer.observe(
                                u64::try_from(count).unwrap_or(u64::MAX),
                                &[
                                    KeyValue::new("pod.product", product.clone()),
                                    KeyValue::new("state", state.as_str()),
                                ],
                            );
                        }
                    }
                })
                .build(),
            _words: meter
                .u64_observable_gauge("pod.spec.words")
                .with_unit("{word}")
                .with_callback(move |observer| {
                    for (product, specs) in &lock(&words).specs {
                        for spec in specs {
                            observer.observe(
                                spec.words,
                                &[
                                    KeyValue::new("pod.product", product.clone()),
                                    KeyValue::new("spec.id", spec.id.clone()),
                                    KeyValue::new("state", spec.state.as_str()),
                                ],
                            );
                        }
                    }
                })
                .build(),
            _effort: meter
                .f64_observable_gauge("pod.spec.effort")
                .with_unit("1")
                .with_callback(move |observer| {
                    let now = OffsetDateTime::now_utc();
                    for (product, specs) in &lock(&effort).specs {
                        for spec in effort_specs(specs, now) {
                            let mut attributes = vec![
                                KeyValue::new("pod.product", product.clone()),
                                KeyValue::new("spec.id", spec.id.clone()),
                                KeyValue::new("state", spec.state.as_str()),
                            ];
                            if let Some(withdrawal) = &spec.withdrawn {
                                attributes
                                    .push(KeyValue::new("withdrawn.reason", withdrawal.reason));
                                attributes.push(KeyValue::new(
                                    "withdrawn.reached",
                                    withdrawal.reached.as_str(),
                                ));
                            }
                            attributes.push(KeyValue::new("spec.size", spec.size.as_str()));
                            attributes.push(KeyValue::new("spec.kind", spec.kind.as_str()));
                            for (measure, value) in effort_measures(spec)
                                .into_iter()
                                .chain(extra_effort_measures(spec))
                            {
                                let mut with_measure = attributes.clone();
                                with_measure.push(KeyValue::new("measure", measure));
                                observer.observe(value, &with_measure);
                            }
                        }
                    }
                })
                .build(),
            // One point for each confirmed file, with its time in Unix seconds. A restart
            // exports the same times again.
            _confirmed: meter
                .f64_observable_gauge("pod.fix.confirmed_at")
                .with_unit("s")
                .with_callback(move |observer| {
                    for (product, specs) in &lock(&confirmed).specs {
                        for spec in specs {
                            let Some(at) = spec.confirmed_at else {
                                continue;
                            };
                            observer.observe(
                                at.unix_timestamp() as f64,
                                &[
                                    KeyValue::new("pod.product", product.clone()),
                                    KeyValue::new("spec.id", spec.id.clone()),
                                ],
                            );
                        }
                    }
                })
                .build(),
            // One point for each spec with an evidence file: 1 when its state is `missing`.
            _evidence_missing: meter
                .u64_observable_gauge("pod.evidence.missing")
                .with_unit("{spec}")
                .with_callback(move |observer| {
                    for (product, specs) in &lock(&evidence).specs {
                        for spec in specs {
                            let Some(missing) = spec.evidence_missing else {
                                continue;
                            };
                            observer.observe(
                                u64::from(missing),
                                &[
                                    KeyValue::new("pod.product", product.clone()),
                                    KeyValue::new("spec.id", spec.id.clone()),
                                ],
                            );
                        }
                    }
                })
                .build(),
            // Every registered pod, `running` and `stopped`, so a card can read 0.
            _pods: meter
                .u64_observable_gauge("pod.pods")
                .with_unit("{pod}")
                .with_callback(move |observer| {
                    for (product, running) in &lock(&pods).pods {
                        for (state, value) in [("running", *running), ("stopped", !*running)] {
                            observer.observe(
                                u64::from(value),
                                &[
                                    KeyValue::new("pod.product", product.clone()),
                                    KeyValue::new("state", state),
                                ],
                            );
                        }
                    }
                })
                .build(),
            _agents: meter
                .u64_observable_gauge("pod.agents")
                .with_unit("{agent}")
                .with_callback(move |observer| {
                    for ((role, product, state), count) in &lock(&agents).agent_states {
                        let mut attributes = vec![
                            KeyValue::new("role", role.as_str()),
                            KeyValue::new("state", state.as_str()),
                        ];
                        if let Some(product) = product {
                            attributes.push(KeyValue::new("pod.product", product.clone()));
                        }
                        observer.observe(*count, &attributes);
                    }
                })
                .build(),
            // The free slots with work waiting, for every product with a read.
            _idle_with_work: meter
                .u64_observable_gauge("pod.slots.idle_with_work")
                .with_unit("{slot}")
                .with_callback(move |observer| {
                    for (product, count) in &lock(&idle).idle_with_work {
                        observer.observe(
                            u64::from(*count),
                            &[KeyValue::new("pod.product", product.clone())],
                        );
                    }
                })
                .build(),
            // All three lists of every product with a read, 0 included.
            _inventory_items: meter
                .u64_observable_gauge(INVENTORY_ITEMS.0)
                .with_unit(INVENTORY_ITEMS.1)
                .with_callback(move |observer| {
                    items(&|value, attributes| observer.observe(value as u64, attributes));
                    for (product, count) in lock(&IDEA_COUNTS).iter() {
                        observer.observe(
                            u64::from(*count),
                            &[
                                KeyValue::new("pod.product", product.clone()),
                                KeyValue::new("list", IDEA_LIST),
                            ],
                        );
                    }
                })
                .build(),
            _inventory_oldest_hours: meter
                .f64_observable_gauge(INVENTORY_OLDEST_HOURS.0)
                .with_unit(INVENTORY_OLDEST_HOURS.1)
                .with_callback(move |observer| {
                    oldest(&|value, attributes| observer.observe(value, attributes));
                })
                .build(),
            _inventory_item_days: meter
                .f64_observable_gauge(INVENTORY_ITEM_DAYS.0)
                .with_unit(INVENTORY_ITEM_DAYS.1)
                .with_callback(move |observer| {
                    item_days(&|value, attributes| observer.observe(value, attributes));
                })
                .build(),
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
///
/// Each poll takes the pane ids and folders of `snapshots`, Harold's agent snapshot, to
/// count the pods and agents. `chief` is the chief's folder.
pub(crate) fn start(
    registry: PathBuf,
    tips_file: PathBuf,
    snapshots: AgentSnapshotHub,
    chief: Option<String>,
    shutdown: watch::Receiver<()>,
) -> JoinHandle<()> {
    let agents: AgentSource = Arc::new(move || {
        snapshots
            .current()
            .panes
            .iter()
            .map(|pane| LivePane {
                pane_id: pane.pane.incarnation.pane_id.clone(),
                folder: pane.pane.working_directory.clone(),
                state: pane.effective_state,
            })
            .collect()
    });
    let stats = PodStats::new(
        registry,
        tips_file,
        SystemGit::default(),
        gauge_snapshot(),
        telemetry::local_offset(),
        agents,
        chief,
    );
    tokio::spawn(run(stats, shutdown))
}

/// Held by every test that writes the process-wide `gauge_snapshot()`, so one test cannot
/// replace the products of another.
#[cfg(test)]
pub(crate) static GAUGE_SNAPSHOT_TESTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(test)]
#[path = "pod_stats_tests.rs"]
mod tests;
