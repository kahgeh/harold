use std::collections::HashMap;
use std::path::{Path, PathBuf};

use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, UtcOffset};

use super::*;
use crate::pod_stats::{
    AGENT_PANE, GitOutput, Sources, SpecState, read_history, read_specs, slots,
};
use crate::tools::tests::TempFolder;

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const NOW: &str = "2026-11-03T00:00:00Z";
const TAG_TIME: &str = "2026-10-20T00:00:00Z";
const MERGE_TIME: &str = "2026-11-01T00:00:00Z";

fn time(text: &str) -> OffsetDateTime {
    OffsetDateTime::parse(text, &Rfc3339).expect("a valid time")
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().expect("a parent folder")).expect("create the folder");
    std::fs::write(path, text).expect("write the file");
}

/// A `Git` that answers from a map. An argument list that the map lacks fails like a git
/// command that exits with an error. `None` in the map is a git call with no answer.
#[derive(Default)]
pub(in crate::tools::release) struct FakeGit {
    answers: HashMap<Vec<String>, Option<(bool, String)>>,
}

impl FakeGit {
    pub(in crate::tools::release) fn answer(
        mut self,
        args: &[&str],
        success: bool,
        stdout: &str,
    ) -> Self {
        let key = args.iter().map(|arg| arg.to_string()).collect();
        self.answers
            .insert(key, Some((success, stdout.to_string())));
        self
    }

    pub(in crate::tools::release) fn no_answer(mut self, args: &[&str]) -> Self {
        let key = args.iter().map(|arg| arg.to_string()).collect();
        self.answers.insert(key, None);
        self
    }
}

impl Git for FakeGit {
    async fn run(&self, _repo: &Path, args: &[&str]) -> Option<GitOutput> {
        let key: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
        let (success, stdout) = match self.answers.get(&key) {
            Some(answer) => answer.clone()?,
            None => (false, String::new()),
        };
        Some(GitOutput { success, stdout })
    }
}

const DESCRIBE: [&str; 6] = [
    "describe",
    "--tags",
    "--abbrev=0",
    "--match",
    "v[0-9]*",
    "origin/main",
];
const WORKTREES: [&str; 3] = ["worktree", "list", "--porcelain"];
const REFLOG: [&str; 5] = [
    "reflog",
    "show",
    "--date=unix",
    "--format=%H%x09%gd%x09%gs",
    "refs/heads/main",
];

/// A fake product: a pod folder in a temporary folder and the answers git gives.
pub(in crate::tools::release) struct Fixture {
    folder: TempFolder,
    pub(in crate::tools::release) repo: PathBuf,
    /// The `worktree list` entries after the base checkout: (folder name, branch).
    worktrees: Vec<(String, String)>,
}

