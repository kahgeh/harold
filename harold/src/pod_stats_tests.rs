use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use time::{OffsetDateTime, UtcOffset};
use tokio::sync::watch;

use super::*;
use crate::outbound::fake::CapturedLogs;
use crate::telemetry::test_support;

// --- fake pods ---

/// A temporary folder holding fake product checkouts, removed when dropped.
struct Pod {
    root: PathBuf,
    /// A product name no other test uses, so metrics can be told apart.
    name: String,
}

impl Pod {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("harold-pod-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("create the pod folder");
        // Canonical, so paths compare with what git reports (on macOS /var is /private/var).
        let root = std::fs::canonicalize(&root).expect("canonical pod folder");
        let name = format!("pod-test-{}", uuid::Uuid::new_v4());
        Self { root, name }
    }

    /// A repository `<root>/<folder>` on branch `main` with one commit of `f` (5 lines).
    fn repo(&self, folder: &str) -> PathBuf {
        let repo = self.root.join(folder);
        git(&self.root, &["init", "-q", "-b", "main", folder]);
        write(&repo.join("f"), "1\n2\n3\n4\n5\n");
        git(&repo, &["add", "f"]);
        git(&repo, &["commit", "-q", "-m", "init"]);
        repo
    }

    fn product(&self, repo: &Path, slots: u32) -> Product {
        Product {
            name: self.name.clone(),
            repo: repo.to_path_buf(),
            base: "main".into(),
            slots,
        }
    }

    /// A worktree `<root>/<folder>` of `repo` on a new branch, holding the agent-pane file
    /// when `busy`.
    fn worktree(&self, repo: &Path, folder: &str, busy: bool) -> PathBuf {
        let path = self.root.join(folder);
        git(
            repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                folder,
                path.to_str().unwrap(),
            ],
        );
        if busy {
            write(&path.join(".superpowers/baton/agent-pane"), "%7\n");
        }
        path
    }
}

impl Drop for Pod {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// Runs test-setup `git` in `dir`, away from the user's own git configuration, and
/// returns its trimmed output.
fn git(dir: &Path, args: &[&str]) -> String {
    git_with(dir, &[], args)
}

/// `git` with the environment variables `env` set as well.
fn git_with(dir: &Path, env: &[(&str, &str)], args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(dir)
        .envs(env.iter().copied())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// Makes branch `branch` from `main` with `f` replaced by `text`, committed, and
/// fast-forwards `main` to it, as baton's merge does.
fn merge_branch(repo: &Path, branch: &str, text: &str) {
    git(repo, &["checkout", "-q", "-b", branch]);
    write(&repo.join("f"), text);
    git(repo, &["commit", "-q", "-a", "-m", branch]);
    git(repo, &["checkout", "-q", "main"]);
    git(repo, &["merge", "-q", "--ff-only", branch]);
}

fn registry(pod: &Pod, text: &str) -> PathBuf {
    let path = pod.root.join("products.toml");
    write(&path, text);
    path
}

fn stats(registry: PathBuf) -> (PodStats<SystemGit>, Arc<Mutex<PodSnapshot>>) {
    let snapshot = Arc::new(Mutex::new(PodSnapshot::default()));
    (
        PodStats::new(registry, SystemGit, Arc::clone(&snapshot), UtcOffset::UTC),
        snapshot,
    )
}

// --- the registry ---

#[test]
fn the_registry_reads_products_and_defaults_slots_to_three() {
    let pod = Pod::new();
    let path = registry(
        &pod,
        "[products.a]\nrepo = \"~/x\"\nbase = \"main\"\nslots = 1\ntest = \"cargo test\"\nmatch = [\"a\"]\n\n\
         [products.b]\nrepo = \"/y\"\nbase = \"trunk\"\nnotes = \"n\"\n",
    );

    let products = read_registry(&path).unwrap();

    let home = std::env::var("HOME").unwrap();
    assert_eq!(
        products,
        vec![
            Product {
                name: "a".into(),
                repo: PathBuf::from(format!("{home}/x")),
                base: "main".into(),
                slots: 1,
            },
            Product {
                name: "b".into(),
                repo: PathBuf::from("/y"),
                base: "trunk".into(),
                slots: 3,
            },
        ]
    );
}

#[test]
fn a_missing_or_malformed_registry_is_an_error() {
    let pod = Pod::new();
    assert!(read_registry(&pod.root.join("absent.toml")).is_err());
    let path = registry(&pod, "[products.a]\nbase = \"main\"\n");
    assert!(read_registry(&path).is_err());
}

// --- slots ---

#[tokio::test]
async fn a_worktree_with_an_agent_pane_file_is_busy_and_others_are_not() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    // The base checkout holding the file still is not a slot.
    write(&repo.join(".superpowers/baton/agent-pane"), "%1\n");
    let a = pod.worktree(&repo, "a", true);
    pod.worktree(&repo, "b", false);

    let slots = slots(&SystemGit, &pod.product(&repo, 3)).await.unwrap();

    assert_eq!(slots.busy, 1);
    assert_eq!(slots.free, 2);
    let head = git(&a, &["rev-parse", "HEAD"]);
    assert_eq!(
        slots.busy_worktrees,
        vec![BusyWorktree {
            name: "a".to_owned(),
            path: a,
            head: Some(head),
        }]
    );
}

#[tokio::test]
async fn free_slots_never_go_below_zero() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    pod.worktree(&repo, "a", true);
    pod.worktree(&repo, "b", true);

