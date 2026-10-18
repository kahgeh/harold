//! Pod facts: how many of each product's build slots are busy and free, what merges to
//! each product's base branch, and each product's specs by state, with their size and
//! the time from "spec ready" to merged. Every minute Harold reads the chief's
//! `products.toml`, runs read-only `git` against each product's base checkout and reads
//! its pod folder. It never writes there.

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
use time::{Date, Month, OffsetDateTime, Time, UtcOffset};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use tracing::{info, warn};

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

/// The pod folder of a base checkout, which holds the product's specs.
const POD_FOLDER: &str = ".superpowers/pod";

/// Why a product's facts, or the registry, could not be read. Each is fixed text: the
/// `reason` field is exported.
const NO_ANSWER: &str = "git gave no answer";
const GIT_FAILED: &str = "git failed";
const REGISTRY_UNREADABLE: &str = "registry unreadable";
const POD_UNREADABLE: &str = "pod folder unreadable";

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
        history: &History,
    ) -> Result<TipMove, &'static str> {
        let tip = history.tip.clone();
        let Some(old) = self.0.get(&product.name).cloned() else {
            self.0.insert(product.name.clone(), tip);
            return Ok(TipMove::Unchanged);
        };
        if old == tip {
            return Ok(TipMove::Unchanged);
        }
        let (merges, rewritten) = merges(git, product, &old, &tip, &history.reflog).await?;
        let moved = if merges.is_empty() && rewritten {
            TipMove::Rewritten
        } else {
            TipMove::Merges(merges)
        };
        self.0.insert(product.name.clone(), tip);
        Ok(moved)
    }
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

/// The state of one spec. A later state wins: a merged spec is merged even though its
/// approval file remains.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum SpecState {
    Drafted,
    Waiting,
    Approved,
    Building,
    Rejected,
    Merged,
}

impl SpecState {
    /// Every state, earliest first.
    pub(crate) const ALL: [SpecState; 6] = [
        SpecState::Drafted,
        SpecState::Waiting,
        SpecState::Approved,
        SpecState::Building,
        SpecState::Rejected,
        SpecState::Merged,
    ];

    /// The `state` attribute's value.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            SpecState::Drafted => "drafted",
            SpecState::Waiting => "waiting_for_approval",
            SpecState::Approved => "approved",
            SpecState::Building => "building",
            SpecState::Rejected => "rejected",
            SpecState::Merged => "merged",
        }
    }
}

/// What the pod folder and the base branch say about one spec.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SpecFacts {
    pub id: String,
    pub state: SpecState,
    /// When the spec entered `state`.
    pub state_at: OffsetDateTime,
    /// "Spec ready": the creation time of `specs/<id>.url`.
    pub ready_at: Option<OffsetDateTime>,
    /// The creation time of `approvals/<id>.md`.
    pub approved_at: Option<OffsetDateTime>,
    pub merged_at: Option<OffsetDateTime>,
    /// The words of `specs/<id>.md`, leaving out drawn lines.
    pub words: u64,
    /// The `## Revision <n>` headings of `requests/<id>.md`.
    pub revisions: u32,
}

/// The creation time of the file at `path`, or its modification time where the file
/// system keeps no creation time. None when there is no such file.
fn file_time(path: &Path, offset: UtcOffset) -> Option<OffsetDateTime> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    let time = metadata.created().or_else(|_| metadata.modified()).ok()?;
    Some(OffsetDateTime::from(time).to_offset(offset))
}

/// The text of the file at `path`, or None when it cannot be read.
fn read_text(path: &Path) -> Option<String> {
    std::fs::read(path)
        .ok()
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

/// The specs of the pod folder `pod`, by id. Reads <repo>/.superpowers/pod/ (never
/// writes). `building` = busy worktree folder names -> the creation time of their
/// agent-pane file; `merged` = spec id -> the time of the reflog entry `merge <id>: ...`
/// (the entry's own time, not the commit's), or from seeing the worktree go. A spec is a
/// `specs/<id>.md` file: an id with no spec file is not a spec. A pod without a `specs`
/// folder has no specs; one whose `specs` folder cannot be read is an error.
pub(crate) fn read_specs(
    pod: &Path,
    building: &HashMap<String, OffsetDateTime>,
    merged: &HashMap<String, OffsetDateTime>,
    offset: UtcOffset,
) -> Result<Vec<SpecFacts>, &'static str> {
    let entries = match std::fs::read_dir(pod.join("specs")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(_) => return Err(POD_UNREADABLE),
    };
    let mut ids: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension()? != "md" {
                return None;
            }
            Some(path.file_stem()?.to_str()?.to_owned())
        })
        .collect();
    ids.sort();
    let rejected = read_text(&pod.join("rejections.md"))
        .map(|text| rejections(&text, offset))
        .unwrap_or_default();
    Ok(ids
        .into_iter()
        .filter_map(|id| spec_facts(pod, id, building, merged, &rejected, offset))
        .collect())
}

