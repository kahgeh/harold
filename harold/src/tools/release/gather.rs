//! Reads the pod folder and git, then gives the facts to `classify`. This module only
//! reads: it runs no `fetch` and writes no file.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use time::{OffsetDateTime, UtcOffset};

use super::classify::{Branch, Facts, ReleaseTag, classify};
use super::model::ProductNext;
use super::title::spec_title;
use crate::pod_stats::{
    AGENT_PANE, Git, NO_ANSWER, POD_FOLDER, Product, Sources, SpecRead, SpecState, file_time,
    read_history, read_specs, slots,
};

/// The first line of a spec that waits for the approval of the user.
const WAITING_LINE: &str = "Waiting for your approval.";

/// The lists of the next release of `product`. When git gives no answer, the product
/// has the reason `git gave no answer` and empty lists.
pub(super) async fn next_for(
    git: &impl Git,
    product: &Product,
    now: OffsetDateTime,
    offset: UtcOffset,
) -> ProductNext {
    match gather(git, product, now, offset).await {
        Ok(next) => next,
        Err(reason) => ProductNext {
            name: product.name.clone(),
            unavailable: Some(reason),
            ..Default::default()
        },
    }
}

/// A paused product has no lists: only its newest release.
pub(super) async fn paused_for(git: &impl Git, product: &Product) -> ProductNext {
    let origin = format!("origin/{}", product.base);
    let mut next = ProductNext {
        name: product.name.clone(),
        paused: true,
        ..Default::default()
    };
    match read_release(git, product, &origin).await {
        Ok(tag) => next.release = tag.map(|tag| tag.name).unwrap_or_default(),
        Err(reason) => next.unavailable = Some(reason),
    }
    next
}

async fn gather(
    git: &impl Git,
    product: &Product,
    now: OffsetDateTime,
    offset: UtcOffset,
) -> Result<ProductNext, &'static str> {
    let origin = format!("origin/{}", product.base);
    let release = read_release(git, product, &origin).await?;

    // The specs, read as the pod poll reads them.
    let history = read_history(git, product).await?;
    let busy_worktrees = slots(git, product).await?.busy_worktrees;
    let busy = busy_worktrees
        .iter()
        .filter_map(|worktree| {
            let created = file_time(&worktree.path.join(AGENT_PANE), offset)?;
            Some((worktree.name.clone(), created))
        })
        .collect();
    let pod = product.repo.join(POD_FOLDER);
    let SpecRead { specs, .. } = read_specs(
        &pod,
        &Sources {
            busy: &busy,
            reflog: &history.reflog,
            // `gone` is state of the pod poller: the worktrees it saw go. This tool has
            // no poller, so the map is empty here.
            gone: &HashMap::new(),
            now,
            offset,
        },
    )?;

    // Ancestry: a completed spec is in the release when the tag reaches its commit.
    let commits: Vec<&str> = specs
        .iter()
        .filter(|spec| spec.state == SpecState::Completed)
        .filter_map(|spec| spec.merged_commit.as_deref())
        .collect();
    let in_release = match &release {
        Some(tag) => in_release(git, &product.repo, &tag.name, &commits).await?,
        None => HashSet::new(),
    };

    // Branches: the commits of each in-progress spec over the base branch.
    let listing = ask(git, &product.repo, &["worktree", "list", "--porcelain"]).await?;
    let branches = branch_by_folder(&listing.unwrap_or_default());
    let mut branch_commits = HashMap::new();
    for spec in specs.iter().filter(|s| s.state == SpecState::InProgress) {
        let commits = match branches.get(&spec.id) {
            Some(branch) => {
                // `refs/heads/` stops a tag with the branch name from taking its place.
                let range = format!("{origin}..refs/heads/{branch}");
                let count = ask(git, &product.repo, &["rev-list", "--count", &range]).await?;
                // The spec has a branch. When the count fails (for example the product
                // has no `origin/<base>`), the count is unknown.
                match count.and_then(|text| text.trim().parse::<u32>().ok()) {
                    Some(commits) => Branch::Commits(commits),
                    None => Branch::Unknown,
                }
            }
            None => Branch::None,
        };
        branch_commits.insert(spec.id.clone(), commits);
    }

    let mut titles = HashMap::new();
    let mut awaiting = HashSet::new();
    for spec in &specs {
        let Ok(text) = std::fs::read_to_string(pod.join(format!("specs/{}.md", spec.id))) else {
            continue;
        };
        titles.insert(spec.id.clone(), spec_title(&spec.id, &text));
        if text.lines().next().map(str::trim) == Some(WAITING_LINE) {
            awaiting.insert(spec.id.clone());
        }
    }
    let parked = parked_ids(&pod);
    let first_merged_file = first_merged_file(&pod, offset);

    let mut next = classify(
        &product.name,
        &Facts {
            specs: &specs,
            titles: &titles,
            parked: &parked,
            branch_commits: &branch_commits,
            in_release: &in_release,
            release: release.clone(),
            first_merged_file,
            awaiting: &awaiting,
            now,
            // Set below, when the merged list is known.
            other_commits: 0,
        },
    );

    // Other commits: the commits on the base branch after the tag that no commit of a
    // listed merged spec reaches. A spec merges as several commits, so the count walks
    // from each of them. No release gives 0. When the count fails, it stays 0.
    if let Some(tag) = &release {
        let listed: Vec<String> = specs
            .iter()
            .filter(|spec| next.merged.iter().any(|line| line.id == spec.id))
            .filter_map(|spec| spec.merged_commit.as_deref())
            .map(|commit| format!("^{commit}"))
            .collect();
        let tag_exclusion = format!("^{}", tag.name);
        let mut args = vec![
            "rev-list",
            "--count",
            origin.as_str(),
            tag_exclusion.as_str(),
        ];
        args.extend(listed.iter().map(String::as_str));
        let count = ask(git, &product.repo, &args).await?;
        next.other_commits = count
            .and_then(|text| text.trim().parse::<u32>().ok())
            .unwrap_or(0);
    }
    Ok(next)
}