impl Fixture {
    pub(in crate::tools::release) fn new() -> Self {
        let folder = TempFolder::new();
        let root = std::fs::canonicalize(&folder.0).expect("canonical folder");
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).expect("create the repo folder");
        Self {
            folder,
            repo,
            worktrees: Vec::new(),
        }
    }

    fn product(&self) -> Product {
        Product {
            name: "demo".into(),
            repo: self.repo.clone(),
            base: "main".into(),
            slots: 3,
            paused: false,
        }
    }

    fn pod(&self) -> PathBuf {
        self.repo.join(crate::pod_stats::POD_FOLDER)
    }

    pub(in crate::tools::release) fn spec(&self, id: &str, text: &str) {
        write(&self.pod().join(format!("specs/{id}.md")), text);
    }

    fn approve(&self, id: &str) {
        write(&self.pod().join(format!("approvals/{id}.md")), "approved\n");
    }

    fn start(&self, id: &str) {
        write(
            &self.pod().join(format!("started/{id}.md")),
            &format!("# Started: {id}\n\nstarted: 2026-10-25T00:00:00Z\n"),
        );
    }

    pub(in crate::tools::release) fn merge(&self, id: &str, commit: &str) {
        write(
            &self.pod().join(format!("merged/{id}.md")),
            &format!("# Merged: {id}\n\nmerged: {MERGE_TIME}\ncommit: {commit}\n"),
        );
    }

    fn park(&self, id: &str) {
        write(&self.pod().join(format!("parked/{id}-handoff.md")), "x\n");
    }

    /// A busy worktree `<id>` on `branch`.
    fn worktree(&mut self, id: &str, branch: &str) {
        let path = self.folder.0.join(id);
        write(&path.join(AGENT_PANE), "pane\n");
        self.worktrees.push((id.into(), branch.into()));
    }

    /// The answers of a product whose base branch has no release.
    pub(in crate::tools::release) fn git(&self) -> FakeGit {
        let mut listing = format!(
            "worktree {}\nHEAD {COMMIT}\nbranch refs/heads/main\n\n",
            self.repo.display()
        );
        for (id, branch) in &self.worktrees {
            let path = std::fs::canonicalize(self.folder.0.join(id)).expect("canonical worktree");
            listing += &format!(
                "worktree {}\nHEAD {COMMIT}\nbranch refs/heads/{branch}\n\n",
                path.display()
            );
        }
        FakeGit::default()
            .answer(&WORKTREES, true, &listing)
            .answer(&["rev-parse", "--verify", "refs/heads/main"], true, COMMIT)
            .answer(&REFLOG, true, "")
    }

    /// The answers of a product whose newest release is `v1.0.0`, tagged on `TAG_TIME`.
    fn git_with_tag(&self) -> FakeGit {
        let unix = time(TAG_TIME).unix_timestamp().to_string();
        self.git().answer(&DESCRIBE, true, "v1.0.0\n").answer(
            &["log", "-1", "--format=%ct", "v1.0.0"],
            true,
            &unix,
        )
    }

    async fn next(&self, git: &FakeGit) -> ProductNext {
        next_for(git, &self.product(), time(NOW), UtcOffset::UTC).await
    }
}

fn ids(lines: &[crate::tools::release::model::Line]) -> Vec<&str> {
    lines.iter().map(|line| line.id.as_str()).collect()
}

fn ancestor(commit: &str) -> [&str; 4] {
    ["merge-base", "--is-ancestor", commit, "v1.0.0"]
}

/// The one call that lists the merged commits that the tag does not reach.
fn unreached(commits: &[&str]) -> Vec<String> {
    let mut args: Vec<String> = ["rev-list", "^v1.0.0"].map(String::from).into();
    args.extend(commits.iter().map(|commit| commit.to_string()));
    args
}

/// The count of commits that the tag and the listed commits do not reach.
fn others(commits: &[&str]) -> Vec<String> {
    let mut args: Vec<String> = ["rev-list", "--count", "origin/main", "^v1.0.0"]
        .map(String::from)
        .into();
    args.extend(commits.iter().map(|commit| format!("^{commit}")));
    args
}

fn strs(args: &[String]) -> Vec<&str> {
    args.iter().map(String::as_str).collect()
}

#[tokio::test]
async fn tag_that_is_not_v_star_is_not_a_release() {
    let fixture = Fixture::new();
    fixture.spec("alpha", "# alpha: First\n");
    fixture.merge("alpha", COMMIT);
    // `release-9` does not match `v[0-9]*`, so git finds no tag and fails.
    let git = fixture.git().answer(&DESCRIBE, false, "");

    let next = fixture.next(&git).await;

    assert_eq!(next.release, "");
    assert_eq!(next.unavailable, None);
    assert_eq!(ids(&next.merged), ["alpha"]);
    assert_eq!(next.merged[0].title, "First");
    assert_eq!(next.other_commits, 0);
}

