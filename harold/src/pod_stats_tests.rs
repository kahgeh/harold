use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
    let output = Command::new("git")
        .current_dir(dir)
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
        PodStats::new(registry, SystemGit, Arc::clone(&snapshot)),
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
    assert_eq!(slots.busy_worktrees, vec![("a".to_owned(), a)]);
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

#[tokio::test]
async fn start_up_counts_no_merge() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    let mut tips = Tips::default();

    let moved = tips.observe(&SystemGit, &pod.product(&repo, 3)).await;

    assert_eq!(moved, Ok(TipMove::Unchanged));
}

#[tokio::test]
async fn an_unmoved_tip_counts_no_merge() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    let product = pod.product(&repo, 3);
    let mut tips = Tips::default();
    tips.observe(&SystemGit, &product).await.unwrap();

    assert_eq!(
        tips.observe(&SystemGit, &product).await,
        Ok(TipMove::Unchanged)
    );
}

#[tokio::test]
async fn a_fast_forward_counts_one_merge_with_its_lines() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    let product = pod.product(&repo, 3);
    let mut tips = Tips::default();
    tips.observe(&SystemGit, &product).await.unwrap();

    // Lines 1, 2, 4 and 5 kept, 3 removed, three added.
    merge_branch(&repo, "spec-x", "1\n2\n4\n5\nx\ny\nz\n");

    assert_eq!(
        tips.observe(&SystemGit, &product).await,
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
    tips.observe(&SystemGit, &product).await.unwrap();

    merge_branch(&repo, "spec-x", "1\n2\n3\n4\n5\n6\n");
    merge_branch(&repo, "spec-y", "1\n2\n3\n4\n6\n");

    assert_eq!(
        tips.observe(&SystemGit, &product).await,
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
    tips.observe(&SystemGit, &product).await.unwrap();

    merge_branch(&repo, "spec-x", "1\n2\n3\n4\n5\n6\n");
    merge_branch(&repo, "spec-y", "1\n2\n3\n4\n5\n6\n7\n");

    assert_eq!(
        tips.observe(&SystemGit, &product).await,
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
    tips.observe(&SystemGit, &product).await.unwrap();
    let tree = git(&repo, &["rev-parse", "HEAD^{tree}"]);
    let unrelated = git(&repo, &["commit-tree", &tree, "-m", "unrelated"]);
    git(&repo, &["update-ref", "refs/heads/main", &unrelated]);
    let logs = CapturedLogs::start();

    let moved = tips.observe(&SystemGit, &product).await;

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
    assert_eq!(
        tips.observe(&SystemGit, &product).await,
        Ok(TipMove::Unchanged)
    );
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
