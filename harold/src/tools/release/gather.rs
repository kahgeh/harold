//! Reads the pod folder and git, then gives the facts to `classify`. This module only
//! reads: it runs no `fetch` and writes no file.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use time::{OffsetDateTime, UtcOffset};

use super::classify::{Facts, ReleaseTag, classify};
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

    // Ancestry: a completed spec is in the release when its commit is in the tag.
    let mut in_release = HashSet::new();
    if let Some(tag) = &release {
        for spec in specs.iter().filter(|s| s.state == SpecState::Completed) {
            let Some(commit) = &spec.merged_commit else {
                continue;
            };
            let args = ["merge-base", "--is-ancestor", commit, &tag.name];
            if ask(git, &product.repo, &args).await?.is_some() {
                in_release.insert(commit.clone());
            }
        }
    }

    // Branches: the commits of each in-progress spec over the base branch.
    let listing = ask(git, &product.repo, &["worktree", "list", "--porcelain"]).await?;
    let branches = branch_by_folder(&listing.unwrap_or_default());
    let mut branch_commits = HashMap::new();
    for spec in specs.iter().filter(|s| s.state == SpecState::InProgress) {
        let commits = match branches.get(&spec.id) {
            Some(branch) => {
                let range = format!("{origin}..{branch}");
                let count = ask(git, &product.repo, &["rev-list", "--count", &range]).await?;
                count.and_then(|text| text.trim().parse::<u32>().ok())
            }
            None => None,
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

    // Other commits: the commits after the tag, less the commits that the merged list
    // holds. No release gives 0.
    if let Some(tag) = &release {
        let range = format!("{}..{origin}", tag.name);
        let count = ask(git, &product.repo, &["rev-list", "--count", &range]).await?;
        let after_tag = count
            .and_then(|text| text.trim().parse::<u32>().ok())
            .unwrap_or(0);
        let listed = specs
            .iter()
            .filter(|spec| next.merged.iter().any(|line| line.id == spec.id))
            .filter(|spec| spec.merged_commit.is_some())
            .count();
        next.other_commits = after_tag.saturating_sub(u32::try_from(listed).unwrap_or(u32::MAX));
    }
    Ok(next)
}

/// Runs a git command. `Err` means git gave no answer. `Ok(None)` means the command
/// failed.
async fn ask(git: &impl Git, repo: &Path, args: &[&str]) -> Result<Option<String>, &'static str> {
    let output = git.run(repo, args).await.ok_or(NO_ANSWER)?;
    Ok(output.success.then_some(output.stdout))
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
mod gather_tests;