    let slots = slots(&SystemGit, &pod.product(&repo, 1)).await.unwrap();

    assert_eq!((slots.busy, slots.free), (2, 0));
}

// --- merges ---

/// Reads the product's history and observes it, as a poll does.
async fn observe(tips: &mut Tips, product: &Product) -> Result<TipMove, &'static str> {
    let history = read_history(&SystemGit, product).await?;
    tips.observe(&SystemGit, product, &history).await
}

#[test]
fn a_reflog_time_comes_from_its_selector() {
    assert_eq!(reflog_time("main@{1791989197}"), Some(1_791_989_197));
    assert_eq!(reflog_time("refs/heads/main@{12}"), Some(12));
    assert_eq!(reflog_time("main"), None);
    assert_eq!(reflog_time("main@{0}x"), None);
}

#[tokio::test]
async fn start_up_counts_no_merge() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    let mut tips = Tips::default();

    let moved = observe(&mut tips, &pod.product(&repo, 3)).await;

    assert_eq!(moved, Ok(TipMove::Unchanged));
}

#[tokio::test]
async fn an_unmoved_tip_counts_no_merge() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    let product = pod.product(&repo, 3);
    let mut tips = Tips::default();
    observe(&mut tips, &product).await.unwrap();

    assert_eq!(observe(&mut tips, &product).await, Ok(TipMove::Unchanged));
}

#[tokio::test]
async fn a_fast_forward_counts_one_merge_with_its_lines() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    let product = pod.product(&repo, 3);
    let mut tips = Tips::default();
    observe(&mut tips, &product).await.unwrap();

    // Lines 1, 2, 4 and 5 kept, 3 removed, three added.
    merge_branch(&repo, "spec-x", "1\n2\n4\n5\nx\ny\nz\n");

    assert_eq!(
        observe(&mut tips, &product).await,
        Ok(TipMove::Merges(vec![Merge {
            spec_id: Some("spec-x".into()),
            added: 3,
            removed: 1,
        }]))
    );
}

#[tokio::test]
async fn two_merges_between_polls_count_as_two() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    let product = pod.product(&repo, 3);
    let mut tips = Tips::default();
    observe(&mut tips, &product).await.unwrap();

    merge_branch(&repo, "spec-x", "1\n2\n3\n4\n5\n6\n");
    merge_branch(&repo, "spec-y", "1\n2\n3\n4\n6\n");

    assert_eq!(
        observe(&mut tips, &product).await,
        Ok(TipMove::Merges(vec![
            Merge {
                spec_id: Some("spec-x".into()),
                added: 1,
                removed: 0,
            },
            Merge {
                spec_id: Some("spec-y".into()),
                added: 0,
                removed: 1,
            },
        ]))
    );
}

#[tokio::test]
async fn a_move_without_a_reflog_counts_one_merge_without_a_spec() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    let product = pod.product(&repo, 3);
    git(&repo, &["config", "core.logAllRefUpdates", "false"]);
    std::fs::remove_file(repo.join(".git/logs/refs/heads/main")).unwrap();
    let mut tips = Tips::default();
    observe(&mut tips, &product).await.unwrap();

    merge_branch(&repo, "spec-x", "1\n2\n3\n4\n5\n6\n");
    merge_branch(&repo, "spec-y", "1\n2\n3\n4\n5\n6\n7\n");

    assert_eq!(
        observe(&mut tips, &product).await,
        Ok(TipMove::Merges(vec![Merge {
            spec_id: None,
            added: 2,
            removed: 0,
        }]))
    );
}

#[tokio::test]
async fn a_rewrite_counts_nothing_and_is_logged() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    let product = pod.product(&repo, 3);
    let mut tips = Tips::default();
    observe(&mut tips, &product).await.unwrap();
    let tree = git(&repo, &["rev-parse", "HEAD^{tree}"]);
    let unrelated = git(&repo, &["commit-tree", &tree, "-m", "unrelated"]);
    git(&repo, &["update-ref", "refs/heads/main", &unrelated]);
    let logs = CapturedLogs::start();

    let moved = observe(&mut tips, &product).await;

    assert_eq!(moved, Ok(TipMove::Rewritten));
    let text = logs.text();
    assert!(
        text.contains(&format!(
            "base branch rewritten; not counted pod.product={}",
            pod.name
        )),
        "{text}"
    );
    // The new tip is known: the next poll counts nothing more.
    assert_eq!(observe(&mut tips, &product).await, Ok(TipMove::Unchanged));
}