fn spec_facts(
    pod: &Path,
    id: String,
    building: &HashMap<String, OffsetDateTime>,
    merged: &HashMap<String, OffsetDateTime>,
    rejected: &HashMap<String, OffsetDateTime>,
    offset: UtcOffset,
) -> Option<SpecFacts> {
    let spec = pod.join("specs").join(format!("{id}.md"));
    let drafted_at = file_time(&spec, offset)?;
    let ready_at = file_time(&pod.join("specs").join(format!("{id}.url")), offset);
    let approved_at = file_time(&pod.join("approvals").join(format!("{id}.md")), offset);
    let time_in =
        |times: &HashMap<String, OffsetDateTime>| times.get(&id).map(|time| time.to_offset(offset));
    let merged_at = time_in(merged);
    let (state, state_at) = [
        (SpecState::Merged, merged_at),
        (SpecState::Rejected, time_in(rejected)),
        (SpecState::Building, time_in(building)),
        (SpecState::Approved, approved_at),
        (SpecState::Waiting, ready_at),
    ]
    .into_iter()
    .find_map(|(state, time)| Some((state, time?)))
    .unwrap_or((SpecState::Drafted, drafted_at));
    let words = read_text(&spec).map_or(0, |text| spec_words(&text));
    let revisions = read_text(&pod.join("requests").join(format!("{id}.md")))
        .map_or(0, |text| revisions(&text));
    Some(SpecFacts {
        id,
        state,
        state_at,
        ready_at,
        approved_at,
        merged_at,
        words,
        revisions,
    })
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

/// The count of lines starting "## Revision ".
pub(crate) fn revisions(request: &str) -> u32 {
    let count = request
        .lines()
        .filter(|line| line.starts_with("## Revision "))
        .count();
    u32::try_from(count).unwrap_or(u32::MAX)
}

/// The rejected specs of `rejections.md`: each line `- <id>, <YYYY-MM-DD>, <reason>`
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

/// The specs merged by name in the reflog: `<id>` -> the time of its newest
/// `merge <id>:` entry.
fn reflog_merges(reflog: &[ReflogEntry]) -> HashMap<String, OffsetDateTime> {
    let mut merged = HashMap::new();
    for entry in reflog {
        let (Some(id), Ok(time)) = (
            merge_name(&entry.subject),
            OffsetDateTime::from_unix_timestamp(entry.time),
        ) else {
            continue;
        };
        merged.entry(id).or_insert(time);
    }
    merged
}

/// The last known state of each product's specs.
#[derive(Default)]
pub(crate) struct SpecBook {
    known: HashMap<(String, String), SpecState>,
    baselined: HashSet<String>,
}

impl SpecBook {
    /// The first call per product records states and reports nothing. Later calls log one
    /// line per change. Returns the changes as (spec id, new state).
    pub(crate) fn observe(
        &mut self,
        product: &str,
        specs: &[SpecFacts],
    ) -> Vec<(String, SpecState)> {
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
            changes.push((spec.id.clone(), spec.state));
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

/// Records the phases of one counted merge of `spec`, merged at `merged_at`: waiting for
/// the user (ready to approved), only when `first` is set, as a spec waits once however
/// many merges it takes, and the build (approved to merged). A phase missing a time, or
/// with a negative length, is skipped.
fn record_lead_time(
    product: &str,
    spec: &SpecRef,
    facts: &SpecFacts,
    merged_at: OffsetDateTime,
    first: bool,
) {
    let phases = [
        ("waiting_for_user", first, facts.ready_at, facts.approved_at),
        ("build", true, facts.approved_at, Some(merged_at)),
    ];
    for (phase, wanted, from, to) in phases {
        let (true, Some(from), Some(to)) = (wanted, from, to) else {
            continue;
        };
        let seconds = (to - from).as_seconds_f64();
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
    /// Specs by product.
    pub specs: BTreeMap<String, Vec<SpecFacts>>,
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
        git: G,
        snapshot: Arc<Mutex<PodSnapshot>>,
        offset: UtcOffset,
    ) -> Self {
        Self {
            registry,
            git,
            tips: Tips::default(),
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
        for product in &products {
            // HyperDX reads a running total's first point as 0, so each product's merge
            // counters start at 0 before its first merge can be counted.
            if self.counted_from_zero.insert(product.name.clone()) {
                telemetry::register_merge_counters(&product.name);
            }
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
            match self.poll_history(product, &slots).await {
                Ok(specs) => {
                    self.history_unavailable.remove(&product.name);
                    snapshot.specs.insert(product.name.clone(), specs);
                }
                Err(reason) => {
                    if self.history_unavailable.insert(product.name.clone()) {
                        warn!(pod.product = %product.name, reason, "pod merges and specs unavailable");
                    }
                }
            }
        }
        *lock(&self.snapshot) = snapshot;
    }

    /// Reads the product's specs and records its merges, each under the spec it names
    /// (and its stage) and with that merge's lead times, from one read of the base
    /// branch's tip and reflog. Specs are read first, so a pod folder that cannot be read
    /// leaves the tip where it was and the merges are counted by a later poll.
    async fn poll_history(
        &mut self,
        product: &Product,
        slots: &Slots,
    ) -> Result<Vec<SpecFacts>, &'static str> {
        let history = read_history(&self.git, product).await?;
        let pod = product.repo.join(POD_FOLDER);
        // Where the base branch keeps no reflog, a spec merges when its worktree goes
        // after its commit was merged.
        self.see_worktrees_go(
            product,
            slots,
            &pod,
            history.reflog.is_empty(),
            OffsetDateTime::now_utc(),
        )
        .await;
        let mut merged = self
            .gone_merged
            .get(&product.name)
            .cloned()
            .unwrap_or_default();
        merged.extend(reflog_merges(&history.reflog));
        let building = slots
            .busy_worktrees
            .iter()
            .filter_map(|worktree| {
                let created = file_time(&worktree.path.join(AGENT_PANE), self.offset)?;
                Some((worktree.name.clone(), created))
            })
            .collect();
        let specs = read_specs(&pod, &building, &merged, self.offset)?;
        if let TipMove::Merges(merges) = self.tips.observe(&self.git, product, &history).await? {
            let is_spec = |id: &str| specs.iter().any(|spec| spec.id == id);
            for merge in &merges {
                let spec = merge.name.as_deref().map(|name| spec_ref(name, is_spec));
                record_merge(&product.name, merge, spec.as_ref());
                let (Some(spec), Some(entry)) = (spec, merge.entry) else {
                    continue;
                };
                let Some(facts) = specs
                    .iter()
                    .find(|facts| facts.id == spec.id && facts.approved_at.is_some())
                else {
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
        self.specs.observe(&product.name, &specs);
        Ok(specs)
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

// ---------------------------------------------------------------------------
// The task
// ---------------------------------------------------------------------------

struct Gauges {
    snapshot: Arc<Mutex<PodSnapshot>>,
    _busy: ObservableGauge<u64>,
    _free: ObservableGauge<u64>,
    _specs: ObservableGauge<u64>,
    _words: ObservableGauge<u64>,
}

static GAUGES: OnceLock<Gauges> = OnceLock::new();

/// The snapshot the `pod.slots.busy`, `pod.slots.free`, `pod.specs` and `pod.spec.words`
/// gauges read. The first call registers the gauges.
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
    let stats = PodStats::new(
        registry,
        SystemGit,
        gauge_snapshot(),
        telemetry::local_offset(),
    );
    tokio::spawn(run(stats, shutdown))
}

#[cfg(test)]
#[path = "pod_stats_tests.rs"]
mod tests;