/// Runs a git command. `Err` means git gave no answer. `Ok(None)` means the command
/// failed.
async fn ask(git: &impl Git, repo: &Path, args: &[&str]) -> Result<Option<String>, &'static str> {
    let output = git.run(repo, args).await.ok_or(NO_ANSWER)?;
    Ok(output.success.then_some(output.stdout))
}

/// The commits that the tag reaches. One call lists the commits that are in the history
/// of the given commits but not in the tag. That output can hold more hashes than the
/// given commits. A commit that it prints is not in the tag. When that call fails (for example, a commit is unknown), one call per commit
/// answers instead. A hash that git prints matches a commit by prefix.
async fn in_release(
    git: &impl Git,
    repo: &Path,
    tag: &str,
    commits: &[&str],
) -> Result<HashSet<String>, &'static str> {
    if commits.is_empty() {
        return Ok(HashSet::new());
    }
    let exclusion = format!("^{tag}");
    let mut args = vec!["rev-list", exclusion.as_str()];
    args.extend(commits);
    if let Some(text) = ask(git, repo, &args).await? {
        let printed: Vec<&str> = text
            .lines()
            .map(str::trim)
            .filter(|hash| !hash.is_empty())
            .collect();
        return Ok(commits
            .iter()
            .filter(|commit| {
                !printed
                    .iter()
                    .any(|hash| hash.starts_with(**commit) || commit.starts_with(hash))
            })
            .map(|commit| commit.to_string())
            .collect());
    }
    let mut reached = HashSet::new();
    for commit in commits {
        if ask(git, repo, &["merge-base", "--is-ancestor", commit, tag])
            .await?
            .is_some()
        {
            reached.insert(commit.to_string());
        }
    }
    Ok(reached)
}

/// The newest tag that matches `v[0-9]*` on `origin/<base>`, with the time of its commit.
/// A failed command means no release.
async fn read_release(
    git: &impl Git,
    product: &Product,
    origin: &str,
) -> Result<Option<ReleaseTag>, &'static str> {
    let describe = [
        "describe",
        "--tags",
        "--abbrev=0",
        "--match",
        "v[0-9]*",
        origin,
    ];
    let Some(text) = ask(git, &product.repo, &describe).await? else {
        return Ok(None);
    };
    let name = text.trim().to_string();
    if name.is_empty() {
        return Ok(None);
    }
    let Some(text) = ask(git, &product.repo, &["log", "-1", "--format=%ct", &name]).await? else {
        return Ok(None);
    };
    let commit_time = text
        .trim()
        .parse::<i64>()
        .ok()
        .and_then(|seconds| OffsetDateTime::from_unix_timestamp(seconds).ok());
    Ok(commit_time.map(|commit_time| ReleaseTag { name, commit_time }))
}

/// The branch of each worktree in `git worktree list --porcelain` output, by the name of
/// the worktree folder.
fn branch_by_folder(listing: &str) -> HashMap<String, String> {
    let mut branches = HashMap::new();
    let mut folder: Option<String> = None;
    for line in listing.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            folder = Path::new(path)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned());
        } else if let (Some(branch), Some(folder)) =
            (line.strip_prefix("branch refs/heads/"), &folder)
        {
            branches.insert(folder.clone(), branch.trim().to_string());
        }
    }
    branches
}

/// The ids that have a file `parked/<id>-handoff.md`.
fn parked_ids(pod: &Path) -> HashSet<String> {
    let Ok(entries) = std::fs::read_dir(pod.join("parked")) else {
        return HashSet::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            name.strip_suffix("-handoff.md").map(str::to_string)
        })
        .collect()
}

/// The earliest time of the files in `merged/`.
fn first_merged_file(pod: &Path, offset: UtcOffset) -> Option<OffsetDateTime> {
    std::fs::read_dir(pod.join("merged"))
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "md"))
        .filter_map(|path| file_time(&path, offset))
        .min()
}

#[cfg(test)]
#[path = "gather_tests.rs"]
pub(in crate::tools::release) mod gather_tests;