#[test]
fn a_spec_id_comes_from_a_merge_subject() {
    assert_eq!(
        spec_id("merge chief-voice: Fast-forward"),
        Some("chief-voice".into())
    );
    assert_eq!(
        spec_id("merge reply-ownership: Merge made by the 'ort' strategy."),
        Some("reply-ownership".into())
    );
    assert_eq!(spec_id("commit: fix the build"), None);
    assert_eq!(spec_id("merge : Fast-forward"), None);
    assert_eq!(spec_id("merge spec-x"), None);
}

#[test]
fn lines_come_from_the_shortstat() {
    assert_eq!(
        shortstat_lines(" 2 files changed, 30 insertions(+), 4 deletions(-)\n"),
        (30, 4)
    );
    assert_eq!(shortstat_lines(" 1 file changed, 1 insertion(+)\n"), (1, 0));
    assert_eq!(shortstat_lines(" 1 file changed, 1 deletion(-)\n"), (0, 1));
    assert_eq!(shortstat_lines(""), (0, 0));
}

// --- polls ---

#[tokio::test]
async fn a_poll_records_slots_and_merges() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    pod.worktree(&repo, "a", true);
    let path = registry(
        &pod,
        &format!(
            "[products.{}]\nrepo = \"{}\"\nbase = \"main\"\nslots = 3\n",
            pod.name,
            repo.display()
        ),
    );
    let (mut stats, snapshot) = stats(path);
    stats.poll().await;
    merge_branch(&repo, "spec-x", "1\n2\n4\n5\nx\ny\nz\n");

    stats.poll().await;

    assert_eq!(
        snapshot.lock().unwrap().slots.get(&pod.name),
        Some(&SlotCounts { busy: 1, free: 2 })
    );
    let product = ("pod.product", pod.name.as_str());
    assert_eq!(
        test_support::counter_value("pod.merges", &[product, ("spec.id", "spec-x")]),
        Some(1)
    );
    assert_eq!(
        test_support::counter_value("pod.merged_lines", &[product, ("change", "added")]),
        Some(3)
    );
    assert_eq!(
        test_support::counter_value("pod.merged_lines", &[product, ("change", "removed")]),
        Some(1)
    );
}

#[tokio::test]
async fn a_missing_repo_skips_that_product_only() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    let absent = pod.root.join("absent");
    let path = registry(
        &pod,
        &format!(
            "[products.{name}-gone]\nrepo = \"{}\"\nbase = \"main\"\n\n\
             [products.{name}]\nrepo = \"{}\"\nbase = \"main\"\nslots = 2\n",
            absent.display(),
            repo.display(),
            name = pod.name,
        ),
    );
    let (mut stats, snapshot) = stats(path);
    let logs = CapturedLogs::start();

    stats.poll().await;
    stats.poll().await;

    let gone = format!("{}-gone", pod.name);
    assert_eq!(
        snapshot.lock().unwrap().slots.get(&pod.name),
        Some(&SlotCounts { busy: 0, free: 2 })
    );
    assert_eq!(snapshot.lock().unwrap().slots.get(&gone), None);
    let line = format!("pod facts unavailable pod.product={gone} reason=\"git failed\"");
    let text = logs.text();
    assert_eq!(text.matches(&line).count(), 1, "{text}");

    // Once it works it is counted, and a later failure is logged again.
    pod.repo("absent");
    stats.poll().await;
    assert_eq!(
        snapshot.lock().unwrap().slots.get(&gone),
        Some(&SlotCounts { busy: 0, free: 3 })
    );
    std::fs::remove_dir_all(&absent).unwrap();
    stats.poll().await;
    let text = logs.text();
    assert_eq!(text.matches(&line).count(), 2, "{text}");
}

#[tokio::test]
async fn an_unreadable_registry_is_logged_once() {
    let pod = Pod::new();
    let (mut stats, snapshot) = stats(pod.root.join("products.toml"));
    let logs = CapturedLogs::start();

    stats.poll().await;
    stats.poll().await;

    assert_eq!(*snapshot.lock().unwrap(), PodSnapshot::default());
    let text = logs.text();
    assert_eq!(
        text.matches("pod facts unavailable reason=\"registry unreadable\"")
            .count(),
        1,
        "{text}"
    );
}

/// A `git` that answers after 30 seconds.
struct Hanging;

impl Git for Hanging {
    async fn run(&self, _repo: &Path, _args: &[&str]) -> Option<GitOutput> {
        tokio::time::sleep(Duration::from_secs(30)).await;
        Some(GitOutput {
            success: true,
            stdout: String::new(),
        })
    }
}

#[tokio::test(start_paused = true)]
async fn a_git_that_hangs_is_given_up_after_five_seconds() {
    let started = tokio::time::Instant::now();

    let output = with_timeout(GIT_TIMEOUT, Hanging.run(Path::new("/nowhere"), &["status"])).await;

    assert!(output.is_none());
    assert_eq!(started.elapsed(), Duration::from_secs(5));
}