#[tokio::test]
async fn merged_commit_inside_the_tag_is_not_listed() {
    let fixture = Fixture::new();
    fixture.spec("alpha", "# alpha: First\n");
    fixture.merge("alpha", COMMIT);
    // The tag reaches the commit, so the call prints nothing.
    let git = fixture
        .git_with_tag()
        .answer(&strs(&unreached(&[COMMIT])), true, "")
        .answer(&strs(&others(&[])), true, "0\n");

    let next = fixture.next(&git).await;

    assert_eq!(next.release, "v1.0.0");
    assert!(next.merged.is_empty(), "{next:?}");
    assert_eq!(next.other_commits, 0);
}

#[tokio::test]
async fn merged_commit_after_the_tag_is_listed() {
    let fixture = Fixture::new();
    fixture.spec("alpha", "# alpha: First\n");
    fixture.merge("alpha", COMMIT);
    let git = fixture
        .git_with_tag()
        .answer(&strs(&unreached(&[COMMIT])), true, &format!("{COMMIT}\n"))
        .answer(&strs(&others(&[COMMIT])), true, "0\n");

    let next = fixture.next(&git).await;

    assert_eq!(ids(&next.merged), ["alpha"]);
    // A spec that merged as several commits leaves no other commit.
    assert_eq!(next.other_commits, 0);
}

#[tokio::test]
async fn a_direct_commit_after_the_specs_is_one_other_commit() {
    let fixture = Fixture::new();
    fixture.spec("alpha", "# alpha: First\n");
    fixture.merge("alpha", COMMIT);
    let git = fixture
        .git_with_tag()
        .answer(&strs(&unreached(&[COMMIT])), true, &format!("{COMMIT}\n"))
        .answer(&strs(&others(&[COMMIT])), true, "1\n");

    assert_eq!(fixture.next(&git).await.other_commits, 1);
}

#[tokio::test]
async fn an_unreached_commit_matches_by_prefix() {
    let fixture = Fixture::new();
    fixture.spec("alpha", "# alpha: First\n");
    fixture.merge("alpha", COMMIT);
    // Git prints a longer hash than the merged file holds.
    let long = format!("{COMMIT}{}", "0".repeat(24));
    let git = fixture
        .git_with_tag()
        .answer(&strs(&unreached(&[COMMIT])), true, &format!("{long}\n"))
        .answer(&strs(&others(&[COMMIT])), true, "0\n");

    assert_eq!(ids(&fixture.next(&git).await.merged), ["alpha"]);
}

#[tokio::test]
async fn ancestor_hashes_in_the_batch_output_change_nothing() {
    let fixture = Fixture::new();
    fixture.spec("alpha", "# alpha: First\n");
    fixture.merge("alpha", COMMIT);
    fixture.spec("beta", "# beta: Second\n");
    let beta = "fedcba9876543210fedcba9876543210fedcba98";
    fixture.merge("beta", beta);
    // Git prints every commit after the tag in the history of the two commits, not only
    // the two commits. Here `beta` is in the tag and `alpha` is after it.
    let other = "1111111111111111111111111111111111111111";
    let git = fixture
        .git_with_tag()
        .answer(
            &strs(&unreached(&[COMMIT, beta])),
            true,
            &format!("{COMMIT}\n{other}\n\n"),
        )
        .answer(&strs(&others(&[COMMIT])), true, "0\n");

    let next = fixture.next(&git).await;

    assert_eq!(ids(&next.merged), ["alpha"]);
}

#[tokio::test]
async fn a_branch_with_no_commits_yet_is_a_known_zero() {
    let mut fixture = Fixture::new();
    fixture.spec("alpha", "# alpha: First\n");
    fixture.start("alpha");
    fixture.worktree("alpha", "alpha-work");
    let git = fixture.git().answer(
        &["rev-list", "--count", "origin/main..refs/heads/alpha-work"],
        true,
        "0\n",
    );

    let next = fixture.next(&git).await;

    assert_eq!(next.in_progress[0].commits, Some(0));
}

