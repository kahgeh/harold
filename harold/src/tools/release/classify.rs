//! Sorts the facts of one product into the lists of its next release. This module has no
//! input or output. It never decides the state of a spec: it reads `SpecFacts.state`.

use std::collections::{HashMap, HashSet};

use time::{Duration, OffsetDateTime};

use super::model::{Line, ProductNext};
use crate::pod_stats::{SpecFacts, SpecState};

/// How far back the merged list looks when a product has no release.
const NO_RELEASE_DAYS: i64 = 14;

/// The newest release tag of a product.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ReleaseTag {
    pub name: String,
    pub commit_time: OffsetDateTime,
}

/// What git says about the branch of an in-progress spec.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Branch {
    /// The spec has no worktree branch.
    None,
    /// The spec has a branch and git could not count its commits.
    Unknown,
    /// The branch commits over `origin/<base>`.
    Commits(u32),
}

/// What the pod folder and git say about one product.
pub(super) struct Facts<'a> {
    pub specs: &'a [SpecFacts],
    /// Spec id to title.
    pub titles: &'a HashMap<String, String>,
    /// The ids that have a parked handoff file.
    pub parked: &'a HashSet<String>,
    /// Spec id to its branch.
    pub branch_commits: &'a HashMap<String, Branch>,
    /// The merged commits that are ancestors of the release tag.
    pub in_release: &'a HashSet<String>,
    pub release: Option<ReleaseTag>,
    /// The time of the product's first merged file.
    pub first_merged_file: Option<OffsetDateTime>,
    /// The ids whose first line is the waiting line.
    pub awaiting: &'a HashSet<String>,
    pub now: OffsetDateTime,
    /// The commits after the release that no listed spec holds.
    pub other_commits: u32,
}

/// Builds the lists of the product `name`. Each list is sorted by id.
pub(super) fn classify(name: &str, facts: &Facts<'_>) -> ProductNext {
    let mut next = ProductNext {
        name: name.to_string(),
        release: facts
            .release
            .as_ref()
            .map(|tag| tag.name.clone())
            .unwrap_or_default(),
        other_commits: facts.other_commits,
        ..Default::default()
    };
    let line = |id: &str, commits: Option<u32>| Line {
        id: id.to_string(),
        title: facts.titles.get(id).cloned().unwrap_or_default(),
        commits,
    };
    for spec in facts.specs {
        let id = spec.id.as_str();
        // A parked spec is only in the parked list, whatever its state.
        if facts.parked.contains(id) {
            continue;
        }
        match spec.state {
            SpecState::Completed if is_merged_since_release(spec, facts) => {
                next.merged.push(line(id, None));
                if let Some(merged_at) = spec.merged_at {
                    next.merged_at.push(merged_at);
                }
            }
            SpecState::InProgress => match facts.branch_commits.get(id).copied() {
                Some(Branch::Commits(commits)) => next.in_progress.push(line(id, Some(commits))),
                Some(Branch::Unknown) => next.in_progress.push(line(id, None)),
                Some(Branch::None) | None => next.not_clear.push(line(id, None)),
            },
            SpecState::Approved if is_current_approval(spec, facts) => {
                next.approved.push(line(id, None));
            }
            SpecState::Open if facts.awaiting.contains(id) => next.awaiting.push(line(id, None)),
            _ => {}
        }
    }
    next.parked = facts.parked.iter().map(|id| line(id, None)).collect();
    for list in [
        &mut next.merged,
        &mut next.in_progress,
        &mut next.approved,
        &mut next.parked,
        &mut next.awaiting,
        &mut next.not_clear,
    ] {
        list.sort_by(|a, b| a.id.cmp(&b.id));
    }
    next
}

/// A completed spec is merged since the release when its commit is not in the tag. A
/// spec with no commit uses its merge time against the commit time of the tag. A product
/// with no release uses the last 14 days.
fn is_merged_since_release(spec: &SpecFacts, facts: &Facts<'_>) -> bool {
    let Some(tag) = &facts.release else {
        let from = facts.now - Duration::days(NO_RELEASE_DAYS);
        return spec.merged_at.is_some_and(|merged_at| merged_at >= from);
    };
    match &spec.merged_commit {
        Some(commit) => !facts.in_release.contains(commit),
        None => spec
            .merged_at
            .is_some_and(|merged_at| merged_at > tag.commit_time),
    }
}

/// An approval older than the first merged file is history. A product with no merged file
/// has no history.
fn is_current_approval(spec: &SpecFacts, facts: &Facts<'_>) -> bool {
    match facts.first_merged_file {
        Some(first) => spec
            .approved_at
            .is_some_and(|approved_at| approved_at >= first),
        None => true,
    }
}

#[cfg(test)]
#[path = "classify_tests.rs"]
mod classify_tests;