#[tokio::test]
async fn the_task_polls_at_start_reports_the_gauges_and_stops_at_shutdown() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    pod.worktree(&repo, "a", true);
    let path = registry(
        &pod,
        &format!(
            "[products.{}]\nrepo = \"{}\"\nbase = \"main\"\nslots = 3\n",
            pod.name,
            repo.display()
        ),
    );
    let (shutdown_tx, shutdown_rx) = watch::channel(());

    let handle = start(path, shutdown_rx);

    let snapshot = gauge_snapshot();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !snapshot.lock().unwrap().slots.contains_key(&pod.name) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the first poll did not run"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let product = [("pod.product", pod.name.as_str())];
    assert_eq!(
        test_support::gauge_value("pod.slots.busy", &product),
        Some(1)
    );
    assert_eq!(
        test_support::gauge_value("pod.slots.free", &product),
        Some(2)
    );

    drop(shutdown_tx);
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("the task stops at shutdown")
        .unwrap();
}

// --- specs ---

/// The time `text` (RFC 3339) names.
fn at(text: &str) -> OffsetDateTime {
    OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339).unwrap()
}

/// The creation time of the file at `path`, or its modification time where the file
/// system keeps no creation time.
fn created(path: &Path) -> OffsetDateTime {
    let metadata = std::fs::metadata(path).unwrap();
    OffsetDateTime::from(metadata.created().or_else(|_| metadata.modified()).unwrap())
}

fn by_id(specs: Vec<SpecFacts>) -> HashMap<String, SpecFacts> {
    specs
        .into_iter()
        .map(|spec| (spec.id.clone(), spec))
        .collect()
}

#[test]
fn each_state_comes_from_its_file_at_that_files_time() {
    let pod = Pod::new();
    let dir = pod.root.join("pod");
    for id in ["a", "b", "c", "d", "e", "f"] {
        write(&dir.join(format!("specs/{id}.md")), "# Spec\n");
    }
    for id in ["b", "c", "d"] {
        write(
            &dir.join(format!("specs/{id}.url")),
            "https://example.com\n",
        );
    }
    for id in ["c", "d"] {
        write(
            &dir.join(format!("approvals/{id}.md")),
            "# Approval\napproved: 2026-10-14\n",
        );
    }
    write(
        &dir.join("rejections.md"),
        "# Rejections\n\n- f, 2026-10-14, too big\n",
    );
    let t = at("2026-10-14T10:00:00Z");
    let t2 = at("2026-10-14T11:34:00Z");
    let building = HashMap::from([("d".to_owned(), t)]);
    let merged = HashMap::from([("e".to_owned(), t2)]);
    let offset = UtcOffset::from_hms(11, 0, 0).unwrap();

    let specs = by_id(read_specs(&dir, &building, &merged, offset).unwrap());

    let state = |id: &str| (specs[id].state, specs[id].state_at);
    assert_eq!(
        state("a"),
        (SpecState::Drafted, created(&dir.join("specs/a.md")))
    );
    assert_eq!(
        state("b"),
        (SpecState::Waiting, created(&dir.join("specs/b.url")))
    );
    assert_eq!(
        state("c"),
        (SpecState::Approved, created(&dir.join("approvals/c.md")))
    );
    assert_eq!(state("d"), (SpecState::Building, t));
    assert_eq!(state("e"), (SpecState::Merged, t2));
    assert_eq!(
        state("f"),
        (SpecState::Rejected, at("2026-10-14T00:00:00+11:00"))
    );
    assert_eq!(specs.len(), 6);
    assert_eq!(specs["d"].ready_at, Some(created(&dir.join("specs/d.url"))));
    assert_eq!(
        specs["d"].approved_at,
        Some(created(&dir.join("approvals/d.md")))
    );
    assert_eq!(specs["e"].merged_at, Some(t2));
}

#[test]
fn a_pod_without_a_specs_folder_has_no_specs() {
    let pod = Pod::new();

    let specs = read_specs(
        &pod.root.join("absent"),
        &HashMap::new(),
        &HashMap::new(),
        UtcOffset::UTC,
    );

    assert_eq!(specs, Ok(Vec::new()));
}

#[test]
fn a_merged_spec_stays_merged_though_its_approval_file_remains() {
    let pod = Pod::new();
    let dir = pod.root.join("pod");
    write(&dir.join("specs/g.md"), "# Spec\n");
    write(&dir.join("specs/g.url"), "https://example.com\n");
    write(&dir.join("approvals/g.md"), "approved: 2026-10-14\n");
    let merged_at = at("2026-10-14T11:34:00Z");
    let merged = HashMap::from([("g".to_owned(), merged_at)]);

    let specs = read_specs(&dir, &HashMap::new(), &merged, UtcOffset::UTC).unwrap();

    assert_eq!(specs[0].state, SpecState::Merged);
    assert_eq!(specs[0].state_at, merged_at);
}

