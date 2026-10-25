use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
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

/// A generous git limit: a full test run can keep one `git` call waiting for seconds.
const TEST_GIT_LIMIT: Duration = Duration::from_secs(60);

fn test_git() -> SystemGit {
    SystemGit::with_limit(TEST_GIT_LIMIT)
}

fn stats(registry: PathBuf) -> (PodStats<SystemGit>, Arc<Mutex<PodSnapshot>>) {
    let snapshot = Arc::new(Mutex::new(PodSnapshot::default()));
    let tips = tips_file(&registry.parent().unwrap().join("data/store"));
    (
        PodStats::new(
            registry,
            tips,
            test_git(),
            Arc::clone(&snapshot),
            UtcOffset::UTC,
        ),
        snapshot,
    )
}

/// The tip file the `stats` of `pod`'s registry use.
fn tips_path(pod: &Pod) -> PathBuf {
    tips_file(&pod.root.join("data/store"))
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

    let slots = slots(&test_git(), &pod.product(&repo, 3)).await.unwrap();

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

    let slots = slots(&test_git(), &pod.product(&repo, 1)).await.unwrap();

    assert_eq!((slots.busy, slots.free), (2, 0));
}

// --- merges ---

/// Reads the product's history, then reads the move and saves the tip, as a poll does.
async fn observe(tips: &mut Tips, product: &Product) -> Result<TipMove, &'static str> {
    let history = read_history(&test_git(), product).await?;
    let moved = tips.moved(&test_git(), product, &history).await?;
    tips.save(&product.name, &history.tip).unwrap();
    Ok(moved)
}

#[test]
fn a_reflog_time_comes_from_its_selector() {
    assert_eq!(reflog_time("main@{1791989197}"), Some(1_791_989_197));
    assert_eq!(reflog_time("refs/heads/main@{12}"), Some(12));
    assert_eq!(reflog_time("main"), None);
    assert_eq!(reflog_time("main@{0}x"), None);
}

#[tokio::test]
async fn a_first_sight_with_no_file_counts_no_merge() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    let mut tips = Tips::load(tips_path(&pod));

    let moved = observe(&mut tips, &pod.product(&repo, 3)).await;

    assert_eq!(moved, Ok(TipMove::Unchanged));
}

#[tokio::test]
async fn an_unmoved_tip_counts_no_merge() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    let product = pod.product(&repo, 3);
    let mut tips = Tips::load(tips_path(&pod));
    observe(&mut tips, &product).await.unwrap();

    assert_eq!(observe(&mut tips, &product).await, Ok(TipMove::Unchanged));
}