#[tokio::test]
async fn a_failed_batch_call_falls_back_to_one_call_per_commit() {
    let fixture = Fixture::new();
    fixture.spec("alpha", "# alpha: First\n");
    fixture.merge("alpha", COMMIT);
    fixture.spec("beta", "# beta: Second\n");
    fixture.merge("beta", "fedcba9876543210fedcba9876543210fedcba98");
    let beta = "fedcba9876543210fedcba9876543210fedcba98";
    let git = fixture
        .git_with_tag()
        .answer(&strs(&unreached(&[COMMIT, beta])), false, "")
        .answer(&ancestor(COMMIT), true, "")
        .answer(&ancestor(beta), false, "")
        .answer(&strs(&others(&[beta])), true, "0\n");

    let next = fixture.next(&git).await;

    assert_eq!(ids(&next.merged), ["beta"]);
    assert_eq!(next.unavailable, None);
}

#[tokio::test]
async fn other_commits_stay_zero_when_the_count_fails() {
    let fixture = Fixture::new();
    fixture.spec("alpha", "# alpha: First\n");
    fixture.merge("alpha", COMMIT);
    let git =
        fixture
            .git_with_tag()
            .answer(&strs(&unreached(&[COMMIT])), true, &format!("{COMMIT}\n"));

    let next = fixture.next(&git).await;

    assert_eq!(ids(&next.merged), ["alpha"]);
    assert_eq!(next.other_commits, 0);
}

#[tokio::test]
async fn started_spec_with_no_branch_is_not_clear() {
    let fixture = Fixture::new();
    fixture.spec("alpha", "# alpha: First\n");
    fixture.start("alpha");
    let git = fixture.git();

    let next = fixture.next(&git).await;

    assert_eq!(ids(&next.not_clear), ["alpha"]);
    assert!(next.in_progress.is_empty());
}

#[tokio::test]
async fn started_spec_with_a_branch_counts_its_commits() {
    let mut fixture = Fixture::new();
    fixture.spec("alpha", "# alpha: First\n");
    fixture.start("alpha");
    fixture.worktree("alpha", "alpha-work");
    let git = fixture.git().answer(
        &["rev-list", "--count", "origin/main..refs/heads/alpha-work"],
        true,
        "4\n",
    );

    let next = fixture.next(&git).await;

    assert_eq!(ids(&next.in_progress), ["alpha"]);
    assert_eq!(next.in_progress[0].commits, Some(4));
    assert!(next.not_clear.is_empty());
}

#[tokio::test]
async fn a_branch_whose_count_fails_stays_in_progress() {
    let mut fixture = Fixture::new();
    fixture.spec("alpha", "# alpha: First\n");
    fixture.start("alpha");
    fixture.worktree("alpha", "alpha-work");
    // The product has no `origin/main`, so the count fails.
    let git = fixture.git();

    let next = fixture.next(&git).await;

    assert_eq!(ids(&next.in_progress), ["alpha"]);
    assert_eq!(next.in_progress[0].commits, None);
    assert!(next.not_clear.is_empty());
}

#[tokio::test]
async fn parked_handoff_moves_the_spec_to_parked() {
    let fixture = Fixture::new();
    fixture.spec("alpha", "# alpha: First\n");
    fixture.approve("alpha");
    fixture.park("alpha");
    let git = fixture.git();

    let next = fixture.next(&git).await;

    assert_eq!(ids(&next.parked), ["alpha"]);
    assert_eq!(next.parked[0].title, "First");
    assert!(next.approved.is_empty());
}

#[tokio::test]
async fn product_without_origin_or_tag_prints_no_release_lists() {
    let fixture = Fixture::new();
    fixture.spec("alpha", "# alpha: First\n");
    // Every git call about `origin/main` and about a tag fails.
    let git = fixture.git();

    let next = fixture.next(&git).await;

    assert_eq!(next.release, "");
    assert_eq!(next.other_commits, 0);
    assert_eq!(next.unavailable, None);
    assert!(next.merged.is_empty() && next.in_progress.is_empty());
}