#[test]
fn a_revision_keeps_ready_and_raises_revisions() {
    let pod = Pod::new();
    let dir = pod.root.join("pod");
    write(&dir.join("specs/b.md"), "# Spec\n");
    write(&dir.join("specs/b.url"), "https://example.com\n");
    let read = || read_specs(&dir, &HashMap::new(), &HashMap::new(), UtcOffset::UTC).unwrap();
    let before = read().remove(0);
    assert_eq!(before.revisions, 0);

    write(
        &dir.join("requests/b.md"),
        "# Requests\n\n## Revision 1, 2026-10-14: revise the drafted spec\ntext\n\n\
         ## Revision 2, 2026-10-15: shorter\n",
    );
    write(&dir.join("specs/b.md"), "# Spec, revised\n");
    let after = read().remove(0);

    assert_eq!(after.ready_at, before.ready_at);
    assert_eq!(after.state, SpecState::Waiting);
    assert_eq!(after.revisions, 2);
}

#[test]
fn revisions_count_the_revision_headings() {
    assert_eq!(
        revisions("# R\n## Revision 1, x\n## Revisions\n### Revision 2\n"),
        1
    );
    assert_eq!(revisions(""), 0);
}

#[test]
fn the_word_count_skips_drawn_lines() {
    assert_eq!(
        spec_words("# Title here\n<div class=\"x\">lots of words inside</div>\nthree more words\n"),
        6
    );
}

#[test]
fn a_rejection_counts_from_the_start_of_its_day() {
    let offset = UtcOffset::from_hms(11, 0, 0).unwrap();
    let rejected = rejections(
        "# Rejections\n\n- f, 2026-10-14, reason\n- bad line\n- h, 2026-13-01, no such month\n",
        offset,
    );
    assert_eq!(rejected["f"], at("2026-10-14T00:00:00+11:00"));
    assert_eq!(rejected.len(), 1);

    let west = UtcOffset::from_hms(-7, 0, 0).unwrap();
    let rejected = rejections("- f, 2026-10-14, reason\n", west);
    assert_eq!(rejected["f"], at("2026-10-14T00:00:00-07:00"));
    assert_eq!(rejected["f"], at("2026-10-14T07:00:00Z"));
}

#[test]
fn the_states_have_the_spec_names_and_later_wins() {
    let names: Vec<_> = SpecState::ALL.iter().map(|state| state.as_str()).collect();
    assert_eq!(
        names,
        [
            "drafted",
            "waiting_for_approval",
            "approved",
            "building",
            "rejected",
            "merged"
        ]
    );
    assert!(SpecState::Merged > SpecState::Rejected);
    assert!(SpecState::Rejected > SpecState::Building);
}

fn spec(id: &str, state: SpecState, state_at: &str) -> SpecFacts {
    SpecFacts {
        id: id.into(),
        state,
        state_at: at(state_at),
        ready_at: Some(at("2026-10-14T10:00:00Z")),
        approved_at: Some(at("2026-10-14T10:31:00Z")),
        merged_at: None,
        words: 120,
        revisions: 1,
    }
}

#[test]
fn start_up_reports_no_change() {
    let mut book = SpecBook::default();

    let changes = book.observe(
        "pod-test-start",
        &[spec("g", SpecState::Approved, "2026-10-14T10:31:00Z")],
    );

    assert_eq!(changes, Vec::new());
}

#[test]
fn a_spec_that_merges_records_both_phases() {
    test_support::install();
    let product = format!("pod-test-{}", uuid::Uuid::new_v4());
    let mut book = SpecBook::default();
    book.observe(
        &product,
        &[spec("g", SpecState::Approved, "2026-10-14T10:31:00Z")],
    );
    let logs = CapturedLogs::start();
    let mut merged = spec("g", SpecState::Merged, "2026-10-14T11:34:00Z");
    merged.merged_at = Some(at("2026-10-14T11:34:00Z"));

    let changes = book.observe(&product, &[merged]);

    assert_eq!(changes, vec![("g".to_owned(), SpecState::Merged)]);
    let wanted = |phase| {
        [
            ("pod.product", product.as_str()),
            ("phase", phase),
            ("spec.revisions", "1"),
        ]
    };
    assert_eq!(
        test_support::histogram_sum("pod.spec.lead_time", &wanted("waiting_for_user")),
        Some(1860.0)
    );
    assert_eq!(
        test_support::histogram_sum("pod.spec.lead_time", &wanted("build")),
        Some(3780.0)
    );
    let text = logs.text();
    assert!(
        text.contains(&format!(
            "INFO harold::pod: spec state changed pod.product={product} spec.id=g state=\"merged\" \
             spec.state_at=2026-10-14T11:34:00Z spec.words=120 spec.revisions=1"
        )),
        "{text}"
    );
    // The same state again is no change.
    assert_eq!(
        book.observe(
            &product,
            &[spec("g", SpecState::Merged, "2026-10-14T11:34:00Z")]
        ),
        Vec::new()
    );
}