#[tokio::test]
async fn a_fast_forward_counts_one_merge_with_its_lines() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    let product = pod.product(&repo, 3);
    let mut tips = Tips::load(tips_path(&pod));
    observe(&mut tips, &product).await.unwrap();

    // Lines 1, 2, 4 and 5 kept, 3 removed, three added.
    merge_branch(&repo, "spec-x", "1\n2\n4\n5\nx\ny\nz\n");

    assert_eq!(
        observe(&mut tips, &product).await,
        Ok(TipMove::Merges(vec![Merge {
            name: Some("spec-x".into()),
            entry: Some(0),
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
    let mut tips = Tips::load(tips_path(&pod));
    observe(&mut tips, &product).await.unwrap();

    merge_branch(&repo, "spec-x", "1\n2\n3\n4\n5\n6\n");
    merge_branch(&repo, "spec-y", "1\n2\n3\n4\n6\n");

    assert_eq!(
        observe(&mut tips, &product).await,
        Ok(TipMove::Merges(vec![
            Merge {
                name: Some("spec-x".into()),
                entry: Some(1),
                added: 1,
                removed: 0,
            },
            Merge {
                name: Some("spec-y".into()),
                entry: Some(0),
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
    let mut tips = Tips::load(tips_path(&pod));
    observe(&mut tips, &product).await.unwrap();

    merge_branch(&repo, "spec-x", "1\n2\n3\n4\n5\n6\n");
    merge_branch(&repo, "spec-y", "1\n2\n3\n4\n5\n6\n7\n");

    assert_eq!(
        observe(&mut tips, &product).await,
        Ok(TipMove::Merges(vec![Merge {
            name: None,
            entry: None,
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
    let mut tips = Tips::load(tips_path(&pod));
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

#[tokio::test]
async fn a_rewrite_between_two_merges_in_one_poll_counts_the_two_merges_and_is_logged() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    let product = pod.product(&repo, 3);
    let mut tips = Tips::load(tips_path(&pod));
    observe(&mut tips, &product).await.unwrap();

    merge_branch(&repo, "spec-x", "1\n2\n3\n4\n5\n6\n");
    let tree = git(&repo, &["rev-parse", "HEAD^{tree}"]);
    let unrelated = git(&repo, &["commit-tree", &tree, "-m", "unrelated"]);
    git(
        &repo,
        &["update-ref", "-m", "rewrite", "refs/heads/main", &unrelated],
    );
    git(&repo, &["reset", "-q", "--hard"]);
    merge_branch(&repo, "spec-y", "1\n2\n3\n4\n5\n6\n7\n");
    let logs = CapturedLogs::start();

    let moved = observe(&mut tips, &product).await;

    assert_eq!(
        moved,
        Ok(TipMove::Merges(vec![
            // Newest first: spec-y, the rewrite, spec-x. `git reset --hard` writes no entry
            // for the branch.
            Merge {
                name: Some("spec-x".into()),
                entry: Some(2),
                added: 1,
                removed: 0,
            },
            Merge {
                name: Some("spec-y".into()),
                entry: Some(0),
                added: 1,
                removed: 0,
            },
        ]))
    );
    let text = logs.text();
    let line = format!(
        "base branch rewritten; not counted pod.product={}",
        pod.name
    );
    assert_eq!(text.matches(&line).count(), 1, "{text}");
}

#[test]
fn a_merge_name_comes_from_a_merge_subject() {
    assert_eq!(
        merge_name("merge chief-voice: Fast-forward"),
        Some("chief-voice".into())
    );
    assert_eq!(
        merge_name("merge reply-ownership: Merge made by the 'ort' strategy."),
        Some("reply-ownership".into())
    );
    assert_eq!(merge_name("commit: fix the build"), None);
    assert_eq!(merge_name("merge : Fast-forward"), None);
    assert_eq!(merge_name("merge spec-x"), None);
}

#[test]
fn a_merge_names_its_spec_and_stage() {
    let specs = ["away-web-app", "foo", "foo-1b"];
    let is_spec = |id: &str| specs.contains(&id);
    let named = |id: &str, stage: Option<&str>| SpecRef {
        id: id.into(),
        stage: stage.map(Into::into),
    };
    assert_eq!(
        spec_ref("away-web-app-1b", is_spec),
        named("away-web-app", Some("1b"))
    );
    assert_eq!(
        spec_ref("away-web-app", is_spec),
        named("away-web-app", None)
    );
    // A spec literally named foo-1b keeps its whole name.
    assert_eq!(spec_ref("foo-1b", is_spec), named("foo-1b", None));
    assert_eq!(spec_ref("foo-2", is_spec), named("foo", Some("2")));
    assert_eq!(spec_ref("foo-3a", is_spec), named("foo", Some("3a")));
    // Not a stage, or no spec of that id: the whole name, as today.
    for name in [
        "foo-1bb", "foo-b1", "foo-1B", "foo-", "foo-v2", "bar-1b", "bar",
    ] {
        assert_eq!(spec_ref(name, is_spec), named(name, None), "{name}");
    }
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
async fn a_poll_counts_a_stage_under_its_spec() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    write(
        &repo.join(".superpowers/pod/specs/away-web-app.md"),
        "# Spec\n",
    );
    let (mut stats, _) = stats(one_product(&pod, &repo));
    stats.poll().await;

    merge_branch(&repo, "away-web-app", "1\n2\n3\n4\n5\n6\n");
    merge_branch(&repo, "away-web-app-1b", "1\n2\n3\n4\n5\n6\n7\n");
    stats.poll().await;

    let product = ("pod.product", pod.name.as_str());
    assert_eq!(
        test_support::exact_counter_value("pod.merges", &[product, ("spec.id", "away-web-app")]),
        Some(1)
    );
    assert_eq!(
        test_support::exact_counter_value(
            "pod.merges",
            &[product, ("spec.id", "away-web-app"), ("spec.stage", "1b")]
        ),
        Some(1)
    );
    assert_eq!(
        test_support::exact_counter_value("pod.merges", &[product, ("spec.id", "away-web-app-1b")]),
        None
    );
}

#[tokio::test]
async fn an_unreadable_pod_folder_counts_its_merges_once_when_readable() {
    use std::os::unix::fs::PermissionsExt;

    let pod = Pod::new();
    let repo = pod.repo("main");
    let specs = repo.join(".superpowers/pod/specs");
    write(&specs.join("spec-u.md"), "# Spec\n");
    let (mut stats, _) = stats(one_product(&pod, &repo));
    stats.poll().await;

    std::fs::set_permissions(&specs, std::fs::Permissions::from_mode(0o000)).unwrap();
    merge_branch(&repo, "spec-u", "1\n2\n3\n4\n5\n6\n");
    stats.poll().await;
    let product = ("pod.product", pod.name.as_str());
    let spec = ("spec.id", "spec-u");
    assert_eq!(
        test_support::counter_value("pod.merges", &[product, spec]),
        None
    );

    std::fs::set_permissions(&specs, std::fs::Permissions::from_mode(0o755)).unwrap();
    stats.poll().await;
    stats.poll().await;

    assert_eq!(
        test_support::counter_value("pod.merges", &[product, spec]),
        Some(1)
    );
}

/// The counter rule the dashboards' SQL applies, in Rust: the rise of one series in a
/// window starting at `start`, from its points in time order from the lookback on. A copy
/// of `rise_in_window` in telemetry_tests.rs, which is private to that module; the two
/// must stay the same.
fn rise_in_window(points: &[(i64, f64)], start: i64) -> f64 {
    let mut before = 0.0;
    let mut total = 0.0;
    for &(time, value) in points {
        // `if(Value >= before, Value - before, Value)`, with `before` 0 for a first point.
        let rise = if value >= before {
            value - before
        } else {
            value
        };
        if time >= start {
            total += rise;
        }
        before = value;
    }
    total
}

/// Every `pod.merges` data point of `product` in the latest export, as its attributes
/// (as text) and its value.
fn merge_series(product: &str) -> Vec<(BTreeMap<String, String>, u64)> {
    test_support::metrics()
        .iter()
        .rev()
        .find_map(|resource| {
            resource
                .scope_metrics()
                .flat_map(|scope| scope.metrics())
                .find(|metric| metric.name() == "pod.merges")
        })
        .and_then(|metric| {
            let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric.data() else {
                return None;
            };
            Some(
                sum.data_points()
                    .map(|point| {
                        let attributes: BTreeMap<String, String> = point
                            .attributes()
                            .map(|kv| (kv.key.to_string(), kv.value.as_str().into_owned()))
                            .collect();
                        (attributes, point.value())
                    })
                    .filter(|(attributes, _)| {
                        attributes.get("pod.product").map(String::as_str) == Some(product)
                    })
                    .collect(),
            )
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn eight_merges_count_eight_each_with_its_spec_and_the_counter_rule_agrees() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    let ids = [
        "webhook-loop-guards",
        "store-disk-fault",
        "alert-repeats",
        "harold-autostart",
        "voice-conversation",
        "away-web-app",
        "monitor-timeouts",
    ];
    for id in ids {
        write(
            &repo.join(format!(".superpowers/pod/specs/{id}.md")),
            "# Spec\n",
        );
    }
    let (mut stats, _) = stats(one_product(&pod, &repo));
    stats.poll().await;

    let merged = [
        "webhook-loop-guards",
        "store-disk-fault",
        "alert-repeats",
        "harold-autostart",
        "voice-conversation",
        "away-web-app",
        "away-web-app-1b",
        "monitor-timeouts",
    ];
    for (n, branch) in merged.iter().enumerate() {
        merge_branch(&repo, branch, &format!("{}\n", "x\n".repeat(n + 1)));
    }
    stats.poll().await;

    let product = ("pod.product", pod.name.as_str());
    assert_eq!(
        test_support::counter_value("pod.merges", &[product]),
        Some(8)
    );
    let series: Vec<_> = merge_series(&pod.name)
        .into_iter()
        .filter(|(attributes, _)| attributes.len() > 1)
        .collect();
    let rise: f64 = series
        .iter()
        .map(|(_, value)| rise_in_window(&[(10, *value as f64)], 5))
        .sum();
    assert_eq!(rise, 8.0);
    let mut named: Vec<(String, Option<String>)> = series
        .iter()
        .map(|(attributes, _)| {
            (
                attributes["spec.id"].clone(),
                attributes.get("spec.stage").cloned(),
            )
        })
        .collect();
    named.sort();
    let mut expected: Vec<(String, Option<String>)> =
        ids.iter().map(|id| ((*id).to_owned(), None)).collect();
    expected.push(("away-web-app".into(), Some("1b".into())));
    expected.sort();
    assert_eq!(named, expected);
    let keys = |stage: bool| -> BTreeSet<&str> {
        let wanted = if stage {
            ["pod.product", "spec.id", "spec.stage"].to_vec()
        } else {
            ["pod.product", "spec.id"].to_vec()
        };
        wanted.into_iter().collect()
    };
    for (attributes, _) in &series {
        let found: BTreeSet<&str> = attributes.keys().map(String::as_str).collect();
        assert_eq!(
            found,
            keys(attributes.contains_key("spec.stage")),
            "{attributes:?}"
        );
    }
    assert!(
        series
            .iter()
            .any(|(attributes, _)| attributes.contains_key("spec.stage"))
    );
}

#[tokio::test]
async fn a_products_first_poll_records_its_merge_counters_from_zero() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    let path = registry(
        &pod,
        &format!(
            "[products.{}]\nrepo = \"{}\"\nbase = \"main\"\n",
            pod.name,
            repo.display()
        ),
    );
    let (mut stats, _snapshot) = stats(path);

    stats.poll().await;

    let product = ("pod.product", pod.name.as_str());
    assert_eq!(
        test_support::exact_counter_value("pod.merges", &[product]),
        Some(0)
    );
    for change in ["added", "removed"] {
        assert_eq!(
            test_support::exact_counter_value("pod.merged_lines", &[product, ("change", change)]),
            Some(0),
            "{change}"
        );
    }
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

/// `start` polls into the one process-wide `gauge_snapshot()`, and each poll replaces the
/// whole snapshot with its own registry's products. The tests that start the poller hold this
/// from before `start` until the task has stopped, so neither clears the other's product.
static GAUGE_SNAPSHOT_TESTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
async fn the_task_polls_at_start_reports_the_gauges_and_stops_at_shutdown() {
    let _gauge_snapshot = GAUGE_SNAPSHOT_TESTS.lock().await;
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

    let handle = start(path, tips_path(&pod), shutdown_rx);

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
fn a_file_with_a_second_dot_is_not_a_spec() {
    let pod = Pod::new();
    let dir = pod.root.join("pod");
    write(&dir.join("specs/a.md"), "# Spec\n");
    write(&dir.join("specs/a.revisions.md"), "# Revisions\n");
    write(&dir.join("specs/b.revisions.md"), "# Revisions\n");

    let specs = read_specs(&dir, &HashMap::new(), &HashMap::new(), UtcOffset::UTC).unwrap();

    let ids: Vec<&str> = specs.iter().map(|spec| spec.id.as_str()).collect();
    assert_eq!(ids, ["a"]);
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
fn a_spec_that_merges_is_logged_without_a_lead_time() {
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
    assert_eq!(
        test_support::histogram_sum("pod.spec.lead_time", &[("pod.product", product.as_str())]),
        None
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
    let mut facts = spec("g", SpecState::Merged, "2026-10-14T09:00:00Z");
    facts.ready_at = None;
    let spec_ref = SpecRef {
        id: "g".into(),
        stage: None,
    };

    // No ready time, and merged before it was approved: the build phase would be negative.
    record_lead_time(
        &product,
        &spec_ref,
        &facts,
        at("2026-10-14T09:00:00Z"),
        true,
    );

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

#[tokio::test]
async fn a_git_call_over_its_limit_answers_none_and_production_keeps_five_seconds() {
    let pod = Pod::new();
    let repo = pod.repo("main");

    // A git alias that sleeps 2 s is the slow command; the limit is 200 ms.
    let slow = ["-c", "alias.wait=!sleep 2", "wait"];
    let answer = SystemGit::with_limit(Duration::from_millis(200))
        .run(&repo, &slow)
        .await;
    assert!(answer.is_none());
    let answer = test_git().run(&repo, &["rev-parse", "HEAD"]).await;
    assert!(answer.is_some());
    assert_eq!(GIT_TIMEOUT, Duration::from_secs(5));
    assert_eq!(SystemGit::default().limit, GIT_TIMEOUT);
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

/// Every `pod.spec.lead_time` data point of `product` with `phase` in the latest export,
/// as its attributes (as text) and its count.
fn lead_time_series(product: &str, phase: &str) -> Vec<(BTreeMap<String, String>, u64)> {
    test_support::metrics()
        .iter()
        .rev()
        .find_map(|resource| {
            resource
                .scope_metrics()
                .flat_map(|scope| scope.metrics())
                .find(|metric| metric.name() == "pod.spec.lead_time")
        })
        .and_then(|metric| {
            let AggregatedMetrics::F64(MetricData::Histogram(histogram)) = metric.data() else {
                return None;
            };
            Some(
                histogram
                    .data_points()
                    .map(|point| {
                        let attributes: BTreeMap<String, String> = point
                            .attributes()
                            .map(|kv| (kv.key.to_string(), kv.value.as_str().into_owned()))
                            .collect();
                        (attributes, point.count())
                    })
                    .filter(|(attributes, _)| {
                        attributes.get("pod.product").map(String::as_str) == Some(product)
                            && attributes.get("phase").map(String::as_str) == Some(phase)
                    })
                    .collect(),
            )
        })
        .unwrap_or_default()
}

/// The time of the newest reflog entry of `main` in `repo`.
fn newest_merge_time(repo: &Path) -> OffsetDateTime {
    let entry = git(
        repo,
        &[
            "reflog",
            "show",
            "--date=unix",
            "--format=%gd",
            "-1",
            "refs/heads/main",
        ],
    );
    OffsetDateTime::from_unix_timestamp(reflog_time(&entry).unwrap()).unwrap()
}

/// Writes the files of a spec `id` that is ready and approved: the spec, its link and its
/// approval, then waits out the second, so the whole-second merge time that follows is
/// after the approval's creation time.
async fn approved_spec(repo: &Path, id: &str) {
    let pod = repo.join(".superpowers/pod");
    write(&pod.join(format!("specs/{id}.md")), "# Spec\n");
    write(
        &pod.join(format!("specs/{id}.url")),
        "https://example.com\n",
    );
    write(&pod.join(format!("approvals/{id}.md")), "approved\n");
    tokio::time::sleep(Duration::from_millis(1100)).await;
}

fn keys(attributes: &BTreeMap<String, String>) -> BTreeSet<&str> {
    attributes.keys().map(String::as_str).collect()
}

#[tokio::test]
async fn a_merge_records_both_phases_with_its_spec() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    approved_spec(&repo, "g").await;
    let (mut stats, _) = stats(one_product(&pod, &repo));
    stats.poll().await;

    merge_branch(&repo, "g", "x\n");
    stats.poll().await;

    let files = repo.join(".superpowers/pod");
    let ready = created(&files.join("specs/g.url"));
    let approved = created(&files.join("approvals/g.md"));
    let merged = newest_merge_time(&repo);
    let seconds = |from: OffsetDateTime, to: OffsetDateTime| (to - from).as_seconds_f64();
    assert_eq!(
        test_support::histogram_sum(
            "pod.spec.lead_time",
            &[
                ("pod.product", pod.name.as_str()),
                ("phase", "waiting_for_user"),
                ("spec.id", "g"),
                ("spec.revisions", "0"),
            ]
        ),
        Some(seconds(ready, approved))
    );
    assert_eq!(
        test_support::histogram_sum(
            "pod.spec.lead_time",
            &[
                ("pod.product", pod.name.as_str()),
                ("phase", "build"),
                ("spec.id", "g"),
            ]
        ),
        Some(seconds(approved, merged))
    );
}

#[tokio::test]
async fn a_stage_records_build_but_not_waiting_again() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    approved_spec(&repo, "away-web-app").await;
    let (mut stats, _) = stats(one_product(&pod, &repo));
    stats.poll().await;

    merge_branch(&repo, "away-web-app", "x\n");
    stats.poll().await;
    merge_branch(&repo, "away-web-app-1b", "y\n");
    stats.poll().await;

    let waiting = lead_time_series(&pod.name, "waiting_for_user");
    assert_eq!(waiting.len(), 1, "{waiting:?}");
    assert_eq!(waiting[0].1, 1);
    assert_eq!(
        keys(&waiting[0].0),
        BTreeSet::from(["pod.product", "phase", "spec.revisions", "spec.id"])
    );
    let mut build = lead_time_series(&pod.name, "build");
    build.sort_by_key(|(attributes, _)| attributes.get("spec.stage").cloned());
    assert_eq!(build.len(), 2, "{build:?}");
    assert_eq!(build[0].0.get("spec.stage"), None);
    assert_eq!(build[1].0.get("spec.stage").map(String::as_str), Some("1b"));
    let base = ["pod.product", "phase", "spec.revisions", "spec.id"];
    let staged = [base.as_slice(), &["spec.stage"]].concat();
    assert_eq!(keys(&build[0].0), base.into_iter().collect());
    assert_eq!(keys(&build[1].0), staged.into_iter().collect());
    assert_eq!((build[0].1, build[1].1), (1, 1));
}

#[tokio::test]
async fn waiting_is_recorded_on_the_specs_oldest_merge() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    approved_spec(&repo, "s").await;
    approved_spec(&repo, "t").await;
    let (mut stats, _) = stats(one_product(&pod, &repo));
    stats.poll().await;

    merge_branch(&repo, "s-1", "a\n");
    stats.poll().await;
    merge_branch(&repo, "s", "b\n");
    stats.poll().await;
    merge_branch(&repo, "t", "c\n");
    stats.poll().await;
    git(&repo, &["branch", "-q", "-D", "t"]);
    merge_branch(&repo, "t", "d\n");
    stats.poll().await;

    let of = |id: &str, phase: &str| -> Vec<_> {
        lead_time_series(&pod.name, phase)
            .into_iter()
            .filter(|(attributes, _)| attributes["spec.id"] == id)
            .collect()
    };
    let waiting = of("s", "waiting_for_user");
    assert_eq!(waiting.len(), 1, "{waiting:?}");
    assert_eq!(waiting[0].1, 1);
    assert_eq!(
        waiting[0].0.get("spec.stage").map(String::as_str),
        Some("1")
    );
    let waiting = of("t", "waiting_for_user");
    assert_eq!(waiting.iter().map(|(_, count)| count).sum::<u64>(), 1);
    let build = of("t", "build");
    assert_eq!(build.iter().map(|(_, count)| count).sum::<u64>(), 2);
}

#[tokio::test]
async fn a_merge_without_an_approval_records_no_lead_time() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    let specs = repo.join(".superpowers/pod/specs");
    write(&specs.join("h.md"), "# Spec\n");
    write(&specs.join("h.url"), "https://example.com\n");
    let (mut stats, _) = stats(one_product(&pod, &repo));
    stats.poll().await;

    merge_branch(&repo, "h", "x\n");
    stats.poll().await;

    assert_eq!(
        test_support::histogram_sum("pod.spec.lead_time", &[("pod.product", pod.name.as_str())]),
        None
    );
    assert_eq!(
        test_support::counter_value(
            "pod.merges",
            &[("pod.product", pod.name.as_str()), ("spec.id", "h")]
        ),
        Some(1)
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
        test_git().run(repo, args).await
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
        tips_path(&pod),
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
    let _gauge_snapshot = GAUGE_SNAPSHOT_TESTS.lock().await;
    let pod = Pod::new();
    let repo = pod.repo("main");
    write(
        &repo.join(".superpowers/pod/specs/spec-z.md"),
        "one two three four\n",
    );
    let path = one_product(&pod, &repo);
    let (shutdown_tx, shutdown_rx) = watch::channel(());

    let handle = start(path, tips_path(&pod), shutdown_rx);

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

// --- the counted-tip file ---

fn merges_of(pod: &Pod, spec: &str) -> Option<u64> {
    test_support::counter_value(
        "pod.merges",
        &[("pod.product", pod.name.as_str()), ("spec.id", spec)],
    )
}

fn tip_file_text(pod: &Pod) -> String {
    std::fs::read_to_string(tips_path(pod)).unwrap()
}

/// What the tip file holds when `main` of `repo` is the pod's counted tip.
fn tip_line(pod: &Pod, repo: &Path) -> String {
    format!("{} {}\n", pod.name, git(repo, &["rev-parse", "main"]))
}

#[tokio::test]
async fn the_first_poll_after_a_start_counts_the_merges_since_the_saved_tip() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    let path = one_product(&pod, &repo);
    let (mut first, _) = stats(path.clone());
    first.poll().await;
    drop(first);

    merge_branch(&repo, "spec-a", "1\n2\n3\n4\n5\n6\n");
    merge_branch(&repo, "spec-b", "1\n2\n3\n4\n5\n6\n7\n");
    let (mut second, _) = stats(path);
    second.poll().await;

    assert_eq!(merges_of(&pod, "spec-a"), Some(1));
    assert_eq!(merges_of(&pod, "spec-b"), Some(1));
    assert_eq!(
        test_support::counter_value("pod.merges", &[("pod.product", pod.name.as_str())]),
        Some(2)
    );
    assert_eq!(tip_file_text(&pod), tip_line(&pod, &repo));
}

#[tokio::test]
async fn a_saved_tip_equal_to_the_current_tip_counts_nothing() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    let path = one_product(&pod, &repo);
    let (mut first, _) = stats(path.clone());
    first.poll().await;
    drop(first);

    let (mut second, _) = stats(path);
    second.poll().await;

    assert_eq!(
        test_support::counter_value("pod.merges", &[("pod.product", pod.name.as_str())]),
        Some(0)
    );
}

#[tokio::test]
async fn a_missing_file_counts_nothing_and_writes_the_current_tip() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    merge_branch(&repo, "spec-a", "1\n2\n3\n4\n5\n6\n");
    let (mut stats, _) = stats(one_product(&pod, &repo));

    stats.poll().await;

    assert_eq!(merges_of(&pod, "spec-a"), None);
    assert_eq!(tip_file_text(&pod), tip_line(&pod, &repo));
}

#[tokio::test]
async fn a_saved_tip_not_in_the_reflog_counts_nothing_and_writes_the_current_tip() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    let tree = git(&repo, &["rev-parse", "HEAD^{tree}"]);
    let unrelated = git(&repo, &["commit-tree", &tree, "-m", "unrelated"]);
    write(&tips_path(&pod), &format!("{} {unrelated}\n", pod.name));
    merge_branch(&repo, "spec-a", "1\n2\n3\n4\n5\n6\n");
    let (mut stats, _) = stats(one_product(&pod, &repo));

    stats.poll().await;

    assert_eq!(merges_of(&pod, "spec-a"), None);
    assert_eq!(tip_file_text(&pod), tip_line(&pod, &repo));
}

/// The `git` on the PATH, but `merge-base` gets no answer while the flag is set.
struct SilentMergeBase(Arc<AtomicBool>);

impl Git for SilentMergeBase {
    async fn run(&self, repo: &Path, args: &[&str]) -> Option<GitOutput> {
        if args.first() == Some(&"merge-base") && self.0.load(Ordering::SeqCst) {
            return None;
        }
        test_git().run(repo, args).await
    }
}

#[tokio::test]
async fn a_git_with_no_answer_records_nothing_and_leaves_the_file() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    let silent = Arc::new(AtomicBool::new(false));
    let mut stats = PodStats::new(
        one_product(&pod, &repo),
        tips_path(&pod),
        SilentMergeBase(Arc::clone(&silent)),
        Arc::new(Mutex::new(PodSnapshot::default())),
        UtcOffset::UTC,
    );
    stats.poll().await;
    let old = tip_file_text(&pod);
    merge_branch(&repo, "spec-a", "1\n2\n3\n4\n5\n6\n");
    merge_branch(&repo, "spec-b", "1\n2\n3\n4\n5\n6\n7\n");

    silent.store(true, Ordering::SeqCst);
    stats.poll().await;

    assert_eq!(merges_of(&pod, "spec-a"), None);
    assert_eq!(tip_file_text(&pod), old);

    silent.store(false, Ordering::SeqCst);
    stats.poll().await;
    stats.poll().await;

    assert_eq!(merges_of(&pod, "spec-a"), Some(1));
    assert_eq!(merges_of(&pod, "spec-b"), Some(1));
}

#[tokio::test]
async fn a_crash_after_the_tip_file_is_written_loses_that_polls_counts_and_never_doubles_them() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    let path = one_product(&pod, &repo);
    let (mut first, _) = stats(path.clone());
    first.poll().await;
    drop(first);
    merge_branch(&repo, "spec-a", "1\n2\n3\n4\n5\n6\n");
    merge_branch(&repo, "spec-b", "1\n2\n3\n4\n5\n6\n7\n");
    // What `save` wrote before a crash stopped the recording.
    write(&tips_path(&pod), &tip_line(&pod, &repo));

    let (mut second, _) = stats(path);
    second.poll().await;
    second.poll().await;

    assert_eq!(merges_of(&pod, "spec-a"), None);
    assert_eq!(merges_of(&pod, "spec-b"), None);
    assert_eq!(tip_file_text(&pod), tip_line(&pod, &repo));
}

#[tokio::test]
async fn a_restart_after_a_counted_poll_counts_nothing_again() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    let path = one_product(&pod, &repo);
    let (mut first, _) = stats(path.clone());
    first.poll().await;
    merge_branch(&repo, "spec-a", "1\n2\n3\n4\n5\n6\n");
    merge_branch(&repo, "spec-b", "1\n2\n3\n4\n5\n6\n7\n");
    first.poll().await;
    assert_eq!(merges_of(&pod, "spec-a"), Some(1));
    assert_eq!(merges_of(&pod, "spec-b"), Some(1));
    drop(first);

    let (mut second, _) = stats(path);
    second.poll().await;
    second.poll().await;

    assert_eq!(merges_of(&pod, "spec-a"), Some(1));
    assert_eq!(merges_of(&pod, "spec-b"), Some(1));
}

#[tokio::test]
async fn the_tip_file_is_readable_by_the_user_only_and_keeps_a_name_with_a_space() {
    use std::os::unix::fs::PermissionsExt as _;
    let pod = Pod::new();
    let repo = pod.repo("main");
    let name = format!("{} x", pod.name);
    let path = registry(
        &pod,
        &format!(
            "[products.\"{name}\"]\nrepo = \"{}\"\nbase = \"main\"\nslots = 3\n",
            repo.display()
        ),
    );
    let (mut stats, _) = stats(path);

    stats.poll().await;

    let mode = std::fs::metadata(tips_path(&pod))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    let tips = Tips::load(tips_path(&pod));
    assert_eq!(
        tips.counted.get(&name),
        Some(&git(&repo, &["rev-parse", "main"]))
    );
}

#[tokio::test]
async fn an_unwritable_tip_file_counts_nothing_until_it_can_be_written() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    let blocker = pod.root.join("blocker");
    let mut stats = PodStats::new(
        one_product(&pod, &repo),
        blocker.join("tips"),
        test_git(),
        Arc::new(Mutex::new(PodSnapshot::default())),
        UtcOffset::UTC,
    );
    // A tip is known, then its folder is replaced by a regular file.
    stats.poll().await;
    std::fs::remove_dir_all(&blocker).unwrap();
    write(&blocker, "in the way");
    merge_branch(&repo, "spec-a", "1\n2\n3\n4\n5\n6\n");
    let logs = CapturedLogs::start();

    stats.poll().await;
    stats.poll().await;

    assert_eq!(merges_of(&pod, "spec-a"), None);
    let text = logs.text();
    let line = format!(
        "pod merges and specs unavailable pod.product={} reason=\"tip file unwritable\"",
        pod.name
    );
    assert_eq!(text.matches(&line).count(), 1, "{text}");

    std::fs::remove_file(&blocker).unwrap();
    stats.poll().await;

    assert_eq!(merges_of(&pod, "spec-a"), Some(1));
}

#[tokio::test]
async fn a_garbled_tip_file_counts_nothing_for_that_product() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    write(&tips_path(&pod), "garbage\n");
    merge_branch(&repo, "spec-a", "1\n2\n3\n4\n5\n6\n");
    let (mut stats, _) = stats(one_product(&pod, &repo));

    stats.poll().await;

    assert_eq!(merges_of(&pod, "spec-a"), None);
    assert_eq!(tip_file_text(&pod), tip_line(&pod, &repo));
}

#[tokio::test]
async fn a_saved_tip_that_is_an_ancestor_but_not_in_the_reflog_counts_nothing() {
    let pod = Pod::new();
    let repo = repo_without_a_reflog(&pod);
    write(&tips_path(&pod), &tip_line(&pod, &repo));
    merge_branch(&repo, "spec-x", "1\n2\n3\n4\n5\n6\n");
    let (mut stats, _) = stats(one_product(&pod, &repo));

    stats.poll().await;

    assert_eq!(
        test_support::counter_value("pod.merges", &[("pod.product", pod.name.as_str())]),
        Some(0)
    );
    assert_eq!(tip_file_text(&pod), tip_line(&pod, &repo));
}