#[tokio::test]
async fn a_git_call_with_no_answer_marks_the_product_unavailable() {
    let fixture = Fixture::new();
    fixture.spec("alpha", "# alpha: First\n");
    fixture.approve("alpha");
    let git = fixture.git().no_answer(&DESCRIBE);

    let next = fixture.next(&git).await;

    assert_eq!(next.unavailable, Some("git gave no answer"));
    assert_eq!(next.name, "demo");
    assert!(
        next.approved.is_empty() && next.merged.is_empty(),
        "{next:?}"
    );
}

#[tokio::test]
async fn each_spec_list_agrees_with_read_specs_state() {
    let mut fixture = Fixture::new();
    fixture.spec("done", "# done: Done\n");
    fixture.merge("done", COMMIT);
    fixture.spec("building", "# building: Building\n");
    fixture.start("building");
    fixture.worktree("building", "building-work");
    fixture.spec("lost", "# lost: Lost\n");
    fixture.start("lost");
    fixture.spec("ready", "# ready: Ready\n");
    fixture.approve("ready");
    fixture.spec(
        "waiting",
        "Waiting for your approval.\n\n# waiting: Waiting\n",
    );
    let git = fixture.git().answer(
        &[
            "rev-list",
            "--count",
            "origin/main..refs/heads/building-work",
        ],
        true,
        "2\n",
    );

    let next = fixture.next(&git).await;

    // The same sources that `next_for` builds.
    let product = fixture.product();
    let history = read_history(&git, &product).await.expect("history");
    let slots = slots(&git, &product).await.expect("slots");
    let busy = slots
        .busy_worktrees
        .iter()
        .filter_map(|worktree| {
            let created =
                crate::pod_stats::file_time(&worktree.path.join(AGENT_PANE), UtcOffset::UTC)?;
            Some((worktree.name.clone(), created))
        })
        .collect();
    let read = read_specs(
        &fixture.pod(),
        &Sources {
            busy: &busy,
            reflog: &history.reflog,
            gone: &HashMap::new(),
            now: time(NOW),
            offset: UtcOffset::UTC,
        },
    )
    .expect("specs");
    let state = |id: &str| {
        read.specs
            .iter()
            .find(|spec| spec.id == id)
            .map(|spec| spec.state)
    };

    let needs = [
        (&next.merged, SpecState::Completed),
        (&next.in_progress, SpecState::InProgress),
        (&next.not_clear, SpecState::InProgress),
        (&next.approved, SpecState::Approved),
        (&next.awaiting, SpecState::Open),
    ];
    for (list, wanted) in needs {
        assert!(!list.is_empty(), "a list has no spec: {next:?}");
        for line in list {
            assert_eq!(state(&line.id), Some(wanted), "{}", line.id);
        }
    }
    assert_eq!(ids(&next.merged), ["done"]);
    assert_eq!(ids(&next.in_progress), ["building"]);
    assert_eq!(ids(&next.not_clear), ["lost"]);
    assert_eq!(ids(&next.approved), ["ready"]);
    assert_eq!(ids(&next.awaiting), ["waiting"]);
}

#[tokio::test]
async fn a_byte_order_mark_before_the_waiting_line_still_awaits_approval() {
    let mut fixture = Fixture::new();
    fixture.spec(
        "waiting",
        "\u{feff}Waiting for your approval.\n\n# waiting: Waiting\n",
    );
    let next = fixture.next(&fixture.git()).await;
    assert_eq!(ids(&next.awaiting), ["waiting"]);
}

#[test]
fn the_release_tool_has_no_copy_of_the_waiting_line() {
    let source = include_str!("gather.rs");
    assert!(!source.contains("WAITING_LINE"));
    assert!(!source.contains("Waiting for your approval."));
}