#[test]
fn a_phase_with_a_missing_time_or_a_negative_length_is_not_recorded() {
    test_support::install();
    let product = format!("pod-test-{}", uuid::Uuid::new_v4());
    let mut book = SpecBook::default();
    book.observe(&product, &[]);
    let mut merged = spec("g", SpecState::Merged, "2026-10-14T09:00:00Z");
    merged.ready_at = None;
    // Merged before it was approved: the build phase would be negative.
    merged.merged_at = Some(at("2026-10-14T09:00:00Z"));

    let changes = book.observe(&product, &[merged]);

    assert_eq!(changes, vec![("g".to_owned(), SpecState::Merged)]);
    assert_eq!(
        test_support::histogram_sum("pod.spec.lead_time", &[("pod.product", product.as_str())]),
        None
    );
}

#[test]
fn a_change_short_of_merged_is_logged_without_a_lead_time() {
    test_support::install();
    let product = format!("pod-test-{}", uuid::Uuid::new_v4());
    let mut book = SpecBook::default();
    book.observe(
        &product,
        &[spec("g", SpecState::Waiting, "2026-10-14T10:00:00Z")],
    );

    let changes = book.observe(
        &product,
        &[
            spec("g", SpecState::Approved, "2026-10-14T10:31:00Z"),
            spec("n", SpecState::Drafted, "2026-10-14T10:40:00Z"),
        ],
    );

    assert_eq!(
        changes,
        vec![
            ("g".to_owned(), SpecState::Approved),
            ("n".to_owned(), SpecState::Drafted)
        ]
    );
    assert_eq!(
        test_support::histogram_sum("pod.spec.lead_time", &[("pod.product", product.as_str())]),
        None
    );
}

/// A registry naming `repo` as the pod's one product.
fn one_product(pod: &Pod, repo: &Path) -> PathBuf {
    registry(
        pod,
        &format!(
            "[products.{}]\nrepo = \"{}\"\nbase = \"main\"\nslots = 3\n",
            pod.name,
            repo.display()
        ),
    )
}

#[tokio::test]
async fn a_poll_reports_specs_by_state_and_their_words() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    let specs = repo.join(".superpowers/pod/specs");
    write(
        &specs.join("spec-x.md"),
        "# Spec x\n<svg>drawn</svg>\none two three\n",
    );
    write(&specs.join("spec-x.url"), "https://example.com\n");
    write(
        &repo.join(".superpowers/pod/approvals/spec-x.md"),
        "approved: 2026-10-14\n",
    );
    write(&specs.join("spec-y.md"), "# Spec y\n");
    let (mut stats, snapshot) = stats(one_product(&pod, &repo));
    stats.poll().await;
    let logs = CapturedLogs::start();

    merge_branch(&repo, "spec-x", "1\n2\n3\n4\n5\n6\n");
    stats.poll().await;

    let specs = by_id(snapshot.lock().unwrap().specs[&pod.name].clone());
    assert_eq!(specs["spec-x"].state, SpecState::Merged);
    assert_eq!(specs["spec-x"].words, 6);
    assert_eq!(specs["spec-y"].state, SpecState::Drafted);
    let text = logs.text();
    assert_eq!(text.matches("spec state changed").count(), 1, "{text}");
    assert!(
        text.contains(&format!(
            "spec state changed pod.product={} spec.id=spec-x state=\"merged\"",
            pod.name
        )),
        "{text}"
    );
}

/// A product checkout `main` whose base branch keeps no reflog.
fn repo_without_a_reflog(pod: &Pod) -> PathBuf {
    let repo = pod.repo("main");
    git(&repo, &["config", "core.logAllRefUpdates", "false"]);
    std::fs::remove_file(repo.join(".git/logs/refs/heads/main")).unwrap();
    repo
}

/// Commits `f` as `text` in the worktree at `path`.
fn commit(path: &Path, text: &str) {
    write(&path.join("f"), text);
    git(path, &["commit", "-q", "-a", "-m", "work"]);
}

fn remove_worktree(repo: &Path, path: &Path) {
    git(
        repo,
        &["worktree", "remove", "--force", path.to_str().unwrap()],
    );
}

const GONE_LINE: &str =
    "worktree removed without its commit on the base branch; spec not counted as merged";

#[tokio::test]
async fn a_worktree_gone_after_its_commit_merged_counts_as_merged_without_a_reflog() {
    let pod = Pod::new();
    let repo = repo_without_a_reflog(&pod);
    for id in ["h", "k"] {
        write(
            &repo.join(format!(".superpowers/pod/specs/{id}.md")),
            "# Spec\n",
        );
    }
    let h = pod.worktree(&repo, "h", true);
    let k = pod.worktree(&repo, "k", true);
    commit(&h, "1\n2\n3\n4\n5\nh\n");
    commit(&k, "1\n2\n3\n4\n5\nk\n");
    let (mut stats, snapshot) = stats(one_product(&pod, &repo));
    stats.poll().await;
    let building = by_id(snapshot.lock().unwrap().specs[&pod.name].clone());
    assert_eq!(building["h"].state, SpecState::Building);
    assert_eq!(
        building["h"].state_at,
        created(&h.join(".superpowers/baton/agent-pane"))
    );
    let logs = CapturedLogs::start();

    // h merges, then goes with its branch straight after, as baton's finish does; k goes
    // without merging.
    git(&repo, &["merge", "-q", "--ff-only", "h"]);
    let before = OffsetDateTime::now_utc();
    remove_worktree(&repo, &h);
    git(&repo, &["branch", "-d", "h"]);
    remove_worktree(&repo, &k);
    stats.poll().await;
    stats.poll().await;

    let specs = by_id(snapshot.lock().unwrap().specs[&pod.name].clone());
    assert_eq!(specs["h"].state, SpecState::Merged);
    assert!(specs["h"].state_at >= before);
    assert_eq!(specs["k"].state, SpecState::Drafted);
    let line = format!("{GONE_LINE} pod.product={} spec.id=k", pod.name);
    // Within the hour k is still asked about, and nothing is logged.
    let text = logs.text();
    assert_eq!(text.matches(&line).count(), 0, "{text}");

    // After an hour k is given up and logged once.
    for gone in stats.gone.get_mut(&pod.name).unwrap().values_mut() {
        gone.seen_at -= time::Duration::hours(2);
    }
    stats.poll().await;
    stats.poll().await;

    let text = logs.text();
    assert_eq!(text.matches(&line).count(), 1, "{text}");
    assert!(stats.gone[&pod.name].is_empty());
    let specs = by_id(snapshot.lock().unwrap().specs[&pod.name].clone());
    assert_eq!(specs["h"].state, SpecState::Merged);
    assert_eq!(specs["k"].state, SpecState::Drafted);
}

#[tokio::test]
async fn a_gone_worktree_is_asked_about_again_until_its_commit_merges() {
    let pod = Pod::new();
    let repo = repo_without_a_reflog(&pod);
    write(&repo.join(".superpowers/pod/specs/m.md"), "# Spec\n");
    let m = pod.worktree(&repo, "m", true);
    commit(&m, "1\n2\n3\n4\n5\nm\n");
    let (mut stats, snapshot) = stats(one_product(&pod, &repo));
    stats.poll().await;
    remove_worktree(&repo, &m);
    let before = OffsetDateTime::now_utc();
    stats.poll().await;
    let seen_by = OffsetDateTime::now_utc();
    let specs = by_id(snapshot.lock().unwrap().specs[&pod.name].clone());
    assert_eq!(specs["m"].state, SpecState::Drafted);

    // The merge lands a poll later: it counts from when the worktree was seen gone.
    git(&repo, &["merge", "-q", "--ff-only", "m"]);
    stats.poll().await;

    let specs = by_id(snapshot.lock().unwrap().specs[&pod.name].clone());
    assert_eq!(specs["m"].state, SpecState::Merged);
    assert!(specs["m"].state_at >= before && specs["m"].state_at <= seen_by);
}

#[tokio::test]
async fn a_spec_merged_by_the_reflog_counts_from_the_move_not_the_commit() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    write(
        &repo.join(".superpowers/pod/specs/spec-late.md"),
        "# Spec\n",
    );
    let (mut stats, snapshot) = stats(one_product(&pod, &repo));
    stats.poll().await;
    git(&repo, &["checkout", "-q", "-b", "spec-late"]);
    write(&repo.join("f"), "1\n2\n3\n4\n5\nlate\n");
    let old = "2020-01-01T00:00:00Z";
    git_with(
        &repo,
        &[("GIT_COMMITTER_DATE", old), ("GIT_AUTHOR_DATE", old)],
        &["commit", "-q", "-a", "-m", "late"],
    );
    git(&repo, &["checkout", "-q", "main"]);
    let before = OffsetDateTime::now_utc() - time::Duration::seconds(1);

    git(&repo, &["merge", "-q", "--ff-only", "spec-late"]);
    stats.poll().await;

    let specs = by_id(snapshot.lock().unwrap().specs[&pod.name].clone());
    assert_eq!(specs["spec-late"].state, SpecState::Merged);
    assert!(
        specs["spec-late"].state_at >= before,
        "{}",
        specs["spec-late"].state_at
    );
}

#[tokio::test]
async fn an_unreadable_specs_folder_waits_for_the_next_poll() {
    use std::os::unix::fs::PermissionsExt;

    let pod = Pod::new();
    let repo = pod.repo("main");
    let specs = repo.join(".superpowers/pod/specs");
    write(&specs.join("spec-u.md"), "# Spec\n");
    write(&specs.join("spec-u.url"), "https://example.com\n");
    let (mut stats, snapshot) = stats(one_product(&pod, &repo));
    stats.poll().await;
    let logs = CapturedLogs::start();

    std::fs::set_permissions(&specs, std::fs::Permissions::from_mode(0o000)).unwrap();
    stats.poll().await;
    std::fs::set_permissions(&specs, std::fs::Permissions::from_mode(0o755)).unwrap();
    let unreadable = snapshot.lock().unwrap().clone();
    stats.poll().await;

    assert_eq!(
        unreadable.slots.get(&pod.name),
        Some(&SlotCounts { busy: 0, free: 3 })
    );
    assert_eq!(unreadable.specs.get(&pod.name), None);
    let specs = by_id(snapshot.lock().unwrap().specs[&pod.name].clone());
    assert_eq!(specs["spec-u"].state, SpecState::Waiting);
    let text = logs.text();
    assert_eq!(
        text.matches(&format!(
            "pod merges and specs unavailable pod.product={} reason=\"pod folder unreadable\"",
            pod.name
        ))
        .count(),
        1,
        "{text}"
    );
    // The specs were not taken as gone and back again.
    assert_eq!(text.matches("spec state changed").count(), 0, "{text}");
}

#[test]
fn a_merged_spec_seen_again_records_its_lead_time_once() {
    test_support::install();
    let product = format!("pod-test-{}", uuid::Uuid::new_v4());
    let mut book = SpecBook::default();
    book.observe(
        &product,
        &[spec("g", SpecState::Approved, "2026-10-14T10:31:00Z")],
    );
    let mut merged = spec("g", SpecState::Merged, "2026-10-14T11:34:00Z");
    merged.merged_at = Some(at("2026-10-14T11:34:00Z"));
    book.observe(&product, std::slice::from_ref(&merged));

    book.observe(&product, &[]);
    let changes = book.observe(&product, &[merged]);

    assert_eq!(changes, vec![("g".to_owned(), SpecState::Merged)]);
    assert_eq!(
        test_support::histogram_sum(
            "pod.spec.lead_time",
            &[("pod.product", product.as_str()), ("phase", "build")]
        ),
        Some(3780.0)
    );
}

/// The `git` on the PATH, but every `git reflog` fails.
struct NoReflog;

impl Git for NoReflog {
    async fn run(&self, repo: &Path, args: &[&str]) -> Option<GitOutput> {
        if args.first() == Some(&"reflog") {
            return Some(GitOutput {
                success: false,
                stdout: String::new(),
            });
        }
        SystemGit.run(repo, args).await
    }
}

#[tokio::test]
async fn a_reflog_that_cannot_be_read_keeps_the_slots() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    pod.worktree(&repo, "a", true);
    write(&repo.join(".superpowers/pod/specs/a.md"), "# Spec\n");
    let snapshot = Arc::new(Mutex::new(PodSnapshot::default()));
    let mut stats = PodStats::new(
        one_product(&pod, &repo),
        NoReflog,
        Arc::clone(&snapshot),
        UtcOffset::UTC,
    );
    let logs = CapturedLogs::start();

    stats.poll().await;
    stats.poll().await;

    assert_eq!(
        snapshot.lock().unwrap().slots.get(&pod.name),
        Some(&SlotCounts { busy: 1, free: 2 })
    );
    assert_eq!(snapshot.lock().unwrap().specs.get(&pod.name), None);
    let text = logs.text();
    let line = format!(
        "pod merges and specs unavailable pod.product={} reason=\"git failed\"",
        pod.name
    );
    assert_eq!(text.matches(&line).count(), 1, "{text}");
    assert!(!text.contains("pod facts unavailable"), "{text}");
}

#[tokio::test]
async fn the_spec_gauges_report_every_state_and_each_specs_words() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    write(
        &repo.join(".superpowers/pod/specs/spec-z.md"),
        "one two three four\n",
    );
    let path = one_product(&pod, &repo);
    let (shutdown_tx, shutdown_rx) = watch::channel(());

    let handle = start(path, shutdown_rx);

    let snapshot = gauge_snapshot();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !snapshot.lock().unwrap().specs.contains_key(&pod.name) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the first poll did not run"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let count = |state| {
        test_support::gauge_value(
            "pod.specs",
            &[("pod.product", pod.name.as_str()), ("state", state)],
        )
    };
    assert_eq!(count("drafted"), Some(1));
    for state in [
        "waiting_for_approval",
        "approved",
        "building",
        "rejected",
        "merged",
    ] {
        assert_eq!(count(state), Some(0), "{state}");
    }
    assert_eq!(
        test_support::gauge_value(
            "pod.spec.words",
            &[
                ("pod.product", pod.name.as_str()),
                ("spec.id", "spec-z"),
                ("state", "drafted")
            ],
        ),
        Some(4)
    );

    drop(shutdown_tx);
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("the task stops at shutdown")
        .unwrap();
}
