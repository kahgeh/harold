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
use crate::agent::domain::{AgentSnapshot, EffectiveAgentState};
use crate::outbound::fake::CapturedLogs;
use crate::store::EventVersion;
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
            paused: false,
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

/// An agent list with no live pane.
fn no_agents() -> AgentSource {
    Arc::new(Vec::new)
}

/// An agent snapshot with no pane, for `start`.
fn empty_hub() -> AgentSnapshotHub {
    AgentSnapshotHub::new(AgentSnapshot {
        through_event_version: EventVersion::start(),
        server_time_ms: 0,
        monitor_health: Vec::new(),
        panes: Vec::new(),
        tts_muted: false,
        messaging_paused: false,
    })
}

fn stats(registry: PathBuf) -> (PodStats<SystemGit>, Arc<Mutex<PodSnapshot>>) {
    stats_with_agents(registry, no_agents(), None)
}

/// `stats` where the live panes are `agents` and the chief's folder is `chief`.
fn stats_with_agents(
    registry: PathBuf,
    agents: AgentSource,
    chief: Option<&str>,
) -> (PodStats<SystemGit>, Arc<Mutex<PodSnapshot>>) {
    let snapshot = Arc::new(Mutex::new(PodSnapshot::default()));
    let tips = tips_file(&registry.parent().unwrap().join("data/store"));
    (
        PodStats::new(
            registry,
            tips,
            test_git(),
            Arc::clone(&snapshot),
            UtcOffset::UTC,
            agents,
            chief.map(str::to_owned),
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
fn the_registry_lists_products_in_the_alphabetical_order_of_their_names() {
    let pod = Pod::new();
    let path = registry(
        &pod,
        "[products.keymap]\nrepo = \"/k\"\nbase = \"main\"\n\n\
         [products.buckwal]\nrepo = \"/b\"\nbase = \"main\"\n\n\
         [products.harold]\nrepo = \"/h\"\nbase = \"main\"\n",
    );

    let names: Vec<String> = read_registry(&path)
        .unwrap()
        .into_iter()
        .map(|product| product.name)
        .collect();

    assert_eq!(names, ["buckwal", "harold", "keymap"]);
}

#[test]
fn the_registry_reads_the_paused_key_and_defaults_to_not_paused() {
    let pod = Pod::new();
    let path = registry(
        &pod,
        "[products.a]\nrepo = \"/a\"\nbase = \"main\"\npaused = true\n\n\
         [products.b]\nrepo = \"/b\"\nbase = \"main\"\n",
    );

    let paused: Vec<(String, bool)> = read_registry(&path)
        .unwrap()
        .into_iter()
        .map(|product| (product.name, product.paused))
        .collect();

    assert_eq!(paused, [("a".to_string(), true), ("b".to_string(), false)]);
}

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
                paused: false,
            },
            Product {
                name: "b".into(),
                repo: PathBuf::from("/y"),
                base: "trunk".into(),
                slots: 3,
                paused: false,
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

// `start` polls into the one process-wide `gauge_snapshot()`, and each poll replaces the
// whole snapshot with its own registry's products. The tests that start the poller hold
// `GAUGE_SNAPSHOT_TESTS` (in the parent module) from before `start` until the task has
// stopped, so neither clears the other's product.

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

    let handle = start(path, tips_path(&pod), empty_hub(), None, shutdown_rx);

    let snapshot = gauge_snapshot();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
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

/// A reflog entry at `time` with `subject`.
fn entry(subject: &str, time: OffsetDateTime) -> ReflogEntry {
    ReflogEntry {
        hash: "h".into(),
        time: time.unix_timestamp(),
        subject: subject.into(),
    }
}

/// What `read_specs` reads from `dir` at `now`, with these worktrees busy, this reflog
/// and these worktrees gone after a merge.
fn read_with(
    dir: &Path,
    now: OffsetDateTime,
    offset: UtcOffset,
    busy: &HashMap<String, OffsetDateTime>,
    reflog: &[ReflogEntry],
    gone: &HashMap<String, OffsetDateTime>,
) -> SpecRead {
    read_specs(
        dir,
        &Sources {
            busy,
            reflog,
            gone,
            now,
            offset,
        },
    )
    .unwrap()
}

/// What `read_specs` reads from `dir` now, from the files alone.
fn read_files(dir: &Path) -> SpecRead {
    read_with(
        dir,
        OffsetDateTime::now_utc(),
        UtcOffset::UTC,
        &HashMap::new(),
        &[],
        &HashMap::new(),
    )
}

fn put_spec(dir: &Path, id: &str) {
    write(&dir.join(format!("specs/{id}.md")), "# Spec\n");
}

fn put_approval(dir: &Path, id: &str) {
    write(
        &dir.join(format!("approvals/{id}.md")),
        "approved: 2026-10-14\n",
    );
}

fn put_started(dir: &Path, id: &str, started: &str) {
    write(
        &dir.join(format!("started/{id}.md")),
        &format!("# Started: {id}\n\nstarted: {started}\nworktree: {id}\n"),
    );
}

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

fn put_merged(dir: &Path, id: &str, merged: &str, commit: &str) {
    write(
        &dir.join(format!("merged/{id}.md")),
        &format!("# Merged: {id}\n\nmerged: {merged}\ncommit: {commit}\n"),
    );
}

fn put_withdrawn(dir: &Path, id: &str, withdrawn: &str, reason: &str) {
    write(
        &dir.join(format!("withdrawn/{id}.md")),
        &format!(
            "# Withdrawn: {id}\n\nwithdrawn: {withdrawn}\nreason: {reason}\nby: other\nnote: one line\n"
        ),
    );
}

#[test]
fn each_state_comes_from_its_file_and_from_the_older_source_at_its_time() {
    let pod = Pod::new();
    let dir = pod.root.join("pod");
    for id in ["a", "b", "c", "d", "e", "f", "g", "h"] {
        put_spec(&dir, id);
    }
    for id in ["b", "c", "d"] {
        put_approval(&dir, id);
    }
    put_started(&dir, "c", "2026-10-20T10:00:00Z");
    put_merged(&dir, "e", "2026-10-22T09:00:00Z", COMMIT);
    put_withdrawn(&dir, "g", "2026-10-21T08:30:00Z", "replaced");
    write(
        &dir.join("rejections.md"),
        "# Rejections\n\n- h, 2026-10-14, too big\n",
    );
    let now = at("2026-10-26T12:00:00Z");
    let t = at("2026-10-14T10:00:00Z");
    let t2 = at("2026-10-14T11:34:00Z");
    let busy = HashMap::from([("d".to_owned(), t)]);
    let reflog = [entry("merge f: Fast-forward", t2)];
    let offset = UtcOffset::from_hms(11, 0, 0).unwrap();

    let read = read_with(&dir, now, offset, &busy, &reflog, &HashMap::new());

    let specs = by_id(read.specs);
    let state = |id: &str| (specs[id].state, specs[id].state_at);
    assert_eq!(
        state("a"),
        (SpecState::Open, created(&dir.join("specs/a.md")))
    );
    assert_eq!(
        state("b"),
        (SpecState::Approved, created(&dir.join("approvals/b.md")))
    );
    assert_eq!(
        state("c"),
        (SpecState::InProgress, at("2026-10-20T10:00:00Z"))
    );
    assert_eq!(state("d"), (SpecState::InProgress, t));
    assert_eq!(
        state("e"),
        (SpecState::Completed, at("2026-10-22T09:00:00Z"))
    );
    assert_eq!(state("f"), (SpecState::Completed, t2));
    assert_eq!(
        state("g"),
        (SpecState::Withdrawn, at("2026-10-21T08:30:00Z"))
    );
    assert_eq!(
        state("h"),
        (SpecState::Withdrawn, at("2026-10-14T00:00:00+11:00"))
    );
    assert_eq!(specs.len(), 8);
    assert_eq!(specs["c"].started_at, Some(at("2026-10-20T10:00:00Z")));
    assert_eq!(specs["e"].merged_commit.as_deref(), Some(COMMIT));
    assert_eq!(specs["f"].merged_at, Some(t2));
    assert_eq!(specs["f"].merged_commit, None);
    assert_eq!(read.notices, Vec::new());
}

#[test]
fn a_later_state_wins_and_a_withdrawn_spec_is_withdrawn_though_it_merged_into_another() {
    let pod = Pod::new();
    let dir = pod.root.join("pod");
    for id in ["approved", "started", "merged", "withdrawn"] {
        put_spec(&dir, id);
        put_approval(&dir, id);
    }
    for id in ["started", "merged", "withdrawn"] {
        put_started(&dir, id, "2026-10-20T10:00:00Z");
    }
    for id in ["merged", "withdrawn"] {
        put_merged(&dir, id, "2026-10-22T09:00:00Z", COMMIT);
    }
    put_withdrawn(&dir, "withdrawn", "2026-10-23T09:00:00Z", "merged_into");

    let specs = by_id(read_files(&dir).specs);

    assert_eq!(specs["approved"].state, SpecState::Approved);
    assert_eq!(specs["started"].state, SpecState::InProgress);
    assert_eq!(specs["merged"].state, SpecState::Completed);
    assert_eq!(specs["withdrawn"].state, SpecState::Withdrawn);
    assert_eq!(
        specs["withdrawn"].withdrawn,
        Some(Withdrawal {
            reason: "merged_into",
            reached: Reached::InProgress,
        })
    );
}

#[test]
fn a_withdrawn_spec_says_how_far_it_got() {
    let pod = Pod::new();
    let dir = pod.root.join("pod");
    for id in ["o", "a", "p"] {
        put_spec(&dir, id);
        put_withdrawn(&dir, id, "2026-10-23T09:00:00Z", "overtaken");
    }
    put_approval(&dir, "a");
    put_approval(&dir, "p");
    put_started(&dir, "p", "2026-10-20T10:00:00Z");
    write(&dir.join("rejections.md"), "- o, 2026-10-14, reason\n");

    let specs = by_id(read_files(&dir).specs);

    let reached = |id: &str| specs[id].withdrawn.clone().unwrap();
    assert_eq!(reached("o").reached, Reached::Open);
    assert_eq!(reached("a").reached, Reached::Approved);
    assert_eq!(reached("p").reached, Reached::InProgress);
    // The withdrawn file wins over the rejection line, and gives the reason.
    assert_eq!(reached("o").reason, "overtaken");
}

#[test]
fn a_pod_without_a_specs_folder_has_no_specs() {
    let pod = Pod::new();

    let read = read_files(&pod.root.join("absent"));

    assert_eq!(read, SpecRead::default());
}

#[test]
fn a_file_with_a_second_dot_is_not_a_spec() {
    let pod = Pod::new();
    let dir = pod.root.join("pod");
    write(&dir.join("specs/a.md"), "# Spec\n");
    write(&dir.join("specs/a.revisions.md"), "# Revisions\n");
    write(&dir.join("specs/b.revisions.md"), "# Revisions\n");

    let specs = read_files(&dir).specs;

    let ids: Vec<&str> = specs.iter().map(|spec| spec.id.as_str()).collect();
    assert_eq!(ids, ["a"]);
}

#[test]
fn revision_marks_count_revisions_direction_and_architect_rounds() {
    let text = "# Revisions\n\
        ## 2026-10-20, rev 1 (first)\n\
        ## 2026-10-21, rev 2 (direction)\n\
        ## 2026-10-22, rev 3 (defect, architect)\n\
        ## 2026-10-23, rev 4 (direction, architect)\n\
        ## 2026-10-24, rev 5\n\
        ### 2026-10-25, a deeper heading (direction)\n\
        ## 2026-10-26, rev 6 (see the note) (defect)\n";

    assert_eq!(
        revision_marks(text),
        RevisionMarks {
            revisions: 5,
            direction: 2,
            architect: 2,
        }
    );
    assert_eq!(revision_marks(""), RevisionMarks::default());
}

#[test]
fn the_revisions_file_gives_the_specs_revisions() {
    let pod = Pod::new();
    let dir = pod.root.join("pod");
    put_spec(&dir, "b");
    assert_eq!(read_files(&dir).specs[0].revisions, 0);

    write(
        &dir.join("specs/b.revisions.md"),
        "## 2026-10-14, rev 1 (first)\n## 2026-10-15, rev 2 (direction, architect)\n",
    );
    // The old count in the request file no longer counts.
    write(
        &dir.join("requests/b.md"),
        "## Revision 1, x\n## Revision 2, y\n",
    );

    let spec = read_files(&dir).specs.remove(0);

    assert_eq!(
        (
            spec.revisions,
            spec.direction_revisions,
            spec.architect_rounds
        ),
        (1, 1, 1)
    );
}

#[test]
fn the_revisions_file_gives_redo_and_revised_after_approval() {
    let pod = Pod::new();
    let dir = pod.root.join("pod");
    put_spec(&dir, "r");
    put_approval(&dir, "r");
    put_started(&dir, "r", "2026-11-01T00:00:00Z");
    // The approval file is new, so headings in the year 2998 are after it.
    write(
        &dir.join("specs/r.revisions.md"),
        "## 2026-10-14, rev 1 (first)\n\
         ## 2998-01-01 10:00, rev 2 (wording)\n\
         ## 2998-01-02 10:00, rev 3 (direction)\n",
    );
    let now = at("2998-01-02T14:00:00Z");

    let read = read_with(
        &dir,
        now,
        UtcOffset::UTC,
        &HashMap::new(),
        &[],
        &HashMap::new(),
    );

    assert_eq!(read.specs[0].redo_seconds, 4 * 3600);
    assert_eq!(read.specs[0].revised_after_approval, 2);
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
        ["open", "approved", "in_progress", "completed", "withdrawn"]
    );
    assert!(SpecState::Withdrawn > SpecState::Completed);
    assert!(SpecState::Completed > SpecState::InProgress);
    assert!(SpecState::InProgress > SpecState::Approved);
    assert!(SpecState::Approved > SpecState::Open);
}

#[test]
fn started_keeps_the_files_time_when_a_later_stage_starts_a_worker() {
    let pod = Pod::new();
    let dir = pod.root.join("pod");
    put_spec(&dir, "s");
    put_started(&dir, "s", "2026-10-20T10:00:00Z");
    let now = OffsetDateTime::now_utc();
    // The worktree of stage 2 is busy now: its pane file is newer than the started file.
    let busy = HashMap::from([("s-2".to_owned(), now)]);

    let read = read_with(&dir, now, UtcOffset::UTC, &busy, &[], &HashMap::new());

    let spec = &read.specs[0];
    assert_eq!(spec.state, SpecState::InProgress);
    assert_eq!(spec.state_at, at("2026-10-20T10:00:00Z"));
    assert_eq!(spec.started_at, Some(at("2026-10-20T10:00:00Z")));
}

/// A pod folder with spec `s` that has a started file 20 days before `now`.
fn staged_spec(pod: &Pod, now: OffsetDateTime) -> PathBuf {
    let dir = pod.root.join("pod");
    put_spec(&dir, "s");
    put_approval(&dir, "s");
    let started = (now - time::Duration::days(20))
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    put_started(&dir, "s", &started);
    dir
}

fn days_ago(now: OffsetDateTime, days: i64) -> OffsetDateTime {
    now - time::Duration::days(days)
}

fn stage_notice() -> Notice {
    Notice::StageMergedWithoutFile {
        id: "s".into(),
        stage: "1b".into(),
    }
}

#[test]
fn a_stage_merge_with_a_started_file_and_no_merged_file_never_completes_the_spec() {
    let pod = Pod::new();
    let now = OffsetDateTime::now_utc();
    let dir = staged_spec(&pod, now);
    let none = HashMap::new();

    // Under 7 days ago, though `started:` is 20 days ago: in progress, nothing to log.
    let fresh = [entry("merge s-1b: Fast-forward", days_ago(now, 2))];
    let read = read_with(&dir, now, UtcOffset::UTC, &none, &fresh, &none);
    assert_eq!(read.specs[0].state, SpecState::InProgress);
    assert_eq!(read.notices, Vec::new());

    // Over 7 days ago: still in progress, and the stage merge is noted.
    let old = [entry("merge s-1b: Fast-forward", days_ago(now, 8))];
    let read = read_with(&dir, now, UtcOffset::UTC, &none, &old, &none);
    assert_eq!(read.specs[0].state, SpecState::InProgress);
    assert_eq!(read.notices, vec![stage_notice()]);

    // A busy worktree of the spec, such as the next stage's, means it is not waiting.
    let busy = HashMap::from([("s-2".to_owned(), now)]);
    let read = read_with(&dir, now, UtcOffset::UTC, &busy, &old, &none);
    assert_eq!(read.specs[0].state, SpecState::InProgress);
    assert_eq!(read.notices, Vec::new());
}

#[test]
fn completed_notice_names_the_merge() {
    let pod = Pod::new();
    let now = OffsetDateTime::now_utc();
    let dir = staged_spec(&pod, now);
    let none = HashMap::new();
    let merge = days_ago(now, 8).replace_nanosecond(0).unwrap();
    let older = days_ago(now, 9).replace_nanosecond(0).unwrap();
    let hash = "0123456789abcdef0123456789abcdef01234567".to_owned();
    let mut newest = entry("merge s: Fast-forward", merge);
    newest.hash = hash.clone();
    let reflog = [newest, entry("merge s: Fast-forward", older)];

    let read = read_with(&dir, now, UtcOffset::UTC, &none, &reflog, &none);
    assert_eq!(
        read.notices,
        vec![Notice::CompletedWithoutFile {
            id: "s".into(),
            merged_at: merge,
            commit: Some(hash)
        }]
    );

    // A merge seen only from a gone worktree has no commit.
    let gone = HashMap::from([("s".to_owned(), merge)]);
    let read = read_with(&dir, now, UtcOffset::UTC, &none, &[], &gone);
    assert_eq!(
        read.notices,
        vec![Notice::CompletedWithoutFile {
            id: "s".into(),
            merged_at: merge,
            commit: None
        }]
    );
}

#[test]
fn a_merge_without_a_stage_over_seven_days_old_completes_a_spec_with_no_merged_file() {
    let pod = Pod::new();
    let now = OffsetDateTime::now_utc();
    let dir = staged_spec(&pod, now);
    let none = HashMap::new();
    // The reflog keeps whole seconds.
    let merge = days_ago(now, 8).replace_nanosecond(0).unwrap();
    let whole = [entry("merge s: Fast-forward", merge)];

    let read = read_with(&dir, now, UtcOffset::UTC, &none, &whole, &none);

    let spec = &read.specs[0];
    assert_eq!((spec.state, spec.state_at), (SpecState::Completed, merge));
    assert_eq!(spec.merged_commit, None);
    assert_eq!(
        read.notices,
        vec![Notice::CompletedWithoutFile {
            id: "s".into(),
            merged_at: merge,
            commit: Some("h".into())
        }]
    );

    // A worktree of a worker running for the spec holds it back.
    let busy = HashMap::from([("s".to_owned(), now)]);
    let read = read_with(&dir, now, UtcOffset::UTC, &busy, &whole, &none);
    assert_eq!(read.specs[0].state, SpecState::InProgress);
    assert_eq!(read.notices, Vec::new());

    // A merge under 7 days ago is not yet the end.
    let recent = [entry("merge s: Fast-forward", days_ago(now, 2))];
    let read = read_with(&dir, now, UtcOffset::UTC, &none, &recent, &none);
    assert_eq!(read.specs[0].state, SpecState::InProgress);
    assert_eq!(read.notices, Vec::new());

    // A worktree seen gone after its merge counts as a merge without a stage.
    let gone = HashMap::from([("s".to_owned(), merge)]);
    let read = read_with(&dir, now, UtcOffset::UTC, &none, &[], &gone);
    assert_eq!(read.specs[0].state, SpecState::Completed);
}

#[test]
fn a_later_stage_merge_blocks_the_completion_by_an_older_merge_without_a_stage() {
    let pod = Pod::new();
    let now = OffsetDateTime::now_utc();
    let dir = staged_spec(&pod, now);
    let none = HashMap::new();
    // Newest first.
    let later_stage = [
        entry("merge s-2: Fast-forward", days_ago(now, 9)),
        entry("merge s: Fast-forward", days_ago(now, 10)),
    ];
    let earlier_stage = [
        entry("merge s: Fast-forward", days_ago(now, 9)),
        entry("merge s-1: Fast-forward", days_ago(now, 10)),
    ];

    let read = read_with(&dir, now, UtcOffset::UTC, &none, &later_stage, &none);
    assert_eq!(read.specs[0].state, SpecState::InProgress);
    assert_eq!(
        read.notices,
        vec![Notice::StageMergedWithoutFile {
            id: "s".into(),
            stage: "2".into()
        }]
    );

    let read = read_with(&dir, now, UtcOffset::UTC, &none, &earlier_stage, &none);
    assert_eq!(read.specs[0].state, SpecState::Completed);
}

#[test]
fn a_spec_with_neither_file_completes_from_a_merge_as_before() {
    let pod = Pod::new();
    let now = OffsetDateTime::now_utc();
    let dir = pod.root.join("pod");
    put_spec(&dir, "s");
    put_spec(&dir, "t");
    let none = HashMap::new();
    let reflog = [
        entry("merge s: Fast-forward", now),
        entry("merge t-1: Fast-forward", now),
    ];

    let read = read_with(&dir, now, UtcOffset::UTC, &none, &reflog, &none);

    let specs = by_id(read.specs);
    assert_eq!(specs["s"].state, SpecState::Completed);
    // A stage merge alone never completed a spec.
    assert_eq!(specs["t"].state, SpecState::Open);
    assert_eq!(read.notices, Vec::new());
}

#[test]
fn a_merged_file_completes_the_spec_whatever_the_reflog_says() {
    let pod = Pod::new();
    let now = OffsetDateTime::now_utc();
    let dir = staged_spec(&pod, now);
    put_merged(&dir, "s", "2026-10-22T09:00:00Z", COMMIT);
    let none = HashMap::new();
    let stage = [entry("merge s-3: Fast-forward", now)];

    let read = read_with(&dir, now, UtcOffset::UTC, &none, &stage, &none);

    assert_eq!(read.specs[0].state, SpecState::Completed);
    assert_eq!(read.specs[0].state_at, at("2026-10-22T09:00:00Z"));
    assert_eq!(read.notices, Vec::new());
}

#[test]
fn a_file_that_does_not_parse_falls_back_to_the_older_sources_and_is_noted() {
    let pod = Pod::new();
    let dir = pod.root.join("pod");
    for id in ["w", "m", "st"] {
        put_spec(&dir, id);
        put_approval(&dir, id);
    }
    write(
        &dir.join("withdrawn/w.md"),
        "withdrawn: yesterday\nreason: replaced\n",
    );
    write(&dir.join("rejections.md"), "- w, 2026-10-14, too big\n");
    write(
        &dir.join("merged/m.md"),
        "merged: 2026-10-22T09:00:00Z\ncommit: not-a-sha\n",
    );
    write(&dir.join("started/st.md"), "started: 20 October\n");
    let now = OffsetDateTime::now_utc();
    let reflog = [entry("merge m: Fast-forward", now)];

    let read = read_with(
        &dir,
        now,
        UtcOffset::UTC,
        &HashMap::new(),
        &reflog,
        &HashMap::new(),
    );

    let specs = by_id(read.specs);
    // Withdrawn by the rejection line, completed by the reflog, approved without a start.
    assert_eq!(specs["w"].state, SpecState::Withdrawn);
    assert_eq!(specs["w"].withdrawn.as_ref().unwrap().reason, "rejected");
    assert_eq!(specs["m"].state, SpecState::Completed);
    assert_eq!(specs["m"].merged_commit, None);
    assert_eq!(specs["st"].state, SpecState::Approved);
    let mut files: Vec<(&str, &str)> = read
        .notices
        .iter()
        .map(|notice| match notice {
            Notice::Unreadable { id, file, .. } => (id.as_str(), file.as_str()),
            other => panic!("{other:?}"),
        })
        .collect();
    files.sort();
    assert_eq!(
        files,
        [
            ("m", "merged/m.md"),
            ("st", "started/st.md"),
            ("w", "withdrawn/w.md")
        ]
    );
}

#[test]
fn a_withdrawn_file_with_an_unknown_reason_does_not_parse() {
    let pod = Pod::new();
    let dir = pod.root.join("pod");
    put_spec(&dir, "w");
    write(
        &dir.join("withdrawn/w.md"),
        "withdrawn: 2026-10-23T09:00:00Z\nreason: bored\n",
    );

    let read = read_files(&dir);

    assert_eq!(read.specs[0].state, SpecState::Open);
    assert_eq!(read.notices.len(), 1);
}

#[test]
fn the_effort_counts_build_worker_hours_from_the_start_to_the_end_or_to_now() {
    let pod = Pod::new();
    let dir = pod.root.join("pod");
    for id in ["done", "gone", "live"] {
        put_spec(&dir, id);
        put_started(&dir, id, "2026-10-20T10:00:00Z");
    }
    put_merged(&dir, "done", "2026-10-20T12:30:00Z", COMMIT);
    put_withdrawn(&dir, "gone", "2026-10-20T11:00:00Z", "replaced");
    let now = at("2026-10-20T14:00:00Z");

    let read = read_with(
        &dir,
        now,
        UtcOffset::UTC,
        &HashMap::new(),
        &[],
        &HashMap::new(),
    );

    let specs = by_id(read.specs);
    assert_eq!(specs["done"].build_worker_seconds, 9000);
    assert_eq!(specs["gone"].build_worker_seconds, 3600);
    assert_eq!(specs["live"].build_worker_seconds, 14400);
    let hours = |id: &str| {
        effort_measures(&specs[id])
            .into_iter()
            .find(|(measure, _)| *measure == "build_worker_hours")
            .unwrap()
            .1
    };
    assert_eq!(hours("done"), 2.5);
    assert_eq!(hours("live"), 4.0);
}

fn facts_in(state: SpecState, state_at: &str) -> SpecFacts {
    SpecFacts {
        id: format!("spec-{}", state.as_str()),
        state,
        state_at: at(state_at),
        ..SpecFacts::default()
    }
}

#[test]
fn the_effort_gauge_leaves_out_a_spec_that_finished_over_thirty_days_ago() {
    let now = at("2026-10-26T12:00:00Z");
    let specs = [
        facts_in(SpecState::Open, "2026-01-01T00:00:00Z"),
        facts_in(SpecState::Approved, "2026-01-01T00:00:00Z"),
        facts_in(SpecState::InProgress, "2026-01-01T00:00:00Z"),
        SpecFacts {
            id: "recent-done".into(),
            ..facts_in(SpecState::Completed, "2026-10-01T00:00:00Z")
        },
        SpecFacts {
            id: "old-done".into(),
            ..facts_in(SpecState::Completed, "2026-09-25T11:00:00Z")
        },
        SpecFacts {
            id: "old-gone".into(),
            ..facts_in(SpecState::Withdrawn, "2026-08-01T00:00:00Z")
        },
    ];

    let ids: Vec<&str> = effort_specs(&specs, now)
        .into_iter()
        .map(|spec| spec.id.as_str())
        .collect();

    assert_eq!(
        ids,
        [
            "spec-open",
            "spec-approved",
            "spec-in_progress",
            "recent-done"
        ]
    );
}

fn spec(id: &str, state: SpecState, state_at: &str) -> SpecFacts {
    SpecFacts {
        id: id.into(),
        state,
        state_at: at(state_at),
        open_at: Some(at("2026-10-14T10:00:00Z")),
        approved_at: Some(at("2026-10-14T10:31:00Z")),
        words: 120,
        revisions: 1,
        ..SpecFacts::default()
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
fn a_spec_that_completes_is_logged_without_a_lead_time() {
    test_support::install();
    let product = format!("pod-test-{}", uuid::Uuid::new_v4());
    let mut book = SpecBook::default();
    book.observe(
        &product,
        &[spec("g", SpecState::Approved, "2026-10-14T10:31:00Z")],
    );
    let logs = CapturedLogs::start();
    let mut completed = spec("g", SpecState::Completed, "2026-10-14T11:34:00Z");
    completed.merged_at = Some(at("2026-10-14T11:34:00Z"));

    let changes = book.observe(&product, &[completed]);

    assert_eq!(
        changes,
        vec![SpecChange {
            id: "g".to_owned(),
            before: Some(SpecState::Approved),
            state: SpecState::Completed
        }]
    );
    assert_eq!(
        test_support::histogram_sum("pod.spec.lead_time", &[("pod.product", product.as_str())]),
        None
    );
    let text = logs.text();
    assert!(
        text.contains(&format!(
            "INFO harold::pod: spec state changed pod.product={product} spec.id=g state=\"completed\" \
             spec.state_at=2026-10-14T11:34:00Z spec.words=120 spec.revisions=1"
        )),
        "{text}"
    );
    // The same state again is no change.
    assert_eq!(
        book.observe(
            &product,
            &[spec("g", SpecState::Completed, "2026-10-14T11:34:00Z")]
        ),
        Vec::new()
    );
}

#[test]
fn a_step_with_a_missing_time_or_a_negative_length_is_not_recorded() {
    test_support::install();
    let product = format!("pod-test-{}", uuid::Uuid::new_v4());
    let mut facts = spec("g", SpecState::Completed, "2026-10-14T09:00:00Z");
    facts.open_at = None;
    let spec_ref = SpecRef {
        id: "g".into(),
        stage: None,
    };

    // No open time, no started time, and a start that never came: the three steps are
    // each missing a time.
    record_lead_time(
        &product,
        &spec_ref,
        &facts,
        at("2026-10-14T09:00:00Z"),
        true,
    );
    // Started before it was approved: that step would be negative, but the start to
    // completion is fine.
    facts.started_at = Some(at("2026-10-14T10:00:00Z"));
    record_lead_time(
        &product,
        &spec_ref,
        &facts,
        at("2026-10-14T12:00:00Z"),
        true,
    );

    assert_eq!(lead_time_series(&product, "open_to_approved"), Vec::new());
    assert_eq!(
        lead_time_series(&product, "approved_to_started"),
        Vec::new()
    );
    let completed = lead_time_series(&product, "started_to_completed");
    assert_eq!(completed.len(), 1, "{completed:?}");
    assert_eq!(
        test_support::histogram_sum("pod.spec.lead_time", &[("pod.product", product.as_str())]),
        Some(7200.0)
    );
}

#[test]
fn a_change_short_of_completed_is_logged_without_a_lead_time() {
    test_support::install();
    let product = format!("pod-test-{}", uuid::Uuid::new_v4());
    let mut book = SpecBook::default();
    book.observe(
        &product,
        &[spec("g", SpecState::Open, "2026-10-14T10:00:00Z")],
    );

    let changes = book.observe(
        &product,
        &[
            spec("g", SpecState::Approved, "2026-10-14T10:31:00Z"),
            spec("n", SpecState::Open, "2026-10-14T10:40:00Z"),
        ],
    );

    let seen: Vec<_> = changes
        .iter()
        .map(|change| (change.id.as_str(), change.before, change.state))
        .collect();
    assert_eq!(
        seen,
        [
            ("g", Some(SpecState::Open), SpecState::Approved),
            ("n", None, SpecState::Open)
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
    assert_eq!(specs["spec-x"].state, SpecState::Completed);
    assert_eq!(specs["spec-x"].words, 6);
    assert_eq!(specs["spec-y"].state, SpecState::Open);
    let text = logs.text();
    assert_eq!(text.matches("spec state changed").count(), 1, "{text}");
    assert!(
        text.contains(&format!(
            "spec state changed pod.product={} spec.id=spec-x state=\"completed\"",
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
    assert_eq!(building["h"].state, SpecState::InProgress);
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
    assert_eq!(specs["h"].state, SpecState::Completed);
    assert!(specs["h"].state_at >= before);
    assert_eq!(specs["k"].state, SpecState::Open);
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
    assert_eq!(specs["h"].state, SpecState::Completed);
    assert_eq!(specs["k"].state, SpecState::Open);
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
    assert_eq!(specs["m"].state, SpecState::Open);

    // The merge lands a poll later: it counts from when the worktree was seen gone.
    git(&repo, &["merge", "-q", "--ff-only", "m"]);
    stats.poll().await;

    let specs = by_id(snapshot.lock().unwrap().specs[&pod.name].clone());
    assert_eq!(specs["m"].state, SpecState::Completed);
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
    assert_eq!(specs["spec-late"].state, SpecState::Completed);
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
    assert_eq!(specs["spec-u"].state, SpecState::Open);
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

/// Writes the files of a spec `id` that is open and approved: the spec and its approval, then waits out the second, so the whole-second merge time that follows is
/// after the approval's creation time.
async fn approved_spec(repo: &Path, id: &str) {
    let pod = repo.join(".superpowers/pod");
    write(&pod.join(format!("specs/{id}.md")), "# Spec\n");
    write(&pod.join(format!("approvals/{id}.md")), "approved\n");
    tokio::time::sleep(Duration::from_millis(1100)).await;
}

fn keys(attributes: &BTreeMap<String, String>) -> BTreeSet<&str> {
    attributes.keys().map(String::as_str).collect()
}

#[tokio::test]
async fn a_merge_without_files_records_open_to_approved_with_its_spec() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    approved_spec(&repo, "g").await;
    let (mut stats, _) = stats(one_product(&pod, &repo));
    stats.poll().await;

    merge_branch(&repo, "g", "x\n");
    stats.poll().await;

    let files = repo.join(".superpowers/pod");
    let open = created(&files.join("specs/g.md"));
    let approved = created(&files.join("approvals/g.md"));
    assert_eq!(
        test_support::histogram_sum(
            "pod.spec.lead_time",
            &[
                ("pod.product", pod.name.as_str()),
                ("phase", "open_to_approved"),
                ("spec.id", "g"),
                ("spec.revisions", "0"),
            ]
        ),
        Some((approved - open).as_seconds_f64())
    );
    // The older sources know no start, so the later steps have no time.
    assert_eq!(
        lead_time_series(&pod.name, "approved_to_started"),
        Vec::new()
    );
    assert_eq!(
        lead_time_series(&pod.name, "started_to_completed"),
        Vec::new()
    );
}

#[tokio::test]
async fn a_stage_does_not_record_open_to_approved_again() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    approved_spec(&repo, "away-web-app").await;
    let (mut stats, _) = stats(one_product(&pod, &repo));
    stats.poll().await;

    merge_branch(&repo, "away-web-app", "x\n");
    stats.poll().await;
    merge_branch(&repo, "away-web-app-1b", "y\n");
    stats.poll().await;

    let steps = lead_time_series(&pod.name, "open_to_approved");
    assert_eq!(steps.len(), 1, "{steps:?}");
    assert_eq!(steps[0].1, 1);
    assert_eq!(
        keys(&steps[0].0),
        BTreeSet::from(["pod.product", "phase", "spec.revisions", "spec.id"])
    );
}

#[tokio::test]
async fn open_to_approved_is_recorded_on_the_specs_oldest_merge() {
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

    let of = |id: &str| -> Vec<_> {
        lead_time_series(&pod.name, "open_to_approved")
            .into_iter()
            .filter(|(attributes, _)| attributes["spec.id"] == id)
            .collect()
    };
    let steps = of("s");
    assert_eq!(steps.len(), 1, "{steps:?}");
    assert_eq!(steps[0].1, 1);
    assert_eq!(steps[0].0.get("spec.stage").map(String::as_str), Some("1"));
    assert_eq!(of("t").iter().map(|(_, count)| count).sum::<u64>(), 1);
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
        no_agents(),
        None,
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

    let handle = start(path, tips_path(&pod), empty_hub(), None, shutdown_rx);

    let snapshot = gauge_snapshot();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
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
    assert_eq!(count("open"), Some(1));
    for state in ["approved", "in_progress", "completed", "withdrawn"] {
        assert_eq!(count(state), Some(0), "{state}");
    }
    assert_eq!(
        test_support::gauge_value(
            "pod.spec.words",
            &[
                ("pod.product", pod.name.as_str()),
                ("spec.id", "spec-z"),
                ("state", "open")
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

// --- stages, the 7-day rules, the new files and the steps ---

/// The pod folder of `repo`.
fn pod_files(repo: &Path) -> PathBuf {
    repo.join(".superpowers/pod")
}

fn rfc(time: OffsetDateTime) -> String {
    time.format(&time::format_description::well_known::Rfc3339)
        .unwrap()
}

/// Like `merge_branch`, with the reflog entry of the merge dated `days` days ago.
fn merge_branch_days_ago(repo: &Path, branch: &str, text: &str, days: i64) {
    git(repo, &["checkout", "-q", "-b", branch]);
    write(&repo.join("f"), text);
    git(repo, &["commit", "-q", "-a", "-m", branch]);
    git(repo, &["checkout", "-q", "main"]);
    let when = rfc(OffsetDateTime::now_utc() - time::Duration::days(days));
    git_with(
        repo,
        &[("GIT_COMMITTER_DATE", when.as_str())],
        &["merge", "-q", "--ff-only", branch],
    );
}

fn lead_time_total(product: &str) -> Option<f64> {
    test_support::histogram_sum("pod.spec.lead_time", &[("pod.product", product)])
}

#[tokio::test]
async fn a_stage_1_merge_with_a_started_file_leaves_the_spec_in_progress_and_records_no_lead_time()
{
    let pod = Pod::new();
    let repo = pod.repo("main");
    approved_spec(&repo, "s").await;
    put_started(&pod_files(&repo), "s", &rfc(OffsetDateTime::now_utc()));
    let (mut stats, snapshot) = stats(one_product(&pod, &repo));
    stats.poll().await;

    merge_branch(&repo, "s-1", "x\n");
    stats.poll().await;
    stats.poll().await;

    let specs = by_id(snapshot.lock().unwrap().specs[&pod.name].clone());
    assert_eq!(specs["s"].state, SpecState::InProgress);
    assert_eq!(lead_time_total(&pod.name), None);
    // The merge itself is still counted under its spec and stage.
    assert_eq!(
        test_support::counter_value(
            "pod.merges",
            &[
                ("pod.product", pod.name.as_str()),
                ("spec.id", "s"),
                ("spec.stage", "1")
            ]
        ),
        Some(1)
    );
}

#[tokio::test]
async fn a_merge_without_a_stage_over_seven_days_old_completes_the_spec_and_logs_once() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    approved_spec(&repo, "s").await;
    let files = pod_files(&repo);
    put_started(
        &files,
        "s",
        &rfc(OffsetDateTime::now_utc() - time::Duration::days(20)),
    );
    let (mut stats, snapshot) = stats(one_product(&pod, &repo));
    stats.poll().await;
    let logs = CapturedLogs::start();

    merge_branch_days_ago(&repo, "s", "x\n", 8);
    stats.poll().await;
    stats.poll().await;

    let specs = by_id(snapshot.lock().unwrap().specs[&pod.name].clone());
    assert_eq!(specs["s"].state, SpecState::Completed);
    let text = logs.text();
    let line = format!(
        "spec completed without merged file pod.product={} spec.id=s",
        pod.name
    );
    assert_eq!(text.matches(&line).count(), 1, "{text}");
    // The time is a guess, so no lead time is recorded.
    assert_eq!(lead_time_total(&pod.name), None);
}

#[tokio::test]
async fn a_stage_merge_over_seven_days_old_logs_once_and_the_spec_stays_in_progress() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    approved_spec(&repo, "s").await;
    let files = pod_files(&repo);
    put_started(
        &files,
        "s",
        &rfc(OffsetDateTime::now_utc() - time::Duration::days(20)),
    );
    let (mut stats, snapshot) = stats(one_product(&pod, &repo));
    stats.poll().await;
    let logs = CapturedLogs::start();

    merge_branch_days_ago(&repo, "s-1b", "x\n", 8);
    stats.poll().await;
    stats.poll().await;

    let specs = by_id(snapshot.lock().unwrap().specs[&pod.name].clone());
    assert_eq!(specs["s"].state, SpecState::InProgress);
    let text = logs.text();
    let line = format!(
        "spec stage merged without merged file pod.product={} spec.id=s spec.stage=1b",
        pod.name
    );
    assert_eq!(text.matches(&line).count(), 1, "{text}");
    assert!(
        !text.contains("spec completed without merged file"),
        "{text}"
    );
}

#[tokio::test]
async fn a_stage_merge_under_seven_days_old_logs_nothing_though_the_spec_started_earlier() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    approved_spec(&repo, "s").await;
    let files = pod_files(&repo);
    put_started(
        &files,
        "s",
        &rfc(OffsetDateTime::now_utc() - time::Duration::days(20)),
    );
    let (mut stats, snapshot) = stats(one_product(&pod, &repo));
    stats.poll().await;
    let logs = CapturedLogs::start();

    merge_branch_days_ago(&repo, "s-1", "x\n", 2);
    stats.poll().await;

    let specs = by_id(snapshot.lock().unwrap().specs[&pod.name].clone());
    assert_eq!(specs["s"].state, SpecState::InProgress);
    let text = logs.text();
    assert!(!text.contains("without merged file"), "{text}");
}

#[tokio::test]
async fn a_pod_file_that_does_not_parse_is_logged_once_per_change_without_its_path() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    let files = pod_files(&repo);
    put_spec(&files, "s");
    write(&files.join("merged/s.md"), "merged: soon\n");
    let (mut stats, snapshot) = stats(one_product(&pod, &repo));
    let logs = CapturedLogs::start();

    stats.poll().await;
    stats.poll().await;

    let line = format!(
        "pod file unreadable pod.product={} spec.id=s file=merged/s.md",
        pod.name
    );
    let text = logs.text();
    assert_eq!(text.matches(&line).count(), 1, "{text}");
    assert!(
        !text.contains(repo.to_str().unwrap()),
        "the log names a full path: {text}"
    );
    assert_eq!(
        snapshot.lock().unwrap().specs[&pod.name][0].state,
        SpecState::Open
    );

    write(&files.join("merged/s.md"), "merged: later\n");
    stats.poll().await;
    stats.poll().await;
    assert_eq!(logs.text().matches(&line).count(), 2);

    // Once it parses it is read as the spec's merged file.
    put_merged(&files, "s", "2026-10-22T09:00:00Z", COMMIT);
    stats.poll().await;
    assert_eq!(
        snapshot.lock().unwrap().specs[&pod.name][0].state,
        SpecState::Completed
    );
    assert_eq!(logs.text().matches(&line).count(), 2);
}

/// The start, approval and merge of spec `g` for the lead time tests, which waits out the
/// second after writing the files. Returns the merged file's commit and the times of its
/// started and merged files.
async fn spec_with_started_file(repo: &Path) -> (OffsetDateTime, OffsetDateTime) {
    approved_spec(repo, "g").await;
    let approved = created(&pod_files(repo).join("approvals/g.md"));
    let started = approved + time::Duration::hours(1);
    put_started(&pod_files(repo), "g", &rfc(started));
    (started, started + time::Duration::hours(2))
}

#[tokio::test]
async fn a_merged_file_records_the_three_steps_once() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    let (started, merged) = spec_with_started_file(&repo).await;
    let (mut stats, _) = stats(one_product(&pod, &repo));
    stats.poll().await;

    merge_branch(&repo, "g", "x\n");
    let commit = git(&repo, &["rev-parse", "g"]);
    put_merged(&pod_files(&repo), "g", &rfc(merged), &commit);
    stats.poll().await;
    stats.poll().await;

    let files = pod_files(&repo);
    let open = created(&files.join("specs/g.md"));
    let approved = created(&files.join("approvals/g.md"));
    let step = |phase: &str| {
        test_support::histogram_sum(
            "pod.spec.lead_time",
            &[
                ("pod.product", pod.name.as_str()),
                ("phase", phase),
                ("spec.id", "g"),
            ],
        )
    };
    assert_eq!(
        step("open_to_approved"),
        Some((approved - open).as_seconds_f64())
    );
    assert_eq!(
        step("approved_to_started"),
        Some((started - approved).as_seconds_f64())
    );
    assert_eq!(step("started_to_completed"), Some(7200.0));
    for phase in [
        "open_to_approved",
        "approved_to_started",
        "started_to_completed",
    ] {
        let series = lead_time_series(&pod.name, phase);
        assert_eq!(
            series.iter().map(|(_, count)| count).sum::<u64>(),
            1,
            "{phase}"
        );
        assert_eq!(
            keys(&series[0].0),
            BTreeSet::from(["pod.product", "phase", "spec.revisions", "spec.id"])
        );
    }
    assert_eq!(
        test_support::counter_value("pod.merges", &[("pod.product", pod.name.as_str())]),
        Some(1)
    );
}

#[tokio::test]
async fn a_merged_file_that_comes_after_the_poll_that_saw_the_merge_still_records_once() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    let (_, merged) = spec_with_started_file(&repo).await;
    let (mut stats, _) = stats(one_product(&pod, &repo));
    stats.poll().await;
    merge_branch(&repo, "g", "x\n");
    let commit = git(&repo, &["rev-parse", "g"]);
    stats.poll().await;
    assert_eq!(lead_time_total(&pod.name), None);

    put_merged(&pod_files(&repo), "g", &rfc(merged), &commit);
    stats.poll().await;
    stats.poll().await;

    let series = lead_time_series(&pod.name, "started_to_completed");
    assert_eq!(series.iter().map(|(_, count)| count).sum::<u64>(), 1);
}

#[tokio::test]
async fn completed_specs_at_a_products_first_poll_and_after_a_restart_record_no_lead_time() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    let (_, merged) = spec_with_started_file(&repo).await;
    merge_branch(&repo, "g", "x\n");
    let commit = git(&repo, &["rev-parse", "g"]);
    put_merged(&pod_files(&repo), "g", &rfc(merged), &commit);
    let registry = one_product(&pod, &repo);
    let (mut first, snapshot) = stats(registry.clone());

    first.poll().await;
    first.poll().await;

    assert_eq!(
        snapshot.lock().unwrap().specs[&pod.name][0].state,
        SpecState::Completed
    );
    assert_eq!(lead_time_total(&pod.name), None);

    // A restart: the tip file is in place, and the spec's commit is in the counted tip.
    let (mut restarted, _) = stats(registry);
    restarted.poll().await;
    restarted.poll().await;
    assert_eq!(lead_time_total(&pod.name), None);
}

#[tokio::test]
async fn a_merge_while_harold_was_stopped_records_its_lead_time_at_the_next_start() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    let (_, merged) = spec_with_started_file(&repo).await;
    let registry = one_product(&pod, &repo);
    let (mut first, _) = stats(registry.clone());
    first.poll().await;
    drop(first);

    merge_branch(&repo, "g", "x\n");
    let commit = git(&repo, &["rev-parse", "g"]);
    put_merged(&pod_files(&repo), "g", &rfc(merged), &commit);
    let (mut restarted, _) = stats(registry);
    restarted.poll().await;
    restarted.poll().await;

    let series = lead_time_series(&pod.name, "started_to_completed");
    assert_eq!(series.iter().map(|(_, count)| count).sum::<u64>(), 1);
}

#[tokio::test]
async fn a_merged_file_with_no_started_file_skips_the_steps_it_has_no_time_for() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    approved_spec(&repo, "g").await;
    let (mut stats, _) = stats(one_product(&pod, &repo));
    stats.poll().await;

    merge_branch(&repo, "g", "x\n");
    let commit = git(&repo, &["rev-parse", "g"]);
    put_merged(
        &pod_files(&repo),
        "g",
        &rfc(OffsetDateTime::now_utc()),
        &commit,
    );
    stats.poll().await;

    assert_eq!(lead_time_series(&pod.name, "open_to_approved").len(), 1);
    assert_eq!(
        lead_time_series(&pod.name, "approved_to_started"),
        Vec::new()
    );
    assert_eq!(
        lead_time_series(&pod.name, "started_to_completed"),
        Vec::new()
    );
}

fn live(pane_id: &str, folder: &str) -> LivePane {
    LivePane {
        pane_id: pane_id.into(),
        folder: folder.into(),
        state: EffectiveAgentState::Working,
    }
}

/// A product with the repo folder `repo`, for the role lookup.
fn product_at(name: &str, repo: &Path) -> Product {
    Product {
        name: name.into(),
        repo: repo.to_path_buf(),
        base: "main".into(),
        slots: 1,
        paused: false,
    }
}

#[test]
fn pod_pane_roles_reads_line_one_as_coordinator_and_line_two_as_pnem() {
    let pod = Pod::new();
    let repo = pod.root.join("alpha");
    write(&pod_files(&repo).join("agent-panes"), "%1 x\n%2\n%3\n");

    let roles = pod_pane_roles(&[product_at("alpha", &repo)]);

    assert_eq!(
        roles,
        vec![
            ("%1".to_owned(), Role::PodCoordination, "alpha".to_owned()),
            ("%2".to_owned(), Role::Pnem, "alpha".to_owned()),
        ]
    );
}

#[test]
fn claim_pod_panes_and_pod_pane_roles_agree() {
    let pod = Pod::new();
    let repo = pod.root.join("alpha");
    // `%3` is not a pod pane. `%2` is named but not live.
    write(&pod_files(&repo).join("agent-panes"), "%1 x\n%2\n%3\n");
    let product = product_at("alpha", &repo);
    let panes = vec![live("%1", "/a"), live("%3", "/c")];
    let mut claimed = HashMap::new();

    assert!(claim_pod_panes(&product, &panes, &mut claimed));

    let roles = pod_pane_roles(std::slice::from_ref(&product));
    assert_eq!(roles.len(), 2);
    for (pane, role, name) in roles {
        let is_live = panes.iter().any(|live| live.pane_id == pane);
        let expected = is_live.then_some((role, Some(name)));
        assert_eq!(claimed.get(&pane), expected.as_ref());
    }
    assert_eq!(claimed.len(), 1);
}

/// The roles that a text in an `agent-panes` file gives.
fn roles_of_text(text: &str) -> Vec<(String, Role)> {
    let pod = Pod::new();
    let repo = pod.root.join("alpha");
    write(&pod_files(&repo).join("agent-panes"), text);
    pod_pane_roles(&[product_at("alpha", &repo)])
        .into_iter()
        .map(|(pane, role, _)| (pane, role))
        .collect()
}

#[test]
fn a_blank_line_is_not_a_line_of_agent_panes() {
    assert_eq!(
        roles_of_text("\n%1\n\n%2\n%3\n"),
        vec![
            ("%1".to_owned(), Role::PodCoordination),
            ("%2".to_owned(), Role::Pnem)
        ]
    );
}

#[test]
fn one_line_gives_only_the_coordinator() {
    assert_eq!(
        roles_of_text("%1\n"),
        vec![("%1".to_owned(), Role::PodCoordination)]
    );
}

#[test]
fn a_third_line_gives_no_role() {
    assert_eq!(
        roles_of_text("%1\n%2\n%3\n%4\n"),
        vec![
            ("%1".to_owned(), Role::PodCoordination),
            ("%2".to_owned(), Role::Pnem)
        ]
    );
}

#[test]
fn a_pane_named_in_two_products_goes_to_the_first_product() {
    let pod = Pod::new();
    let first = pod.root.join("alpha");
    let second = pod.root.join("beta");
    write(&pod_files(&first).join("agent-panes"), "%1\n%2\n");
    write(&pod_files(&second).join("agent-panes"), "%2\n%1\n");
    let products = [product_at("alpha", &first), product_at("beta", &second)];
    let panes = vec![live("%1", "/a"), live("%2", "/b")];

    let mut claimed = HashMap::new();
    for product in &products {
        claim_pod_panes(product, &panes, &mut claimed);
    }

    assert_eq!(
        claimed.get("%1"),
        Some(&(Role::PodCoordination, Some("alpha".to_owned())))
    );
    assert_eq!(
        claimed.get("%2"),
        Some(&(Role::Pnem, Some("alpha".to_owned())))
    );
    assert_eq!(claimed.len(), 2);
}

#[test]
fn a_missing_agent_panes_file_gives_no_roles() {
    let pod = Pod::new();
    let repo = pod.root.join("alpha");
    assert_eq!(pod_pane_roles(&[product_at("alpha", &repo)]), Vec::new());
}

#[tokio::test]
async fn a_pod_runs_when_a_pane_of_its_agent_panes_file_is_live_and_agents_count_by_role() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    pod.worktree(&repo, "w", true);
    write(&pod_files(&repo).join("agent-panes"), "%80\n%81\n");
    let panes = vec![
        live("%80", "/pods/harold"),
        live("%7", "/pods/harold-w"),
        live("%99", "/work/chief/"),
        live("%55", "/work/elsewhere"),
    ];
    let agents: AgentSource = Arc::new(move || panes.clone());
    let (mut stats, snapshot) =
        stats_with_agents(one_product(&pod, &repo), agents, Some("/work/chief"));

    stats.poll().await;

    let snapshot = snapshot.lock().unwrap().clone();
    assert_eq!(snapshot.pods.get(&pod.name), Some(&true));
    let count = |role, product: Option<&str>| {
        snapshot
            .agents
            .get(&(role, product.map(str::to_owned)))
            .copied()
    };
    let product = Some(pod.name.as_str());
    assert_eq!(count(Role::PodCoordination, product), Some(1));
    // `%81` is not live: the PnEM counts 0, not nothing.
    assert_eq!(count(Role::Pnem, product), Some(0));
    assert_eq!(count(Role::BuildWorker, product), Some(1));
    assert_eq!(count(Role::Chief, None), Some(1));
    assert_eq!(count(Role::Other, None), Some(1));
}

const ALL_STATES: [EffectiveAgentState; 6] = [
    EffectiveAgentState::Working,
    EffectiveAgentState::Blocked,
    EffectiveAgentState::Done,
    EffectiveAgentState::Idle,
    EffectiveAgentState::Error,
    EffectiveAgentState::Unknown,
];

#[tokio::test]
async fn pod_agents_report_each_of_the_six_states() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    let ids = ["%1", "%2", "%3", "%4", "%5", "%6"];
    for (n, id) in ids.iter().enumerate() {
        let path = pod.worktree(&repo, &format!("w{n}"), false);
        write(
            &path.join(".superpowers/baton/agent-pane"),
            &format!("{id}\n"),
        );
    }
    let panes: Vec<LivePane> = ids
        .iter()
        .zip(ALL_STATES)
        .map(|(id, state)| LivePane {
            state,
            ..live(id, "/pods/elsewhere")
        })
        .collect();
    let agents: AgentSource = Arc::new(move || panes.clone());
    let (mut stats, snapshot) = stats_with_agents(one_product(&pod, &repo), agents, None);

    stats.poll().await;

    let snapshot = snapshot.lock().unwrap().clone();
    let product = Some(pod.name.clone());
    for state in ALL_STATES {
        assert_eq!(
            snapshot.agent_states[&(Role::BuildWorker, product.clone(), state)],
            1,
            "{state:?}"
        );
        assert_eq!(
            snapshot.agent_states[&(Role::Pnem, product.clone(), state)],
            0
        );
    }
    for state in ALL_STATES {
        assert_eq!(snapshot.agent_states[&(Role::Chief, None, state)], 0);
        assert_eq!(snapshot.agent_states[&(Role::Other, None, state)], 0);
    }
}

#[test]
fn an_unknown_state_word_is_reported_as_unknown() {
    let state: EffectiveAgentState = serde_json::from_str("\"Mystery\"").unwrap();
    assert_eq!(state, EffectiveAgentState::Unknown);
    assert_eq!(state.as_str(), "unknown");
}

#[tokio::test]
async fn a_pod_with_no_live_pane_is_stopped() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    write(&pod_files(&repo).join("agent-panes"), "%80\n%81\n");
    let agents: AgentSource = Arc::new(|| vec![live("%12", "/work/elsewhere")]);
    let (mut stats, snapshot) = stats_with_agents(one_product(&pod, &repo), agents, None);

    stats.poll().await;

    let snapshot = snapshot.lock().unwrap().clone();
    assert_eq!(snapshot.pods.get(&pod.name), Some(&false));
    assert_eq!(snapshot.agents[&(Role::Other, None)], 1);
    assert_eq!(snapshot.agents[&(Role::Chief, None)], 0);
}

#[tokio::test]
async fn a_pod_without_an_agent_panes_file_is_stopped() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    let agents: AgentSource = Arc::new(|| vec![live("%80", "/work/elsewhere")]);
    let (mut stats, snapshot) = stats_with_agents(one_product(&pod, &repo), agents, None);

    stats.poll().await;

    assert_eq!(snapshot.lock().unwrap().pods.get(&pod.name), Some(&false));
}

#[tokio::test]
async fn the_pod_effort_and_agent_gauges_report_what_the_snapshot_holds() {
    let _gauge_snapshot = GAUGE_SNAPSHOT_TESTS.lock().await;
    let product = format!("pod-test-{}", uuid::Uuid::new_v4());
    let now = OffsetDateTime::now_utc();
    let withdrawn = SpecFacts {
        id: "gone".into(),
        state: SpecState::Withdrawn,
        state_at: now - time::Duration::days(2),
        withdrawn: Some(Withdrawal {
            reason: "replaced",
            reached: Reached::InProgress,
        }),
        revisions: 3,
        direction_revisions: 2,
        architect_rounds: 1,
        build_worker_seconds: 5400,
        ..SpecFacts::default()
    };
    let old = SpecFacts {
        id: "old".into(),
        state: SpecState::Completed,
        state_at: now - time::Duration::days(40),
        ..SpecFacts::default()
    };
    let snapshot = gauge_snapshot();
    *snapshot.lock().unwrap() = PodSnapshot {
        specs: BTreeMap::from([(product.clone(), vec![withdrawn, old])]),
        pods: BTreeMap::from([(product.clone(), false)]),
        agents: BTreeMap::from([
            ((Role::BuildWorker, Some(product.clone())), 2),
            ((Role::Chief, None), 1),
        ]),
        agent_states: BTreeMap::from([
            (
                (
                    Role::BuildWorker,
                    Some(product.clone()),
                    EffectiveAgentState::Working,
                ),
                2,
            ),
            ((Role::Chief, None, EffectiveAgentState::Working), 1),
        ]),
        ..PodSnapshot::default()
    };

    let effort = |spec: &str, measure: &str| {
        test_support::gauge_f64_value(
            "pod.spec.effort",
            &[
                ("pod.product", product.as_str()),
                ("spec.id", spec),
                ("measure", measure),
                ("state", "withdrawn"),
                ("withdrawn.reason", "replaced"),
                ("withdrawn.reached", "in_progress"),
            ],
        )
    };
    assert_eq!(effort("gone", "revisions"), Some(3.0));
    assert_eq!(effort("gone", "direction_revisions"), Some(2.0));
    assert_eq!(effort("gone", "architect_rounds"), Some(1.0));
    assert_eq!(effort("gone", "build_worker_hours"), Some(1.5));
    assert_eq!(
        test_support::gauge_f64_value(
            "pod.spec.effort",
            &[("pod.product", product.as_str()), ("spec.id", "old")]
        ),
        None
    );
    let pods = |state: &str| {
        test_support::gauge_value(
            "pod.pods",
            &[("pod.product", product.as_str()), ("state", state)],
        )
    };
    assert_eq!((pods("running"), pods("stopped")), (Some(0), Some(1)));
    assert_eq!(
        test_support::gauge_value(
            "pod.agents",
            &[
                ("pod.product", product.as_str()),
                ("role", "build_worker"),
                ("state", "working")
            ]
        ),
        Some(2)
    );
    assert_eq!(
        test_support::gauge_value("pod.agents", &[("role", "chief")]),
        Some(1)
    );

    *snapshot.lock().unwrap() = PodSnapshot::default();
}

#[tokio::test]
async fn the_effort_gauge_reports_redo_hours_and_revised_after_approval_with_the_size() {
    let _gauge_snapshot = GAUGE_SNAPSHOT_TESTS.lock().await;
    let product = format!("pod-test-{}", uuid::Uuid::new_v4());
    let spec = SpecFacts {
        id: "big".into(),
        state: SpecState::InProgress,
        state_at: OffsetDateTime::now_utc(),
        redo_seconds: 5400,
        revised_after_approval: 2,
        size: SpecSize::Large,
        ..SpecFacts::default()
    };
    assert_eq!(
        extra_effort_measures(&spec),
        [("redo_hours", 1.5), ("revised_after_approval", 2.0)]
    );
    let snapshot = gauge_snapshot();
    *snapshot.lock().unwrap() = PodSnapshot {
        specs: BTreeMap::from([(product.clone(), vec![spec])]),
        ..PodSnapshot::default()
    };

    let effort = |measure: &str, size: &str| {
        test_support::gauge_f64_value(
            "pod.spec.effort",
            &[
                ("pod.product", product.as_str()),
                ("spec.id", "big"),
                ("measure", measure),
                ("state", "in_progress"),
                ("spec.size", size),
            ],
        )
    };
    assert_eq!(effort("redo_hours", "large"), Some(1.5));
    assert_eq!(effort("revised_after_approval", "large"), Some(2.0));
    assert_eq!(effort("revisions", "large"), Some(0.0));
    assert_eq!(effort("redo_hours", "small"), None);

    *snapshot.lock().unwrap() = PodSnapshot::default();
}

#[tokio::test]
async fn a_large_spec_that_is_approved_is_logged_once() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    let files = pod_files(&repo);
    write(&files.join("specs/big.md"), "# Big\n\n**Size.** large\n");
    put_approval(&files, "big");
    let logs = CapturedLogs::start();
    let (mut stats, _) = stats(one_product(&pod, &repo));
    stats.poll().await;
    stats.poll().await;
    let text = logs.text();
    let line = format!("large spec approved pod.product={} spec.id=big", pod.name);
    assert_eq!(text.matches(&line).count(), 1, "{text}");
}

#[tokio::test]
async fn a_large_spec_that_is_not_approved_is_not_logged() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    let files = pod_files(&repo);
    write(&files.join("specs/big.md"), "# Big\n\n**Size.** large\n");
    write(
        &files.join("specs/small.md"),
        "# Small\n\n**Size.** small\n",
    );
    put_approval(&files, "small");
    let logs = CapturedLogs::start();
    let (mut stats, _) = stats(one_product(&pod, &repo));
    stats.poll().await;
    stats.poll().await;
    assert!(
        !logs.text().contains("large spec approved"),
        "{}",
        logs.text()
    );
}

fn large_line(pod: &Pod) -> String {
    format!("large spec approved pod.product={} spec.id=big", pod.name)
}

fn large_key(pod: &Pod) -> String {
    format!("{}\0large\0big", pod.name)
}

#[tokio::test]
async fn a_restart_does_not_log_a_large_spec_again() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    let files = pod_files(&repo);
    write(&files.join("specs/big.md"), "# Big\n\n**Size.** large\n");
    put_approval(&files, "big");
    let registry = one_product(&pod, &repo);
    let logs = CapturedLogs::start();
    let (mut first, _) = stats(registry.clone());
    first.poll().await;
    assert_eq!(logs.text().matches(&large_line(&pod)).count(), 1);
    assert_eq!(notice_keys(&pod), vec![large_key(&pod)]);
    drop(first);

    let restarted = CapturedLogs::start();
    let (mut second, _) = stats(registry);
    second.poll().await;
    let text = restarted.text();
    assert_eq!(text.matches(&large_line(&pod)).count(), 0, "{text}");
    assert_eq!(notice_keys(&pod), vec![large_key(&pod)]);
}

#[tokio::test]
async fn a_restart_over_a_notices_file_with_the_large_key_does_not_log() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    let files = pod_files(&repo);
    write(&files.join("specs/big.md"), "# Big\n\n**Size.** large\n");
    put_approval(&files, "big");
    let saved: BTreeMap<String, String> =
        BTreeMap::from([(large_key(&pod), "2026-11-01T00:00:00Z".to_owned())]);
    write(&notices_path(&pod), &serde_json::to_string(&saved).unwrap());
    let logs = CapturedLogs::start();
    let (mut stats, _) = stats(one_product(&pod, &repo));
    stats.poll().await;
    let text = logs.text();
    assert_eq!(text.matches(&large_line(&pod)).count(), 0, "{text}");
    assert!(!text.contains("pod notices unreadable"), "{text}");
    assert_eq!(notice_keys(&pod), vec![large_key(&pod)]);
}

#[tokio::test]
async fn the_large_key_leaves_when_the_spec_starts() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    let files = pod_files(&repo);
    write(&files.join("specs/big.md"), "# Big\n\n**Size.** large\n");
    put_approval(&files, "big");
    let (mut stats, _) = stats(one_product(&pod, &repo));
    stats.poll().await;
    assert_eq!(notice_keys(&pod), vec![large_key(&pod)]);
    put_started(&files, "big", "2026-10-15T10:00:00Z");
    stats.poll().await;
    assert_eq!(notice_keys(&pod), Vec::<String>::new());
}

async fn large_spec_log_count(prepare: impl FnOnce(&Path)) -> usize {
    let pod = Pod::new();
    let repo = pod.repo("main");
    let files = pod_files(&repo);
    prepare(&files);
    let logs = CapturedLogs::start();
    let (mut stats, _) = stats(one_product(&pod, &repo));
    stats.poll().await;
    stats.poll().await;
    let line = format!("large spec approved pod.product={} spec.id=big", pod.name);
    logs.text().matches(&line).count()
}

#[tokio::test]
async fn a_completed_large_spec_is_not_logged() {
    let count = large_spec_log_count(|files| {
        write(&files.join("specs/big.md"), "# Big\n\n**Size.** large\n");
        put_started(files, "big", "2026-10-15T10:00:00Z");
        put_merged(files, "big", "2026-10-16T10:00:00Z", COMMIT);
        std::thread::sleep(Duration::from_millis(100));
        put_approval(files, "big");
    })
    .await;
    assert_eq!(count, 0);
}

#[tokio::test]
async fn an_in_progress_large_spec_is_not_logged() {
    let count = large_spec_log_count(|files| {
        write(&files.join("specs/big.md"), "# Big\n\n**Size.** large\n");
        put_approval(files, "big");
        put_started(files, "big", "2026-10-15T10:00:00Z");
    })
    .await;
    assert_eq!(count, 0);
}

#[tokio::test]
async fn a_large_spec_approved_before_the_first_merged_file_is_not_logged() {
    let count = large_spec_log_count(|files| {
        write(&files.join("specs/big.md"), "# Big\n\n**Size.** large\n");
        put_approval(files, "big");
        std::thread::sleep(Duration::from_millis(100));
        write(&files.join("merged/old.md"), "merged\n");
    })
    .await;
    assert_eq!(count, 0);
}

#[tokio::test]
async fn a_large_spec_approved_after_the_first_merged_file_is_logged_once() {
    let count = large_spec_log_count(|files| {
        write(&files.join("merged/old.md"), "merged\n");
        std::thread::sleep(Duration::from_millis(100));
        write(&files.join("specs/big.md"), "# Big\n\n**Size.** large\n");
        put_approval(files, "big");
    })
    .await;
    assert_eq!(count, 1);
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
        no_agents(),
        None,
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
        no_agents(),
        None,
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

// --- revision entries and redo ---

/// A local offset of eleven hours east of UTC for the heading tests.
fn plus_eleven() -> UtcOffset {
    UtcOffset::from_hms(11, 0, 0).unwrap()
}

/// A revision entry at `time` with `tags`.
fn revision(time: Option<OffsetDateTime>, tags: &[&str]) -> RevisionEntry {
    RevisionEntry {
        at: time,
        tags: tags.iter().map(|tag| (*tag).to_owned()).collect(),
    }
}

#[test]
fn a_heading_with_a_time_and_one_with_a_date_only_both_parse() {
    let text = "# Revisions\n\
        ## 2026-11-02 14:30, rev 2 (direction)\n\
        ## 2026-11-02, rev 2 (defect)\n";

    let entries = revision_entries(text, plus_eleven());

    assert_eq!(
        entries,
        vec![
            revision(Some(at("2026-11-02T14:30:00+11:00")), &["direction"]),
            revision(Some(at("2026-11-02T00:00:00+11:00")), &["defect"]),
        ]
    );
}

#[test]
fn wording_counts_as_a_revision_and_is_never_redo() {
    let text = "## 2026-11-02 10:00, rev 2 (wording)\n";
    let marks = revision_marks(text);
    let started = at("2026-11-01T00:00:00Z");

    assert_eq!(marks.revisions, 1);
    assert_eq!(marks.direction, 0);
    let entries = revision_entries(text, UtcOffset::UTC);
    assert_eq!(
        redo_seconds(&entries, Some(started), started + time::Duration::days(5)),
        0
    );
}

#[test]
fn redo_runs_from_the_earliest_direction_or_defect_revision_after_the_start_to_the_end() {
    let start = at("2026-11-02T00:00:00Z");
    let entries = [
        revision(Some(start + time::Duration::hours(3)), &["direction"]),
        revision(Some(start + time::Duration::hours(1)), &["defect"]),
    ];

    let seconds = redo_seconds(&entries, Some(start), start + time::Duration::hours(5));

    assert_eq!(seconds, 4 * 3600);
}

#[test]
fn a_revision_between_approval_and_start_is_not_redo() {
    let start = at("2026-11-02T00:00:00Z");
    let entries = [revision(
        Some(start - time::Duration::hours(2)),
        &["direction"],
    )];

    assert_eq!(
        redo_seconds(&entries, Some(start), start + time::Duration::hours(5)),
        0
    );
}

#[test]
fn redo_runs_to_now_for_a_spec_in_progress() {
    let start = at("2026-11-02T00:00:00Z");
    let now = start + time::Duration::hours(9);
    let entries = [revision(
        Some(start + time::Duration::hours(4)),
        &["defect"],
    )];

    assert_eq!(redo_seconds(&entries, Some(start), now), 5 * 3600);
}

#[test]
fn a_revision_with_no_time_or_an_unknown_tag_is_never_redo() {
    let start = at("2026-11-02T00:00:00Z");
    let end = start + time::Duration::hours(5);
    let entries = [
        revision(None, &["direction"]),
        revision(Some(start + time::Duration::hours(1)), &["mystery"]),
        revision(Some(start + time::Duration::hours(1)), &[]),
    ];

    assert_eq!(redo_seconds(&entries, Some(start), end), 0);
    assert_eq!(redo_seconds(&entries[..1], None, end), 0);
    let bad_time = revision_entries("## 2026-11-02 25:99, rev 2 (direction)\n", UtcOffset::UTC);
    assert_eq!(bad_time, vec![revision(None, &["direction"])]);
}

#[test]
fn a_revision_dated_after_the_end_gives_0_not_negative() {
    let start = at("2026-11-02T00:00:00Z");
    let entries = [revision(
        Some(start + time::Duration::hours(8)),
        &["direction"],
    )];

    assert_eq!(
        redo_seconds(&entries, Some(start), start + time::Duration::hours(5)),
        0
    );
}

#[test]
fn revised_after_approval_counts_any_tag_dated_after_the_approval() {
    let approved = at("2026-11-02T00:00:00Z");
    let entries = [
        revision(Some(approved + time::Duration::hours(1)), &["first"]),
        revision(Some(approved + time::Duration::hours(2)), &["wording"]),
        revision(Some(approved + time::Duration::hours(3)), &["architect"]),
        revision(Some(approved - time::Duration::hours(3)), &["direction"]),
        revision(None, &["direction"]),
    ];

    assert_eq!(revised_after_approval(&entries, Some(approved)), 2);
    assert_eq!(revised_after_approval(&entries, None), 0);
}

// --- spec size and waiting line ---

#[test]
fn size_line_in_each_form() {
    assert_eq!(spec_size("# T\n\n**Size.** small\n"), SpecSize::Small);
    assert_eq!(spec_size("# T\n**Size.** medium\n"), SpecSize::Medium);
    assert_eq!(spec_size("**Size.** large"), SpecSize::Large);
    assert_eq!(spec_size("# T\n**Size.** Medium\n"), SpecSize::Medium);
    assert_eq!(
        spec_size("# T\n**Size.** large, because it moves the store.\n"),
        SpecSize::Large
    );
    assert_eq!(SpecSize::Small.as_str(), "small");
    assert_eq!(SpecSize::Medium.as_str(), "medium");
    assert_eq!(SpecSize::Large.as_str(), "large");
    assert_eq!(SpecSize::Unsized.as_str(), "unsized");
}

#[test]
fn no_size_line_is_unsized() {
    assert_eq!(spec_size("# T\n\nSome text.\n"), SpecSize::Unsized);
    assert_eq!(spec_size(""), SpecSize::Unsized);
}

#[test]
fn an_unknown_size_word_is_unsized() {
    assert_eq!(spec_size("**Size.** huge\n"), SpecSize::Unsized);
    assert_eq!(spec_size("**Size.**\n"), SpecSize::Unsized);
}

#[test]
fn kind_line_reads_each_kind() {
    assert_eq!(
        spec_kind("# T\n**Size.** small\n**Kind.** feature\n"),
        SpecKind::Feature
    );
    assert_eq!(spec_kind("**Kind.** fix"), SpecKind::Fix);
    assert_eq!(spec_kind("**Kind.** other\n"), SpecKind::Other);
    assert_eq!(SpecKind::Feature.as_str(), "feature");
    assert_eq!(SpecKind::Fix.as_str(), "fix");
    assert_eq!(SpecKind::Other.as_str(), "other");
    assert_eq!(SpecKind::Unkinded.as_str(), "unkinded");
}

#[test]
fn kind_line_ignores_case_and_trailing_text() {
    assert_eq!(spec_kind("**Kind.** Feature\n"), SpecKind::Feature);
    assert_eq!(
        spec_kind("**Kind.** fix (a bug in the store).\n"),
        SpecKind::Fix
    );
    assert_eq!(
        spec_kind("**Kind.** product feature (`feature`): one use is a load.\n"),
        SpecKind::Unkinded
    );
}

#[test]
fn no_kind_line_or_an_unknown_word_is_unkinded() {
    assert_eq!(spec_kind("# T\n\nSome text.\n"), SpecKind::Unkinded);
    assert_eq!(spec_kind(""), SpecKind::Unkinded);
    assert_eq!(spec_kind("**Kind.** chore\n"), SpecKind::Unkinded);
    assert_eq!(spec_kind("**Kind.**\n"), SpecKind::Unkinded);
    assert_eq!(SpecFacts::default().kind, SpecKind::Unkinded);
}

#[tokio::test]
async fn the_effort_gauge_carries_the_spec_kind() {
    let _gauge_snapshot = GAUGE_SNAPSHOT_TESTS.lock().await;
    let product = format!("pod-test-{}", uuid::Uuid::new_v4());
    let spec = SpecFacts {
        id: "kinded".into(),
        state: SpecState::InProgress,
        state_at: OffsetDateTime::now_utc(),
        kind: SpecKind::Fix,
        ..SpecFacts::default()
    };
    let snapshot = gauge_snapshot();
    *snapshot.lock().unwrap() = PodSnapshot {
        specs: BTreeMap::from([(product.clone(), vec![spec])]),
        ..PodSnapshot::default()
    };
    let effort = |kind: &str| {
        test_support::gauge_f64_value(
            "pod.spec.effort",
            &[
                ("pod.product", product.as_str()),
                ("spec.id", "kinded"),
                ("measure", "revisions"),
                ("spec.kind", kind),
            ],
        )
    };
    assert_eq!(effort("fix"), Some(0.0));
    assert_eq!(effort("unkinded"), None);

    *snapshot.lock().unwrap() = PodSnapshot::default();
}

#[test]
fn awaiting_is_true_only_for_the_exact_first_line() {
    assert!(spec_awaiting("Waiting for your approval.\n\nText"));
    assert!(spec_awaiting("  Waiting for your approval.  \nText"));
    assert!(!spec_awaiting("# T\nWaiting for your approval.\n"));
    assert!(!spec_awaiting("Waiting for your approval\n"));
    assert!(!spec_awaiting("Starts under the small-fix waiver.\n"));
    assert!(!spec_awaiting(""));
}

#[test]
fn awaiting_ignores_one_leading_byte_order_mark() {
    assert!(spec_awaiting("\u{feff}Waiting for your approval.\n\nText"));
    assert!(spec_awaiting(
        "\u{feff}  Waiting for your approval.  \nText"
    ));
    assert!(!spec_awaiting(
        "\u{feff}\u{feff}Waiting for your approval.\n"
    ));
    assert!(!spec_awaiting(
        "\u{feff}Starts under the small-fix waiver.\n"
    ));
    assert!(!spec_awaiting("Text\u{feff}Waiting for your approval.\n"));
}

#[test]
fn defaults_for_size_and_awaiting() {
    let facts = SpecFacts::default();
    assert_eq!(facts.size, SpecSize::Unsized);
    assert!(!facts.awaiting);
}

// --- confirmed files and pod.fix.confirmed_at ---

fn put_confirmed(dir: &Path, id: &str, text: &str) {
    write(&dir.join(format!("confirmed/{id}.md")), text);
}

const CONFIRMED_AT: &str = "2026-11-05T09:30:00Z";

#[tokio::test]
async fn a_confirmed_file_gives_the_spec_its_confirmed_time() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    let files = pod_files(&repo);
    write(&files.join("specs/fixed.md"), "# Fixed\n\n**Kind.** fix\n");
    put_confirmed(
        &files,
        "fixed",
        &format!(
            "# Confirmed: fixed\n\nconfirmed: {CONFIRMED_AT}\nevidence: queries/example.sql\n"
        ),
    );
    write(&files.join("specs/plain.md"), "# Plain\n");
    let (mut stats, snapshot) = stats(one_product(&pod, &repo));
    stats.poll().await;
    let specs = snapshot.lock().unwrap().specs[&pod.name].clone();
    let at = |id: &str| {
        specs
            .iter()
            .find(|spec| spec.id == id)
            .unwrap()
            .confirmed_at
    };
    assert_eq!(
        at("fixed").map(|time| time.unix_timestamp()),
        Some(parse_time(CONFIRMED_AT).unwrap().unix_timestamp())
    );
    assert_eq!(at("plain"), None);
}

#[tokio::test]
async fn a_restart_reads_the_same_confirmed_time_again() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    let files = pod_files(&repo);
    write(&files.join("specs/fixed.md"), "# Fixed\n");
    put_confirmed(
        &files,
        "fixed",
        &format!("confirmed: {CONFIRMED_AT}\nevidence: q\n"),
    );
    let read = || async {
        let (mut stats, snapshot) = stats(one_product(&pod, &repo));
        stats.poll().await;
        let specs = snapshot.lock().unwrap().specs[&pod.name].clone();
        specs[0].confirmed_at
    };
    let first = read().await;
    let second = read().await;
    assert!(first.is_some());
    assert_eq!(first, second);
}

#[tokio::test]
async fn a_confirmed_file_with_no_time_is_ignored() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    let files = pod_files(&repo);
    write(&files.join("specs/a.md"), "# A\n");
    write(&files.join("specs/b.md"), "# B\n");
    put_confirmed(&files, "a", "evidence: queries/example.sql\n");
    put_confirmed(&files, "b", "confirmed: yesterday\nevidence: q\n");
    let (mut stats, snapshot) = stats(one_product(&pod, &repo));
    stats.poll().await;
    let specs = snapshot.lock().unwrap().specs[&pod.name].clone();
    assert_eq!(specs.len(), 2);
    assert!(specs.iter().all(|spec| spec.confirmed_at.is_none()));
}

#[tokio::test]
async fn the_confirmed_gauge_carries_unix_seconds_with_the_spec_and_product() {
    let _gauge_snapshot = GAUGE_SNAPSHOT_TESTS.lock().await;
    let product = format!("pod-test-{}", uuid::Uuid::new_v4());
    let at = parse_time(CONFIRMED_AT).unwrap();
    let confirmed = SpecFacts {
        id: "fixed".into(),
        confirmed_at: Some(at),
        ..SpecFacts::default()
    };
    let not_confirmed = SpecFacts {
        id: "open".into(),
        ..SpecFacts::default()
    };
    let snapshot = gauge_snapshot();
    *snapshot.lock().unwrap() = PodSnapshot {
        specs: BTreeMap::from([(product.clone(), vec![confirmed, not_confirmed])]),
        ..PodSnapshot::default()
    };
    let read = |id: &str| {
        test_support::gauge_f64_value(
            "pod.fix.confirmed_at",
            &[("pod.product", product.as_str()), ("spec.id", id)],
        )
    };
    assert_eq!(read("fixed"), Some(at.unix_timestamp() as f64));
    assert_eq!(read("open"), None);

    *snapshot.lock().unwrap() = PodSnapshot::default();
}

// --- evidence files and pod.evidence.missing ---

fn put_evidence(dir: &Path, id: &str, state: &str) {
    write(
        &dir.join(format!("evidence/{id}.md")),
        &format!(
            "# Evidence: {id}\n\nchecked: 2026-11-20T09:00:00Z\nsince: 2026-11-12T03:05:00Z\nreleased:\nstate: {state}\nquery: {id}-1 absent no_use_yet\n"
        ),
    );
}

#[tokio::test]
async fn an_evidence_file_gives_the_spec_its_missing_flag() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    let files = pod_files(&repo);
    for (id, state) in [("a", "missing"), ("b", "waiting"), ("c", "present")] {
        put_spec(&files, id);
        put_evidence(&files, id, state);
    }
    put_spec(&files, "none");
    let (mut stats, snapshot) = stats(one_product(&pod, &repo));
    stats.poll().await;
    let specs = by_id(snapshot.lock().unwrap().specs[&pod.name].clone());
    assert_eq!(specs["a"].evidence_missing, Some(true));
    assert_eq!(specs["b"].evidence_missing, Some(false));
    assert_eq!(specs["c"].evidence_missing, Some(false));
    assert_eq!(specs["none"].evidence_missing, None);
}

#[tokio::test]
async fn a_bad_evidence_file_gives_no_flag_and_one_notice() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    let files = pod_files(&repo);
    put_spec(&files, "s");
    write(&files.join("evidence/s.md"), "state: maybe\n");
    let (mut stats, snapshot) = stats(one_product(&pod, &repo));
    let logs = CapturedLogs::start();
    stats.poll().await;
    stats.poll().await;
    let line = format!(
        "pod file unreadable pod.product={} spec.id=s file=evidence/s.md",
        pod.name
    );
    let text = logs.text();
    assert_eq!(text.matches(&line).count(), 1, "{text}");
    assert_eq!(
        snapshot.lock().unwrap().specs[&pod.name][0].evidence_missing,
        None
    );
}

#[tokio::test]
async fn the_evidence_gauge_is_1_for_missing_0_for_the_others_and_no_point_without_a_file() {
    let _gauge_snapshot = GAUGE_SNAPSHOT_TESTS.lock().await;
    let product = format!("pod-test-{}", uuid::Uuid::new_v4());
    let spec = |id: &str, flag: Option<bool>| SpecFacts {
        id: id.into(),
        evidence_missing: flag,
        ..SpecFacts::default()
    };
    let snapshot = gauge_snapshot();
    *snapshot.lock().unwrap() = PodSnapshot {
        specs: BTreeMap::from([(
            product.clone(),
            vec![
                spec("gone", Some(true)),
                spec("fine", Some(false)),
                spec("none", None),
            ],
        )]),
        ..PodSnapshot::default()
    };
    let read = |id: &str| {
        test_support::gauge_value(
            "pod.evidence.missing",
            &[("pod.product", product.as_str()), ("spec.id", id)],
        )
    };
    assert_eq!(read("gone"), Some(1));
    assert_eq!(read("fine"), Some(0));
    assert_eq!(read("none"), None);

    *snapshot.lock().unwrap() = PodSnapshot::default();
}

// --- free slots with work waiting, and work waiting ---

/// Writes a spec with an approval, and no other file, in the pod folder of `repo`.
fn approve(repo: &Path, id: &str) {
    put_spec(&pod_files(repo), id);
    put_approval(&pod_files(repo), id);
}

#[tokio::test]
async fn free_slots_with_an_approved_spec_count_as_idle_with_work() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    approve(&repo, "spec-a");
    let (mut stats, snapshot) = stats(one_product(&pod, &repo));
    stats.poll().await;
    let snapshot = snapshot.lock().unwrap();
    assert_eq!(snapshot.idle_with_work.get(&pod.name), Some(&3));
    assert_eq!(
        snapshot.inventory[&pod.name]["approved_not_started"].items,
        1
    );
}

#[tokio::test]
async fn free_slots_with_no_approved_spec_count_0() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    put_spec(&pod_files(&repo), "spec-a");
    let (mut stats, snapshot) = stats(one_product(&pod, &repo));
    stats.poll().await;
    assert_eq!(
        snapshot.lock().unwrap().idle_with_work.get(&pod.name),
        Some(&0)
    );
}

#[tokio::test]
async fn an_approval_before_the_first_merged_file_does_not_count() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    approve(&repo, "spec-a");
    tokio::time::sleep(Duration::from_millis(100)).await;
    write(&pod_files(&repo).join("merged/old.md"), "merged\n");
    let (mut stats, snapshot) = stats(one_product(&pod, &repo));
    stats.poll().await;
    let snapshot = snapshot.lock().unwrap();
    assert_eq!(snapshot.idle_with_work.get(&pod.name), Some(&0));
    assert_eq!(
        snapshot.inventory[&pod.name]["approved_not_started"].items,
        0
    );
}

#[tokio::test]
async fn a_product_with_no_merged_folder_counts_every_approval() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    approve(&repo, "spec-a");
    approve(&repo, "spec-b");
    let (mut stats, snapshot) = stats(one_product(&pod, &repo));
    stats.poll().await;
    assert_eq!(
        snapshot.lock().unwrap().inventory[&pod.name]["approved_not_started"].items,
        2
    );
}

#[test]
fn list_stats_of_no_ages_is_all_0() {
    assert_eq!(list_stats(&[]), ListStats::default());
}

#[test]
fn list_stats_gives_the_count_the_oldest_hours_and_the_item_days() {
    let stats = list_stats(&[time::Duration::days(2), time::Duration::days(4)]);
    assert_eq!(
        stats,
        ListStats {
            items: 2,
            oldest_hours: 96.0,
            item_days: 6.0
        }
    );
}

fn spec_in(id: &str, state: SpecState, state_at: OffsetDateTime) -> SpecFacts {
    SpecFacts {
        id: id.into(),
        state,
        state_at,
        ..SpecFacts::default()
    }
}

#[test]
fn the_three_lists_come_from_fake_pod_folders_with_item_days_and_oldest_age() {
    let now = at("2026-11-03T00:00:00Z");
    let waiting = |id: &str, days: i64| SpecFacts {
        open_at: Some(now - time::Duration::days(days)),
        awaiting: true,
        ..spec_in(id, SpecState::Open, now)
    };
    let approved = |id: &str, days: i64| SpecFacts {
        approved_at: Some(now - time::Duration::days(days)),
        ..spec_in(id, SpecState::Approved, now)
    };
    let specs = vec![
        waiting("w1", 2),
        waiting("w2", 4),
        // An open spec that does not wait for approval is in no list.
        SpecFacts {
            open_at: Some(now - time::Duration::days(9)),
            ..spec_in("draft", SpecState::Open, now)
        },
        approved("a1", 3),
        // Approved before the first merged file.
        approved("a0", 30),
        spec_in("p1", SpecState::InProgress, now - time::Duration::days(1)),
        spec_in("done", SpecState::Completed, now - time::Duration::days(1)),
    ];
    let cutoff = Some(now - time::Duration::days(10));
    let (idle, lists) = work_waiting(&specs, 2, cutoff, now);
    assert_eq!(idle, 2);
    assert_eq!(
        lists["awaiting_approval"],
        ListStats {
            items: 2,
            oldest_hours: 96.0,
            item_days: 6.0
        }
    );
    assert_eq!(
        lists["approved_not_started"],
        ListStats {
            items: 1,
            oldest_hours: 72.0,
            item_days: 3.0
        }
    );
    assert_eq!(
        lists["in_progress"],
        ListStats {
            items: 1,
            oldest_hours: 24.0,
            item_days: 1.0
        }
    );
}

#[test]
fn an_empty_list_reports_0() {
    let now = at("2026-11-03T00:00:00Z");
    let (idle, lists) = work_waiting(&[], 3, None, now);
    assert_eq!(idle, 0);
    assert_eq!(lists.len(), 6);
    for name in WORK_LISTS {
        assert_eq!(lists[name], ListStats::default(), "{name}");
    }
}

#[tokio::test]
async fn the_work_waiting_gauges_report_every_list_and_a_product_that_was_not_read_has_none() {
    let _gauge_snapshot = GAUGE_SNAPSHOT_TESTS.lock().await;
    let read = format!("pod-test-{}", uuid::Uuid::new_v4());
    let unread = format!("pod-test-{}", uuid::Uuid::new_v4());
    let now = at("2026-11-03T00:00:00Z");
    let (idle, lists) = work_waiting(&[], 3, None, now);
    let mut lists = lists;
    lists.insert(
        "awaiting_approval",
        ListStats {
            items: 2,
            oldest_hours: 96.0,
            item_days: 6.0,
        },
    );
    let snapshot = gauge_snapshot();
    *snapshot.lock().unwrap() = PodSnapshot {
        idle_with_work: BTreeMap::from([(read.clone(), idle)]),
        inventory: BTreeMap::from([(read.clone(), lists)]),
        slots: BTreeMap::from([(unread.clone(), SlotCounts { busy: 0, free: 3 })]),
        ..PodSnapshot::default()
    };
    assert_eq!(
        test_support::gauge_value(
            "pod.slots.idle_with_work",
            &[("pod.product", read.as_str())]
        ),
        Some(0)
    );
    let attrs = |list: &'static str| [("pod.product", read.as_str()), ("list", list)];
    assert_eq!(
        test_support::gauge_value("pod.inventory.items", &attrs("awaiting_approval")),
        Some(2)
    );
    assert_eq!(
        test_support::gauge_f64_value("pod.inventory.oldest_hours", &attrs("awaiting_approval")),
        Some(96.0)
    );
    assert_eq!(
        test_support::gauge_f64_value("pod.inventory.item_days", &attrs("awaiting_approval")),
        Some(6.0)
    );
    for list in ["approved_not_started", "in_progress"] {
        assert_eq!(
            test_support::gauge_value("pod.inventory.items", &attrs(list)),
            Some(0),
            "{list}"
        );
    }
    assert_eq!(
        test_support::gauge_value(
            "pod.slots.idle_with_work",
            &[("pod.product", unread.as_str())]
        ),
        None
    );
    assert_eq!(
        test_support::gauge_value("pod.inventory.items", &[("pod.product", unread.as_str())]),
        None
    );
    *snapshot.lock().unwrap() = PodSnapshot::default();
}

#[test]
fn the_inventory_constants_name_the_signals() {
    assert_eq!(INVENTORY_ITEMS, ("pod.inventory.items", "{spec}"));
    assert_eq!(INVENTORY_OLDEST_HOURS, ("pod.inventory.oldest_hours", "h"));
    assert_eq!(INVENTORY_ITEM_DAYS, ("pod.inventory.item_days", "d"));
}

#[test]
fn the_first_merged_file_is_the_earliest_md_file() {
    let pod = Pod::new();
    let dir = pod.repo("main");
    assert_eq!(first_merged_file(&dir, UtcOffset::UTC), None);
    write(&dir.join("merged/a.md"), "x\n");
    write(&dir.join("merged/b.txt"), "x\n");
    let found = first_merged_file(&dir, UtcOffset::UTC).unwrap();
    assert_eq!(found, created(&dir.join("merged/a.md")));
}

#[tokio::test]
async fn a_poll_reports_the_three_lists_from_fake_pod_folders() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    let files = pod_files(&repo);
    write(
        &files.join("specs/wait-1.md"),
        "Waiting for your approval.\n# Spec\n",
    );
    write(
        &files.join("specs/wait-2.md"),
        "Waiting for your approval.\n# Spec\n",
    );
    approve(&repo, "ready");
    put_spec(&files, "going");
    put_approval(&files, "going");
    let started = rfc(OffsetDateTime::now_utc() - time::Duration::days(2));
    put_started(&files, "going", &started);
    let (mut stats, snapshot) = stats(one_product(&pod, &repo));
    stats.poll().await;
    let snapshot = snapshot.lock().unwrap();
    let lists = &snapshot.inventory[&pod.name];
    let near = |actual: f64, expected: f64| {
        assert!((actual - expected).abs() < 0.1, "{actual} vs {expected}");
    };
    let waiting = lists["awaiting_approval"];
    assert_eq!(waiting.items, 2);
    near(waiting.oldest_hours, 0.0);
    near(waiting.item_days, 0.0);
    let ready = lists["approved_not_started"];
    assert_eq!(ready.items, 1);
    near(ready.oldest_hours, 0.0);
    near(ready.item_days, 0.0);
    let going = lists["in_progress"];
    assert_eq!(going.items, 1);
    near(going.oldest_hours, 48.0);
    near(going.item_days, 2.0);
}

#[test]
fn an_approval_at_the_cutoff_counts() {
    let now = at("2026-11-03T00:00:00Z");
    let cutoff = now - time::Duration::days(1);
    let specs = [SpecFacts {
        approved_at: Some(cutoff),
        ..spec_in("edge", SpecState::Approved, now)
    }];
    let (idle, lists) = work_waiting(&specs, 2, Some(cutoff), now);
    assert_eq!(idle, 2);
    assert_eq!(lists["approved_not_started"].items, 1);
}

// --- pod notices: the warnings kept across restarts ---

/// The notices file the `stats` of `pod`'s registry use: `<data folder>/pod-notices.json`.
fn notices_path(pod: &Pod) -> PathBuf {
    pod.root.join("data/pod-notices.json")
}

/// A product whose spec `s` started 20 days ago and merged without a stage 8 days ago,
/// with no merged file: each poll reads it as completed without its merged file.
async fn completed_without_file(pod: &Pod) -> (PathBuf, PathBuf) {
    let repo = pod.repo("main");
    approved_spec(&repo, "s").await;
    put_started(
        &pod_files(&repo),
        "s",
        &rfc(OffsetDateTime::now_utc() - time::Duration::days(20)),
    );
    merge_branch_days_ago(&repo, "s", "x\n", 8);
    let registry = one_product(pod, &repo);
    (registry, repo)
}

fn completed_line(pod: &Pod) -> String {
    format!(
        "spec completed without merged file pod.product={} spec.id=s",
        pod.name
    )
}

/// The keys in the notices file, which must be a JSON object.
fn notice_keys(pod: &Pod) -> Vec<String> {
    let text = std::fs::read_to_string(notices_path(pod)).expect("the notices file");
    let saved: BTreeMap<String, String> =
        serde_json::from_str(&text).expect("the notices file is a JSON object");
    saved.into_keys().collect()
}

fn completed_key(pod: &Pod) -> String {
    format!("{}\0completed\0s", pod.name)
}

#[tokio::test]
async fn a_restart_does_not_warn_again() {
    use std::os::unix::fs::PermissionsExt;

    let pod = Pod::new();
    let (registry, _repo) = completed_without_file(&pod).await;
    let logs = CapturedLogs::start();

    let (mut first, _) = stats(registry.clone());
    first.poll().await;
    first.poll().await;
    assert_eq!(logs.text().matches(&completed_line(&pod)).count(), 1);
    drop(first);
    let restarted = CapturedLogs::start();
    let (mut second, _) = stats(registry);
    second.poll().await;
    second.poll().await;

    let text = restarted.text();
    assert_eq!(text.matches(&completed_line(&pod)).count(), 0, "{text}");
    assert!(!text.contains("pod notices unreadable"), "{text}");
    let mode = std::fs::metadata(notices_path(&pod))
        .expect("the notices file")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    assert_eq!(notice_keys(&pod), vec![completed_key(&pod)]);
}

#[tokio::test]
async fn a_merged_file_clears_the_key_and_a_later_gap_warns_again() {
    let pod = Pod::new();
    let (registry, repo) = completed_without_file(&pod).await;
    let logs = CapturedLogs::start();
    let (mut stats, _) = stats(registry);
    stats.poll().await;

    put_merged(
        &pod_files(&repo),
        "s",
        &rfc(OffsetDateTime::now_utc()),
        COMMIT,
    );
    stats.poll().await;
    assert_eq!(notice_keys(&pod), Vec::<String>::new());
    std::fs::remove_file(pod_files(&repo).join("merged/s.md")).unwrap();
    stats.poll().await;

    let text = logs.text();
    assert_eq!(text.matches(&completed_line(&pod)).count(), 2, "{text}");
    assert_eq!(notice_keys(&pod), vec![completed_key(&pod)]);
}

/// Builds `PodStats` over a notices file made by `spoil`, polls twice and checks one
/// `pod notices unreadable` line with `reason`, the spec warning once more and a valid
/// file afterwards.
async fn check_unreadable_notices(spoil: impl FnOnce(&Path) -> bool, reason: &str) {
    let pod = Pod::new();
    let (registry, _repo) = completed_without_file(&pod).await;
    let file = notices_path(&pod);
    write(&file, "");
    if !spoil(&file) {
        // The file stays readable, as for root: there is nothing to check.
        return;
    }
    let logs = CapturedLogs::start();

    let (mut stats, _) = stats(registry);
    stats.poll().await;
    stats.poll().await;

    let text = logs.text();
    let line = format!("pod notices unreadable reason=\"{reason}\"");
    assert_eq!(text.matches(&line).count(), 1, "{text}");
    assert_eq!(text.matches("pod notices unreadable").count(), 1, "{text}");
    assert_eq!(text.matches(&completed_line(&pod)).count(), 1, "{text}");
    assert_eq!(notice_keys(&pod), vec![completed_key(&pod)]);
}

#[tokio::test]
async fn unreadable_notices_file_is_replaced_and_warns_once() {
    use std::os::unix::fs::PermissionsExt;

    for text in ["not json", "", "[]", "[\"a\"]", "{\"k\": 1}"] {
        check_unreadable_notices(
            |file| {
                std::fs::write(file, text).unwrap();
                true
            },
            "parse",
        )
        .await;
    }
    check_unreadable_notices(
        |file| {
            std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o000)).unwrap();
            std::fs::read(file).is_err()
        },
        "permission",
    )
    .await;
}

#[tokio::test]
async fn an_unavailable_product_keeps_its_keys() {
    use std::os::unix::fs::PermissionsExt;

    let pod = Pod::new();
    let (registry, repo) = completed_without_file(&pod).await;
    let specs = pod_files(&repo).join("specs");
    let logs = CapturedLogs::start();
    let (mut running, _) = stats(registry.clone());
    running.poll().await;

    std::fs::set_permissions(&specs, std::fs::Permissions::from_mode(0o000)).unwrap();
    running.poll().await;
    drop(running);
    // A restart while the product is still unavailable.
    let (mut restarted, _) = stats(registry);
    restarted.poll().await;
    assert_eq!(notice_keys(&pod), vec![completed_key(&pod)]);
    std::fs::set_permissions(&specs, std::fs::Permissions::from_mode(0o755)).unwrap();
    restarted.poll().await;

    let text = logs.text();
    assert_eq!(text.matches(&completed_line(&pod)).count(), 1, "{text}");
    assert_eq!(notice_keys(&pod), vec![completed_key(&pod)]);
}

#[tokio::test]
async fn the_stage_warning_survives_a_restart() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    approved_spec(&repo, "s").await;
    put_started(
        &pod_files(&repo),
        "s",
        &rfc(OffsetDateTime::now_utc() - time::Duration::days(20)),
    );
    merge_branch_days_ago(&repo, "s-1b", "x\n", 8);
    let registry = one_product(&pod, &repo);
    let logs = CapturedLogs::start();

    let (mut first, _) = stats(registry.clone());
    first.poll().await;
    drop(first);
    let (mut second, _) = stats(registry);
    second.poll().await;

    let text = logs.text();
    let line = format!(
        "spec stage merged without merged file pod.product={} spec.id=s spec.stage=1b",
        pod.name
    );
    assert_eq!(text.matches(&line).count(), 1, "{text}");
    assert_eq!(
        notice_keys(&pod),
        vec![format!("{}\0stage\0s\01b", pod.name)]
    );
}

#[tokio::test]
async fn a_read_only_data_folder_still_warns_once_in_memory() {
    use std::os::unix::fs::PermissionsExt;

    let pod = Pod::new();
    let (registry, _repo) = completed_without_file(&pod).await;
    let data = pod.root.join("data");
    std::fs::create_dir_all(data.join("pod")).unwrap();
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o555)).unwrap();
    let logs = CapturedLogs::start();

    let (mut stats, _) = stats(registry);
    stats.poll().await;
    stats.poll().await;
    stats.poll().await;
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o755)).unwrap();

    let text = logs.text();
    assert_eq!(text.matches(&completed_line(&pod)).count(), 1, "{text}");
    // Root can write the folder, and then the save does not fail.
    if !notices_path(&pod).exists() {
        assert_eq!(
            text.matches("pod notices not saved reason=\"permission\"")
                .count(),
            1,
            "{text}"
        );
    }
    assert!(!text.contains("pod notices unreadable"), "{text}");
}

#[test]
fn a_missing_data_folder_fails_the_save_once_and_keeps_the_keys() {
    let pod = Pod::new();
    let file = pod.root.join("missing/pod-notices.json");
    let logs = CapturedLogs::start();

    let mut notices = PodNotices::load(file.clone());
    notices.insert("p\0completed\0s".into(), OffsetDateTime::now_utc());
    notices.save();
    notices.insert("p\0completed\0t".into(), OffsetDateTime::now_utc());
    notices.save();

    assert!(notices.contains("p\0completed\0s"));
    assert!(notices.contains("p\0completed\0t"));
    assert!(!file.exists());
    let text = logs.text();
    assert_eq!(
        text.matches("pod notices not saved reason=\"io\"").count(),
        1,
        "{text}"
    );
    assert_eq!(text.matches("pod notices not saved").count(), 1, "{text}");
    assert!(!text.contains("pod notices unreadable"), "{text}");
}

#[test]
fn retain_product_removes_only_that_products_keys_that_were_not_seen() {
    let pod = Pod::new();
    let mut notices = PodNotices::load(pod.root.join("pod-notices.json"));
    let now = OffsetDateTime::now_utc();
    for key in [
        "p\0completed\0a",
        "p\0completed\0b",
        "pq\0completed\0a",
        "q\0completed\0a",
    ] {
        notices.insert(key.into(), now);
    }

    notices.retain_product("p", &HashSet::from(["p\0completed\0a".to_owned()]));

    assert!(notices.contains("p\0completed\0a"));
    assert!(!notices.contains("p\0completed\0b"));
    assert!(notices.contains("pq\0completed\0a"));
    assert!(notices.contains("q\0completed\0a"));
}

#[tokio::test]
async fn the_completed_warning_names_the_merge_time_and_commit() {
    let pod = Pod::new();
    let (registry, repo) = completed_without_file(&pod).await;
    // The newest entry of the base branch's reflog is the merge: its full commit and its
    // time, as Harold reads them.
    let newest = git(
        &repo,
        &[
            "reflog",
            "show",
            "--date=unix",
            "--format=%H%x09%gd",
            "-1",
            "main",
        ],
    );
    let (commit, selector) = newest.split_once('\t').unwrap();
    let seconds: i64 = selector
        .trim_start_matches("main@{")
        .trim_end_matches('}')
        .parse()
        .unwrap();
    let merged_at = rfc(OffsetDateTime::from_unix_timestamp(seconds).unwrap());
    let logs = CapturedLogs::start();

    let (mut stats, _) = stats(registry);
    stats.poll().await;

    let text = logs.text();
    let line = format!(
        "{} merged_at={merged_at} commit={commit}",
        completed_line(&pod)
    );
    assert_eq!(commit.len(), 40, "{commit}");
    assert_eq!(text.matches(&line).count(), 1, "{text}");
}

#[test]
fn a_merge_seen_only_from_a_gone_worktree_logs_an_unknown_commit() {
    let pod = Pod::new();
    let (mut stats, _) = stats(pod.root.join("products.toml"));
    let merged_at = at("2026-10-20T08:30:00Z");
    let logs = CapturedLogs::start();

    stats.log_notices(
        &pod.name,
        &[Notice::CompletedWithoutFile {
            id: "s".into(),
            merged_at,
            commit: None,
        }],
        &[],
        None,
    );

    let text = logs.text();
    let line = format!(
        "{} merged_at=2026-10-20T08:30:00Z commit=unknown",
        completed_line(&pod)
    );
    assert_eq!(text.matches(&line).count(), 1, "{text}");
}

// ---------------------------------------------------------------------------
// The pane lookup
// ---------------------------------------------------------------------------

/// Polls the pod once into the process-wide snapshot, which `pane_identity` reads.
async fn poll_into_gauge_snapshot(pod: &Pod, repo: &Path, panes: Vec<LivePane>, chief: &str) {
    let agents: AgentSource = Arc::new(move || panes.clone());
    let tips = tips_file(&pod.root.join("data/store"));
    let mut stats = PodStats::new(
        one_product(pod, repo),
        tips,
        test_git(),
        gauge_snapshot(),
        UtcOffset::UTC,
        agents,
        Some(chief.to_owned()),
    );
    stats.poll().await;
}

fn started_file(repo: &Path, id: &str, worktree: &Path) {
    write(
        &pod_files(repo).join(format!("started/{id}.md")),
        &format!(
            "# {id}\nstarted: 2026-11-01T00:00:00Z\nworktree: {}\n",
            worktree.display()
        ),
    );
}

#[tokio::test]
async fn pane_identity_gives_pod_roles_and_product() {
    let _gauge_snapshot = GAUGE_SNAPSHOT_TESTS.lock().await;
    let pod = Pod::new();
    let repo = pod.repo("main");
    write(&pod_files(&repo).join("agent-panes"), "%80\n%81\n");
    let panes = vec![live("%80", "/pods/a"), live("%81", "/pods/b")];

    poll_into_gauge_snapshot(&pod, &repo, panes, "/work/chief").await;

    let coordination = pane_identity("%80");
    assert_eq!(coordination.role, Role::PodCoordination);
    assert_eq!(coordination.product.as_deref(), Some(pod.name.as_str()));
    assert_eq!(coordination.spec_id, None);
    let pnem = pane_identity("%81");
    assert_eq!(pnem.role, Role::Pnem);
    assert_eq!(pnem.product.as_deref(), Some(pod.name.as_str()));
}

#[tokio::test]
async fn pane_identity_gives_a_build_worker_its_spec_id() {
    let _gauge_snapshot = GAUGE_SNAPSHOT_TESTS.lock().await;
    let pod = Pod::new();
    let repo = pod.repo("main");
    let worktree = pod.worktree(&repo, "w", true);
    started_file(&repo, "spec-one", &worktree);
    started_file(&repo, "spec-two", &pod.root.join("elsewhere"));

    poll_into_gauge_snapshot(&pod, &repo, vec![live("%7", "/pods/w")], "/work/chief").await;

    let worker = pane_identity("%7");
    assert_eq!(worker.role, Role::BuildWorker);
    assert_eq!(worker.product.as_deref(), Some(pod.name.as_str()));
    assert_eq!(worker.spec_id.as_deref(), Some("spec-one"));
}

#[tokio::test]
async fn pane_identity_has_no_spec_without_a_started_file() {
    let _gauge_snapshot = GAUGE_SNAPSHOT_TESTS.lock().await;
    let pod = Pod::new();
    let repo = pod.repo("main");
    pod.worktree(&repo, "w", true);

    poll_into_gauge_snapshot(&pod, &repo, vec![live("%7", "/pods/w")], "/work/chief").await;

    let worker = pane_identity("%7");
    assert_eq!(worker.role, Role::BuildWorker);
    assert_eq!(worker.product.as_deref(), Some(pod.name.as_str()));
    assert_eq!(worker.spec_id, None);
}

#[tokio::test]
async fn pane_identity_gives_chief_and_other() {
    let _gauge_snapshot = GAUGE_SNAPSHOT_TESTS.lock().await;
    let pod = Pod::new();
    let repo = pod.repo("main");
    let panes = vec![live("%99", "/work/chief"), live("%55", "/work/elsewhere")];

    poll_into_gauge_snapshot(&pod, &repo, panes, "/work/chief").await;

    let chief = pane_identity("%99");
    assert_eq!(
        (chief.role, chief.product, chief.spec_id),
        (Role::Chief, None, None)
    );
    let other = pane_identity("%55");
    assert_eq!(
        (other.role, other.product, other.spec_id),
        (Role::Other, None, None)
    );
}

#[tokio::test]
async fn pane_identity_of_an_unknown_pane_is_other() {
    let _gauge_snapshot = GAUGE_SNAPSHOT_TESTS.lock().await;
    let pod = Pod::new();
    let repo = pod.repo("main");

    poll_into_gauge_snapshot(&pod, &repo, Vec::new(), "/work/chief").await;

    let unknown = pane_identity("%404");
    assert_eq!(
        (unknown.role, unknown.product, unknown.spec_id),
        (Role::Other, None, None)
    );
}

#[tokio::test]
async fn pane_identity_finds_the_spec_when_the_started_file_holds_the_folder_name() {
    let _gauge_snapshot = GAUGE_SNAPSHOT_TESTS.lock().await;
    let pod = Pod::new();
    let repo = pod.repo("main");
    pod.worktree(&repo, "adr-0027-host-scan", true);
    write(
        &pod_files(&repo).join("started/adr-0027-host-scan.md"),
        "# Started: adr-0027-host-scan\n\nstarted: 2026-11-02T23:02:42+11:00\nworktree: adr-0027-host-scan\n",
    );

    poll_into_gauge_snapshot(&pod, &repo, vec![live("%7", "/pods/w")], "/work/chief").await;

    let worker = pane_identity("%7");
    assert_eq!(worker.role, Role::BuildWorker);
    assert_eq!(worker.spec_id.as_deref(), Some("adr-0027-host-scan"));
}

fn sha_of(text: &str) -> String {
    hex::encode(<sha2::Sha256 as sha2::Digest>::digest(text.as_bytes()))
}

#[test]
fn approval_current_is_true_for_a_matching_checksum() {
    let approval = format!("approved: 2026-10-14\nspec-sha256: {}\n", sha_of("text"));
    assert!(approval_is_current(Some(&approval), Some("text")));
}

#[test]
fn approval_current_is_false_for_another_checksum() {
    let approval = format!("spec-sha256: {}\n", sha_of("older text"));
    assert!(!approval_is_current(Some(&approval), Some("text")));
}

#[test]
fn approval_current_is_true_without_a_checksum_line() {
    assert!(approval_is_current(
        Some("approved: 2026-10-14\n"),
        Some("text")
    ));
}

#[test]
fn approval_current_ignores_case_and_spaces_of_the_checksum() {
    let approval = format!("  spec-sha256:   {}  \n", sha_of("text").to_uppercase());
    assert!(approval_is_current(Some(&approval), Some("text")));
}

#[test]
fn read_specs_marks_an_approval_that_names_an_older_text() {
    let pod = Pod::new();
    let dir = pod.root.join("pod");
    for id in ["fresh", "stale"] {
        write(&dir.join(format!("specs/{id}.md")), "# Spec\nnew\n");
    }
    let approval = |text: &str| format!("approved: 2026-10-14\nspec-sha256: {}\n", sha_of(text));
    write(&dir.join("approvals/fresh.md"), &approval("# Spec\nnew\n"));
    write(&dir.join("approvals/stale.md"), &approval("# Spec\nold\n"));

    let specs = by_id(read_files(&dir).specs);

    assert!(specs["fresh"].approval_current);
    assert!(!specs["stale"].approval_current);
}

#[test]
fn work_waiting_counts_a_stale_approval_as_awaiting_approval() {
    let now = at("2026-11-03T00:00:00Z");
    let stale = SpecFacts {
        approved_at: Some(now - time::Duration::days(2)),
        approval_current: false,
        ..spec_in("stale", SpecState::Approved, now)
    };

    let (idle, lists) = work_waiting(&[stale], 2, None, now);

    assert_eq!(idle, 0);
    assert_eq!(lists["awaiting_approval"].items, 1);
    assert_eq!(lists["approved_not_started"].items, 0);
}

#[test]
fn work_waiting_still_counts_a_current_approval_as_approved() {
    let now = at("2026-11-03T00:00:00Z");
    let current = SpecFacts {
        approved_at: Some(now - time::Duration::days(2)),
        ..spec_in("current", SpecState::Approved, now)
    };

    let (idle, lists) = work_waiting(&[current], 2, None, now);

    assert_eq!(idle, 2);
    assert_eq!(lists["awaiting_approval"].items, 0);
    assert_eq!(lists["approved_not_started"].items, 1);
}

// --- draft_time ---

#[test]
fn draft_time_takes_the_time_of_a_first_heading_that_has_one() {
    let text = "# Revisions\n\n## 2026-11-02 09:30, rev 1 (first)\n\n## 2026-11-03 10:00, rev 2\n";
    let birth = at("2026-11-05T12:00:00Z");
    assert_eq!(
        draft_time(Some(text), Some(birth), UtcOffset::UTC),
        Some(at("2026-11-02T09:30:00Z"))
    );
}

#[test]
fn draft_time_takes_the_birth_time_when_the_first_heading_has_only_a_date() {
    let text = "## 2026-11-02, rev 1 (first)\n\n## 2026-11-03 10:00, rev 2\n";
    let birth = at("2026-11-05T12:00:00Z");
    assert_eq!(
        draft_time(Some(text), Some(birth), UtcOffset::UTC),
        Some(birth)
    );
}

#[test]
fn draft_time_takes_the_birth_time_when_there_is_no_revisions_file() {
    let birth = at("2026-11-05T12:00:00Z");
    assert_eq!(draft_time(None, Some(birth), UtcOffset::UTC), Some(birth));
    assert_eq!(draft_time(None, None, UtcOffset::UTC), None);
}

#[test]
fn draft_time_takes_an_earlier_birth_time() {
    let text = "## 2026-11-02 09:30, rev 1 (first)\n";
    let birth = at("2026-11-01T08:00:00Z");
    assert_eq!(
        draft_time(Some(text), Some(birth), UtcOffset::UTC),
        Some(birth)
    );
}

// --- the reviews file and the three phases ---

const TWO_ROUNDS: &str = "# Reviews: g\n\n\
sent: 2026-11-08T10:10:00Z sha256:bd85a4c5b901\n\
verdict: 2026-11-08T10:30:00Z revise\n\
sent: 2026-11-08T11:00:00Z sha256:5d43735fbe17\n\
verdict: 2026-11-08T11:20:00Z ok\n";

fn reviewed(text: &str) -> SpecFacts {
    SpecFacts {
        reviews: parse_reviews(text),
        open_at: Some(at("2026-11-08T10:00:00Z")),
        approved_at: Some(at("2026-11-08T12:00:00Z")),
        ..spec("g", SpecState::Completed, "2026-11-08T13:00:00Z")
    }
}

fn phase_sum(product: &str, phase: &str) -> Option<f64> {
    test_support::histogram_sum(
        "pod.spec.lead_time",
        &[("pod.product", product), ("phase", phase)],
    )
}

fn record_reviewed(text: &str) -> String {
    test_support::install();
    let product = format!("pod-test-{}", uuid::Uuid::new_v4());
    let spec_ref = SpecRef {
        id: "g".into(),
        stage: None,
    };
    record_lead_time(
        &product,
        &spec_ref,
        &reviewed(text),
        at("2026-11-08T13:00:00Z"),
        true,
    );
    product
}

#[test]
fn two_review_rounds_give_three_phases_that_add_up_to_open_to_approved() {
    let product = record_reviewed(TWO_ROUNDS);
    // 10 min + 30 min of writing, 20 + 20 min of review, 40 min of approval.
    assert_eq!(phase_sum(&product, "spec_writing"), Some(2400.0));
    assert_eq!(phase_sum(&product, "architect_review"), Some(2400.0));
    assert_eq!(phase_sum(&product, "approval_wait"), Some(2400.0));
    assert_eq!(phase_sum(&product, "open_to_approved"), Some(7200.0));
}

#[test]
fn a_skipped_spec_gives_architect_review_0_and_the_phases_still_add_up() {
    let product = record_reviewed("# Reviews: g\n\nskipped: 2026-11-08T10:30:00Z\n");
    assert_eq!(phase_sum(&product, "spec_writing"), Some(1800.0));
    assert_eq!(phase_sum(&product, "architect_review"), Some(0.0));
    assert_eq!(phase_sum(&product, "approval_wait"), Some(5400.0));
    assert_eq!(phase_sum(&product, "open_to_approved"), Some(7200.0));
}

#[test]
fn no_reviews_file_gives_no_new_phase() {
    test_support::install();
    let product = format!("pod-test-{}", uuid::Uuid::new_v4());
    let facts = SpecFacts {
        reviews: None,
        ..reviewed(TWO_ROUNDS)
    };
    let spec_ref = SpecRef {
        id: "g".into(),
        stage: None,
    };
    record_lead_time(
        &product,
        &spec_ref,
        &facts,
        at("2026-11-08T13:00:00Z"),
        true,
    );
    for phase in ["spec_writing", "architect_review", "approval_wait"] {
        assert_eq!(lead_time_series(&product, phase), Vec::new(), "{phase}");
    }
    assert_eq!(phase_sum(&product, "open_to_approved"), Some(7200.0));
}

#[test]
fn a_draft_time_after_the_first_line_gives_no_review_phase() {
    // The first line is before the open time: a phase would be negative.
    let product = record_reviewed("# Reviews: g\n\nskipped: 2026-11-08T09:00:00Z\n");
    for phase in ["spec_writing", "architect_review", "approval_wait"] {
        assert_eq!(lead_time_series(&product, phase), Vec::new(), "{phase}");
    }
}

#[test]
fn a_reviews_file_with_a_line_out_of_order_does_not_parse() {
    for text in [
        "# Reviews: g\n\nverdict: 2026-11-08T10:30:00Z ok\n",
        "sent: 2026-11-08T10:10:00Z sha256:aa\nsent: 2026-11-08T10:20:00Z sha256:bb\n",
        "sent: not-a-time sha256:aa\n",
        "verdict: 2026-11-08T10:30:00Z maybe\n",
        "# Reviews: g\n",
        "other: 2026-11-08T10:30:00Z\n",
        // The file of propose-hints: a verdict after a closed review.
        "sent: 2026-11-08T10:10:00Z sha256:aa\nverdict: 2026-11-08T10:20:00Z revise\nverdict: 2026-11-08T10:30:00Z ok\n",
    ] {
        assert_eq!(parse_reviews(text), None, "{text}");
    }
}

#[test]
fn a_skipped_line_after_a_sent_line_closes_the_review() {
    assert_eq!(
        parse_reviews("sent: 2026-11-08T10:00:00Z sha256:aa\nskipped: 2026-11-08T10:20:00Z\n"),
        Some(vec![
            ReviewLine::Sent(at("2026-11-08T10:00:00Z")),
            ReviewLine::Skipped(at("2026-11-08T10:20:00Z")),
        ])
    );
}

#[test]
fn the_agent_jobs_file_with_two_rounds_then_sent_and_skipped_parses() {
    let text = format!(
        "{TWO_ROUNDS}sent: 2026-11-08T11:30:00Z sha256:2fcaa698327b\nskipped: 2026-11-08T11:30:00Z\n"
    );
    assert_eq!(parse_reviews(&text).map(|lines| lines.len()), Some(6));
}

#[test]
fn a_skipped_line_with_no_open_review_still_parses() {
    assert_eq!(
        parse_reviews("skipped: 2026-11-08T10:30:00Z\n"),
        Some(vec![ReviewLine::Skipped(at("2026-11-08T10:30:00Z"))])
    );
}

#[test]
fn the_time_from_sent_to_skipped_is_architect_review() {
    let product =
        record_reviewed("sent: 2026-11-08T10:00:00Z sha256:aa\nskipped: 2026-11-08T10:20:00Z\n");
    // Open 10:00, approved 12:00: 0 s writing, 20 min review, 100 min approval.
    assert_eq!(phase_sum(&product, "spec_writing"), Some(0.0));
    assert_eq!(phase_sum(&product, "architect_review"), Some(1200.0));
    assert_eq!(phase_sum(&product, "approval_wait"), Some(6000.0));
    assert_eq!(phase_sum(&product, "open_to_approved"), Some(7200.0));
}

#[test]
fn sent_then_skipped_gives_architect_review_and_approval_wait_of_the_spec() {
    let lines =
        parse_reviews("sent: 2026-11-08T10:00:00Z sha256:aa\nskipped: 2026-11-08T10:20:00Z\n")
            .unwrap();
    let phases = review_phases(
        at("2026-11-08T09:00:00Z"),
        at("2026-11-08T10:30:00Z"),
        &lines,
    );
    assert_eq!(
        phases,
        Some([
            ("spec_writing", 3600.0),
            ("architect_review", 1200.0),
            ("approval_wait", 600.0),
        ])
    );
}

#[test]
fn a_reviews_file_ignores_a_blank_line_and_reads_any_offset() {
    let lines = parse_reviews(
        "# Reviews: g\n\nsent: 2026-11-08T21:10:00+11:00 sha256:aa\n\nverdict: 2026-11-08T21:30:00+11:00 ok\n\n",
    )
    .unwrap();
    assert_eq!(
        lines,
        [
            ReviewLine::Sent(at("2026-11-08T10:10:00Z")),
            ReviewLine::Verdict(at("2026-11-08T10:30:00Z"), true),
        ]
    );
}

#[test]
fn read_specs_reads_the_reviews_file_and_logs_a_bad_one_once() {
    let pod = Pod::new();
    let dir = pod.root.join("pod");
    for id in ["ok", "bad", "none"] {
        write(&dir.join(format!("specs/{id}.md")), "# Spec\n");
    }
    write(&dir.join("reviews/ok.md"), TWO_ROUNDS);
    write(
        &dir.join("reviews/bad.md"),
        "verdict: 2026-11-08T10:30:00Z ok\n",
    );

    let read = read_files(&dir);
    let specs = by_id(read.specs);

    assert_eq!(specs["ok"].reviews.as_ref().map(Vec::len), Some(4));
    assert_eq!(specs["bad"].reviews, None);
    assert_eq!(specs["none"].reviews, None);
    assert_eq!(read.notices.len(), 1, "{:?}", read.notices);
    assert!(matches!(
        &read.notices[0],
        Notice::Unreadable { id, file, .. } if id == "bad" && file == "reviews/bad.md"
    ));
}

// --- the two review lists ---

fn awaiting_with(id: &str, reviews: Option<&str>, stale: bool, now: OffsetDateTime) -> SpecFacts {
    let base = if stale {
        SpecFacts {
            approved_at: Some(now - time::Duration::days(2)),
            approval_current: false,
            ..spec_in(id, SpecState::Approved, now)
        }
    } else {
        SpecFacts {
            awaiting: true,
            open_at: Some(now - time::Duration::days(1)),
            ..spec_in(id, SpecState::Open, now)
        }
    };
    SpecFacts {
        reviews: reviews.and_then(parse_reviews),
        ..base
    }
}

const SENT: &str = "sent: 2026-11-08T10:10:00Z sha256:aa\n";
const REVISE: &str = "verdict: 2026-11-08T10:30:00Z revise\n";
const OK: &str = "verdict: 2026-11-08T10:30:00Z ok\n";
const SKIPPED: &str = "skipped: 2026-11-08T10:30:00Z\n";

#[test]
fn the_last_review_line_puts_a_waiting_spec_in_a_stage() {
    let now = at("2026-11-09T00:00:00Z");
    for stale in [false, true] {
        let cases: [(&str, Option<String>, [u64; 3]); 6] = [
            ("none", None, [1, 0, 0]),
            ("ok", Some(format!("{SENT}{OK}")), [1, 0, 0]),
            ("skipped", Some(SKIPPED.to_owned()), [1, 0, 0]),
            ("revise", Some(format!("{SENT}{REVISE}")), [1, 1, 0]),
            ("sent", Some(SENT.to_owned()), [1, 0, 1]),
            ("sent_skipped", Some(format!("{SENT}{SKIPPED}")), [1, 0, 0]),
        ];
        for (name, reviews, [awaiting, writing, review]) in cases {
            let spec = awaiting_with(name, reviews.as_deref(), stale, now);
            let (_, lists) = work_waiting(&[spec], 2, None, now);
            assert_eq!(lists["awaiting_approval"].items, awaiting, "{name} {stale}");
            assert_eq!(lists["in_spec_writing"].items, writing, "{name} {stale}");
            assert_eq!(lists["in_architect_review"].items, review, "{name} {stale}");
        }
    }
}

#[test]
fn a_spec_that_does_not_wait_for_approval_is_in_no_review_list() {
    let now = at("2026-11-09T00:00:00Z");
    let reviews = parse_reviews(&format!("{SENT}{REVISE}"));
    let approved = SpecFacts {
        approved_at: Some(now),
        reviews: reviews.clone(),
        ..spec_in("approved", SpecState::Approved, now)
    };
    let not_waiting = SpecFacts {
        reviews,
        ..spec_in("quiet", SpecState::Open, now)
    };
    let (_, lists) = work_waiting(&[approved, not_waiting], 2, None, now);
    assert_eq!(lists["in_spec_writing"].items, 0);
    assert_eq!(lists["in_architect_review"].items, 0);
}

#[test]
fn the_review_lists_never_count_a_spec_twice() {
    let now = at("2026-11-09T00:00:00Z");
    let specs = [
        awaiting_with("a", Some(&format!("{SENT}{REVISE}")), false, now),
        awaiting_with("b", Some(SENT), true, now),
        awaiting_with("c", None, false, now),
    ];
    let (_, lists) = work_waiting(&specs, 2, None, now);
    let (awaiting, writing, review) = (
        lists["awaiting_approval"].items,
        lists["in_spec_writing"].items,
        lists["in_architect_review"].items,
    );
    assert_eq!((awaiting, writing, review), (3, 1, 1));
}

// --- the ready file ---

const SHA: &str = "ba90fbf0b35bc839dcb2cf0a9947e6384f02b1c5";

fn built(ready: Option<&str>) -> SpecFacts {
    SpecFacts {
        started_at: Some(at("2026-11-08T12:00:00Z")),
        ready_at: ready.map(at),
        ..reviewed(TWO_ROUNDS)
    }
}

fn record_built(facts: &SpecFacts, first: bool) -> String {
    test_support::install();
    let product = format!("pod-test-{}", uuid::Uuid::new_v4());
    let spec_ref = SpecRef {
        id: "g".into(),
        stage: None,
    };
    record_lead_time(
        &product,
        &spec_ref,
        facts,
        at("2026-11-08T13:00:00Z"),
        first,
    );
    product
}

#[test]
fn a_ready_file_splits_the_build_at_the_first_merge() {
    let product = record_built(&built(Some("2026-11-08T12:40:00Z")), true);
    assert_eq!(phase_sum(&product, "started_to_ready"), Some(2400.0));
    assert_eq!(phase_sum(&product, "ready_to_completed"), Some(1200.0));
    assert_eq!(phase_sum(&product, "started_to_completed"), Some(3600.0));
}

#[test]
fn a_later_merge_records_started_to_completed_only() {
    let product = record_built(&built(Some("2026-11-08T12:40:00Z")), false);
    assert_eq!(phase_sum(&product, "started_to_ready"), None);
    assert_eq!(phase_sum(&product, "ready_to_completed"), None);
    assert_eq!(phase_sum(&product, "started_to_completed"), Some(3600.0));
}

#[test]
fn no_ready_file_gives_neither_phase() {
    let product = record_built(&built(None), true);
    assert_eq!(phase_sum(&product, "started_to_ready"), None);
    assert_eq!(phase_sum(&product, "ready_to_completed"), None);
    assert_eq!(phase_sum(&product, "started_to_completed"), Some(3600.0));
}

#[test]
fn a_ready_time_after_the_merge_gives_neither_ready_phase() {
    let product = record_built(&built(Some("2026-11-08T14:00:00Z")), true);
    assert_eq!(phase_sum(&product, "started_to_ready"), None);
    assert_eq!(phase_sum(&product, "ready_to_completed"), None);
}

#[test]
fn a_ready_time_before_the_start_gives_neither_ready_phase() {
    let product = record_built(&built(Some("2026-11-08T11:00:00Z")), true);
    assert_eq!(phase_sum(&product, "started_to_ready"), None);
    assert_eq!(phase_sum(&product, "ready_to_completed"), None);
    assert_eq!(phase_sum(&product, "started_to_completed"), Some(3600.0));
}

#[test]
fn a_ready_file_needs_a_time_and_a_commit() {
    let ok = format!("# Ready: g\n\nready: 2026-11-08T22:10:00+11:00\ncommit: {SHA}\n");
    let pairs = key_values(&ok);
    assert_eq!(parse_ready(&pairs), Some(at("2026-11-08T11:10:00Z")));
    for text in [
        "# Ready: g\n\nready: 2026-11-08T22:10:00+11:00\n".to_owned(),
        format!("ready: not-a-time\ncommit: {SHA}\n"),
        "ready: 2026-11-08T22:10:00+11:00\ncommit: abc\n".to_owned(),
    ] {
        assert_eq!(parse_ready(&key_values(&text)), None, "{text}");
    }
}

#[test]
fn read_specs_reads_the_ready_file_and_logs_a_bad_one_once() {
    let pod = Pod::new();
    let dir = pod.root.join("pod");
    for id in ["ok", "bad", "none"] {
        write(&dir.join(format!("specs/{id}.md")), "# Spec\n");
    }
    write(
        &dir.join("ready/ok.md"),
        &format!("# Ready: ok\n\nready: 2026-11-08T22:10:00+11:00\ncommit: {SHA}\n"),
    );
    write(&dir.join("ready/bad.md"), "ready: soon\n");

    let read = read_files(&dir);
    let specs = by_id(read.specs);

    assert_eq!(specs["ok"].ready_at, Some(at("2026-11-08T11:10:00Z")));
    assert_eq!(specs["bad"].ready_at, None);
    assert_eq!(specs["none"].ready_at, None);
    assert_eq!(read.notices.len(), 1, "{:?}", read.notices);
    assert!(matches!(
        &read.notices[0],
        Notice::Unreadable { id, file, .. } if id == "bad" && file == "ready/bad.md"
    ));
}

#[test]
fn a_ready_spec_is_ready_not_merged_and_stays_in_progress() {
    let now = at("2026-11-09T00:00:00Z");
    let ready = SpecFacts {
        ready_at: Some(now - time::Duration::hours(2)),
        ..spec_in(
            "ready",
            SpecState::InProgress,
            now - time::Duration::hours(5),
        )
    };
    let building = spec_in("building", SpecState::InProgress, now);
    let merged = SpecFacts {
        ready_at: Some(now - time::Duration::hours(2)),
        ..spec_in("merged", SpecState::Completed, now)
    };
    let (_, lists) = work_waiting(&[ready, building, merged], 2, None, now);
    assert_eq!(lists["in_progress"].items, 2);
    assert_eq!(lists["ready_not_merged"].items, 1);
    assert!((lists["ready_not_merged"].oldest_hours - 2.0).abs() < 1e-9);
}

#[tokio::test]
async fn set_idea_count_is_exported_as_list_idea() {
    let _gauge_snapshot = GAUGE_SNAPSHOT_TESTS.lock().await;
    let product = format!("pod-test-{}", uuid::Uuid::new_v4());
    let _ = gauge_snapshot();
    let signals = PodSignals::new();
    let idea = |product: &str| {
        test_support::gauge_value(
            "pod.inventory.items",
            &[("pod.product", product), ("list", "idea")],
        )
    };
    signals.set_idea_count(&product, 3);
    assert_eq!(idea(&product), Some(3));
    // A later count replaces the earlier one.
    signals.set_idea_count(&product, 0);
    assert_eq!(idea(&product), Some(0));
}

#[tokio::test]
async fn a_signals_handle_with_no_pod_stats_drops_every_call() {
    let _gauge_snapshot = GAUGE_SNAPSHOT_TESTS.lock().await;
    let product = format!("pod-test-{}", uuid::Uuid::new_v4());
    let _ = gauge_snapshot();
    let signals = PodSignals::none();
    signals.set_idea_count(&product, 4);
    signals.record_idea_opened(&product, "an-idea", 60.0);
    assert_eq!(
        test_support::gauge_value("pod.inventory.items", &[("pod.product", product.as_str())]),
        None
    );
    assert_eq!(
        test_support::histogram_sum("pod.spec.lead_time", &[("pod.product", product.as_str())]),
        None
    );
}

#[test]
fn record_idea_opened_records_the_phase_idea_to_open() {
    let product = format!("pod-test-{}", uuid::Uuid::new_v4());
    PodSignals::new().record_idea_opened(&product, "an-idea", 90.0);
    let series = lead_time_series(&product, "idea_to_open");
    assert_eq!(series.len(), 1);
    assert_eq!(series[0].0["spec.id"], "an-idea");
    assert_eq!(
        test_support::histogram_sum("pod.spec.lead_time", &[("pod.product", product.as_str())]),
        Some(90.0)
    );
}

// --- the value line, the cause line and spec-values.json ---

#[test]
fn the_value_line_gives_the_weight_of_its_first_letter() {
    assert_eq!(spec_value("# T\n**Value.** S\n"), Some(SpecValue::S));
    assert_eq!(
        spec_value("**Value.** M, a daily nuisance\n"),
        Some(SpecValue::M)
    );
    assert_eq!(spec_value("**Value.** l\n"), Some(SpecValue::L));
    assert_eq!(SpecValue::S.weight(), 1);
    assert_eq!(SpecValue::M.weight(), 2);
    assert_eq!(SpecValue::L.weight(), 4);
    assert_eq!(SpecValue::M.as_str(), "m");
}

#[test]
fn a_spec_with_no_usable_value_line_has_no_value() {
    assert_eq!(spec_value("# T\n\nText.\n"), None);
    assert_eq!(spec_value("**Value.**\n"), None);
    assert_eq!(spec_value("**Value.** huge\n"), None);
}

#[test]
fn the_first_value_line_is_the_one_that_counts() {
    assert_eq!(
        spec_value("**Value.** S\n**Value.** L\n"),
        Some(SpecValue::S)
    );
}

#[test]
fn a_size_line_still_gives_the_size_and_no_size_line_gives_unsized() {
    assert_eq!(spec_size("**Size.** small\n"), SpecSize::Small);
    assert_eq!(spec_size("# T\n**Value.** M\n"), SpecSize::Unsized);
}

#[test]
fn the_cause_line_gives_a_spec_id_or_unknown() {
    assert_eq!(
        spec_cause("**Cause.** decision-tool\n").as_deref(),
        Some("decision-tool")
    );
    assert_eq!(
        spec_cause("**Cause.** unknown\n").as_deref(),
        Some("unknown")
    );
    assert_eq!(
        spec_cause("**Cause.** Decision-Tool (the merge)\n").as_deref(),
        Some("decision-tool")
    );
    assert_eq!(spec_cause("# T\n"), None);
    assert_eq!(spec_cause("**Cause.**\n"), None);
}

fn kept_value(snapshot: &Arc<Mutex<PodSnapshot>>, pod: &Pod, id: &str) -> Option<SpecValue> {
    snapshot.lock().unwrap().specs[&pod.name]
        .iter()
        .find(|spec| spec.id == id)
        .and_then(|spec| spec.value)
}

fn values_path(pod: &Pod) -> PathBuf {
    pod.root.join("data/spec-values.json")
}

#[tokio::test]
async fn a_value_read_before_the_approval_is_not_kept() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    let files = pod_files(&repo);
    write(&files.join("specs/v.md"), "# V\n\n**Value.** M\n");
    let (mut stats, snapshot) = stats(one_product(&pod, &repo));
    stats.poll().await;
    assert_eq!(kept_value(&snapshot, &pod, "v"), None);
    put_approval(&files, "v");
    stats.poll().await;
    assert_eq!(kept_value(&snapshot, &pod, "v"), Some(SpecValue::M));
}

#[tokio::test]
async fn a_changed_value_line_after_the_approval_keeps_the_first_and_warns_once() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    let files = pod_files(&repo);
    write(&files.join("specs/v.md"), "# V\n\n**Value.** M\n");
    put_approval(&files, "v");
    let logs = CapturedLogs::start();
    let (mut stats, snapshot) = stats(one_product(&pod, &repo));
    stats.poll().await;
    write(&files.join("specs/v.md"), "# V\n\n**Value.** L\n");
    stats.poll().await;
    stats.poll().await;
    assert_eq!(kept_value(&snapshot, &pod, "v"), Some(SpecValue::M));
    let text = logs.text();
    let line = format!(
        "spec value changed after approval pod.product={} spec.id=v kept=m read=l",
        pod.name
    );
    assert_eq!(text.matches(&line).count(), 1, "{text}");
}

#[tokio::test]
async fn the_kept_value_survives_a_restart() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    let files = pod_files(&repo);
    write(&files.join("specs/v.md"), "# V\n\n**Value.** M\n");
    put_approval(&files, "v");
    let registry = one_product(&pod, &repo);
    let (mut first, _) = stats(registry.clone());
    first.poll().await;
    drop(first);
    assert!(values_path(&pod).exists());
    write(&files.join("specs/v.md"), "# V\n\n**Value.** S\n");
    let (mut second, snapshot) = stats(registry);
    second.poll().await;
    assert_eq!(kept_value(&snapshot, &pod, "v"), Some(SpecValue::M));
}

#[tokio::test]
async fn an_unreadable_values_file_starts_empty_and_is_replaced() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    let files = pod_files(&repo);
    write(&files.join("specs/v.md"), "# V\n\n**Value.** L\n");
    put_approval(&files, "v");
    write(&values_path(&pod), "not json");
    let (mut stats, snapshot) = stats(one_product(&pod, &repo));
    stats.poll().await;
    assert_eq!(kept_value(&snapshot, &pod, "v"), Some(SpecValue::L));
    let saved = std::fs::read_to_string(values_path(&pod)).unwrap();
    assert!(
        serde_json::from_str::<serde_json::Value>(&saved).is_ok(),
        "{saved}"
    );
}

#[tokio::test]
async fn a_spec_with_no_value_line_has_no_value_and_no_cause_unless_it_has_the_line() {
    let pod = Pod::new();
    let repo = pod.repo("main");
    let files = pod_files(&repo);
    write(
        &files.join("specs/f.md"),
        "# F\n\n**Kind.** fix\n**Cause.** unknown\n",
    );
    put_approval(&files, "f");
    let (mut stats, snapshot) = stats(one_product(&pod, &repo));
    stats.poll().await;
    let specs = snapshot.lock().unwrap().specs[&pod.name].clone();
    assert_eq!(specs[0].value, None);
    assert_eq!(specs[0].cause.as_deref(), Some("unknown"));
}

#[tokio::test]
async fn the_value_gauge_sends_the_weight_of_each_kept_value_whatever_its_age() {
    let _gauge_snapshot = GAUGE_SNAPSHOT_TESTS.lock().await;
    let product = format!("pod-test-{}", uuid::Uuid::new_v4());
    let old = OffsetDateTime::now_utc() - time::Duration::days(200);
    let spec = SpecFacts {
        id: "old".into(),
        state: SpecState::Completed,
        state_at: old,
        merged_at: Some(old),
        value: Some(SpecValue::L),
        ..SpecFacts::default()
    };
    let unvalued = SpecFacts {
        id: "none".into(),
        ..SpecFacts::default()
    };
    let snapshot = gauge_snapshot();
    *snapshot.lock().unwrap() = PodSnapshot {
        specs: BTreeMap::from([(product.clone(), vec![spec, unvalued])]),
        ..PodSnapshot::default()
    };
    assert_eq!(
        test_support::gauge_value(
            "pod.spec.value",
            &[
                ("pod.product", product.as_str()),
                ("spec.id", "old"),
                ("spec.value", "l")
            ],
        ),
        Some(4)
    );
    assert_eq!(
        test_support::gauge_value(
            "pod.spec.value",
            &[("pod.product", product.as_str()), ("spec.id", "none")]
        ),
        None
    );
    *snapshot.lock().unwrap() = PodSnapshot::default();
}

#[tokio::test]
async fn the_effort_gauge_carries_the_value_and_the_cause() {
    let _gauge_snapshot = GAUGE_SNAPSHOT_TESTS.lock().await;
    let product = format!("pod-test-{}", uuid::Uuid::new_v4());
    let valued = SpecFacts {
        id: "valued".into(),
        state: SpecState::InProgress,
        state_at: OffsetDateTime::now_utc(),
        kind: SpecKind::Feature,
        value: Some(SpecValue::M),
        ..SpecFacts::default()
    };
    let fix = SpecFacts {
        id: "fixed".into(),
        state: SpecState::InProgress,
        state_at: OffsetDateTime::now_utc(),
        kind: SpecKind::Fix,
        cause: Some("decision-tool".into()),
        ..SpecFacts::default()
    };
    let snapshot = gauge_snapshot();
    *snapshot.lock().unwrap() = PodSnapshot {
        specs: BTreeMap::from([(product.clone(), vec![valued, fix])]),
        ..PodSnapshot::default()
    };
    let read = |id: &str, extra: (&str, &str)| {
        test_support::gauge_f64_value(
            "pod.spec.effort",
            &[
                ("pod.product", product.as_str()),
                ("spec.id", id),
                ("measure", "revisions"),
                extra,
            ],
        )
    };
    assert_eq!(read("valued", ("spec.value", "m")), Some(0.0));
    assert_eq!(read("fixed", ("spec.value", "none")), Some(0.0));
    assert_eq!(read("fixed", ("spec.cause", "decision-tool")), Some(0.0));
    assert_eq!(read("valued", ("spec.cause", "decision-tool")), None);
    *snapshot.lock().unwrap() = PodSnapshot::default();
}

// The named queries of the spec cost-per-value, on fixture rows. The fixture rows are literal
// constants in a SELECT: the test replaces each table of the query with them, so it reads no
// real data and writes nothing. It asks the read-only tool `bin/logq`. It skips when the tool
// or ClickHouse cannot be reached (CI has none).

mod cost_per_value_fixture {
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    const QUERIES: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../docs/references/telemetry/queries"
    );
    const DAY: u32 = 24;
    const USED: &str = "p.feature.used";
    const NULL: &str = "\\N";

    fn logq() -> Option<PathBuf> {
        let path = std::env::var_os("LOGQ").map_or_else(|| PathBuf::from("logq"), PathBuf::from);
        (run(&path, "SELECT 1")? == "1").then_some(path)
    }

    fn run(logq: &Path, sql: &str) -> Option<String> {
        let mut child = Command::new(logq)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        child.stdin.take()?.write_all(sql.as_bytes()).ok()?;
        let done = child.wait_with_output().ok()?;
        done.status
            .success()
            .then(|| String::from_utf8_lossy(&done.stdout).trim().to_owned())
    }

    #[derive(Clone)]
    struct Row {
        metric: &'static str,
        spec: &'static str,
        hours_ago: u32,
        value: f64,
        extra: Vec<(&'static str, &'static str)>,
    }

    fn row(metric: &'static str, spec: &'static str, hours_ago: u32, value: f64) -> Row {
        Row {
            metric,
            spec,
            hours_ago,
            value,
            extra: Vec::new(),
        }
    }

    fn with(mut row: Row, extra: &[(&'static str, &'static str)]) -> Row {
        row.extra.extend_from_slice(extra);
        row
    }

    const EMPTY: &str = "SELECT '' AS MetricName, CAST(map(), 'Map(String, String)') AS Attributes, \
         CAST(map(), 'Map(String, String)') AS ResourceAttributes, \
         CAST(now() AS DateTime64(9)) AS TimeUnix, toFloat64(0) AS Value, \
         toUInt64(0) AS Count, toFloat64(0) AS Sum WHERE 0";

    fn table(rows: &[Row]) -> String {
        let mut selects: Vec<String> = rows
            .iter()
            .map(|row| {
                let mut attributes = format!("'spec.id', '{}', 'pod.product', 'p'", row.spec);
                for (key, value) in &row.extra {
                    attributes.push_str(&format!(", '{key}', '{value}'"));
                }
                format!(
                    "SELECT '{}' AS MetricName, map({attributes}) AS Attributes, \
                     map('service.name', 'h') AS ResourceAttributes, \
                     toDateTime64(now() - toIntervalHour({}), 9) AS TimeUnix, \
                     toFloat64({}) AS Value, toUInt64(1) AS Count, toFloat64(0) AS Sum",
                    row.metric, row.hours_ago, row.value
                )
            })
            .collect();
        selects.push(EMPTY.to_owned());
        format!("(\n{}\n)", selects.join("\nUNION ALL\n"))
    }

    /// The `chief.attention_answered` lines: (hours ago, seconds).
    fn logs(lines: &[(u32, u32)]) -> String {
        let mut selects: Vec<String> = lines
            .iter()
            .map(|(hours_ago, seconds)| {
                format!(
                    "SELECT toDateTime64(now() - toIntervalHour({hours_ago}), 9) AS Timestamp, \
                     'agent event' AS Body, map('agent.name', 'chief.attention_answered', \
                     'agent.attr.seconds', '{seconds}') AS LogAttributes"
                )
            })
            .collect();
        selects.push(
            "SELECT now64(9) AS Timestamp, '' AS Body, \
             CAST(map(), 'Map(String, String)') AS LogAttributes WHERE 0"
                .to_owned(),
        );
        format!("(\n{}\n)", selects.join("\nUNION ALL\n"))
    }

    fn settings(rows: &[(&str, f64)]) -> String {
        let mut selects: Vec<String> = rows
            .iter()
            .map(|(name, value)| {
                format!("SELECT '{name}' AS name, toFloat64({value}) AS value, 'X' AS currency")
            })
            .collect();
        selects.push("SELECT '' AS name, toFloat64(0) AS value, '' AS currency WHERE 0".to_owned());
        format!("(\n{}\n)", selects.join("\nUNION ALL\n"))
    }

    #[derive(Default)]
    struct Data {
        sum: Vec<Row>,
        gauge: Vec<Row>,
        histogram: Vec<Row>,
        logs: Vec<(u32, u32)>,
        settings: Vec<(&'static str, f64)>,
    }

    fn answer(logq: &Path, file: &str, data: &Data) -> String {
        let text = std::fs::read_to_string(format!("{QUERIES}/{file}")).expect("read the query");
        answer_text(logq, &text, data)
    }

    fn answer_text(logq: &Path, text: &str, data: &Data) -> String {
        let sql = text
            .replace("otel.otel_metrics_sum", &table(&data.sum))
            .replace("otel.otel_metrics_gauge", &table(&data.gauge))
            .replace("otel.otel_metrics_histogram", &table(&data.histogram))
            .replace("otel.otel_logs", &logs(&data.logs))
            .replace("otel.harold_settings", &settings(&data.settings));
        run(logq, &sql).unwrap_or_else(|| panic!("the query failed on the fixture:\n{sql}"))
    }

    const RATES: [(&str, f64); 3] = [
        ("harold.exchange_rate.usd_to_aud", 2.0),
        ("harold.attention.hourly_rate", 100.0),
        ("harold.list_price.per_mtok/m1/input", 10.0),
    ];

    fn weight(spec: &'static str, value: &'static str) -> Row {
        let number = match value {
            "s" => 1.0,
            "m" => 2.0,
            _ => 4.0,
        };
        with(
            row("pod.spec.value", spec, 2 * DAY, number),
            &[("spec.value", value)],
        )
    }

    fn effort(spec: &'static str, kind: &'static str) -> Row {
        with(
            row("pod.spec.effort", spec, 2 * DAY, 0.0),
            &[("spec.kind", kind), ("state", "completed")],
        )
    }

    fn merged(spec: &'static str, hours_ago: u32) -> Row {
        with(
            row("pod.spec.lead_time", spec, hours_ago, 0.0),
            &[("phase", "started_to_completed")],
        )
    }

    fn tokens(metric: &'static str, spec: &'static str, hours_ago: u32, value: f64) -> Row {
        with(
            row(metric, spec, hours_ago, value),
            &[("model", "m1"), ("kind", "input"), ("role", "build_worker")],
        )
    }

    fn used(spec: &'static str, days_ago: u32) -> Vec<Row> {
        vec![
            row(USED, spec, 20 * DAY, 0.0),
            row(USED, spec, days_ago * DAY, 1.0),
        ]
    }

    /// The columns of the one row of `cost-per-value.sql`.
    fn columns(logq: &Path, data: &Data) -> Vec<String> {
        answer(logq, "cost-per-value.sql", data)
            .split('\t')
            .map(str::to_owned)
            .collect()
    }

    // The columns are: cost_per_value, cost_per_value_change_pct, value, total_cost,
    // token_cost, attention_cost, cloud_cost, fix_cost, unpriced_models, unvalued.
    #[test]
    #[ignore = "opens a port: runs in the container test set and in CI"]
    fn cost_per_value_sql_on_a_clickhouse_fixture() {
        let Some(logq) = logq() else {
            eprintln!("skipped: bin/logq or ClickHouse cannot be reached");
            return;
        };
        let spent = |hours_ago, value| tokens("harold.turn.tokens", "", hours_ago, value);
        let base = |extra: Vec<Row>| Data {
            sum: [used("a", 2), used("b", 3)]
                .concat()
                .into_iter()
                .chain(extra)
                .collect(),
            gauge: vec![weight("a", "m"), weight("b", "l")],
            settings: RATES.to_vec(),
            ..Data::default()
        };

        // An M feature and an L feature used give 6. 1 million tokens at 10 USD and a rate of 2
        // give 20 AUD and one hour of attention gives 100: 120 for 6 is 20.
        let mut data = base(vec![spent(20 * DAY, 0.0), spent(2 * DAY, 1_000_000.0)]);
        data.logs = vec![(10, 3600)];
        let got = columns(&logq, &data);
        assert_eq!(&got[..6], ["20", NULL, "6", "120", "20", "100"], "{got:?}");

        // A fix adds no value and its cost is in fix_cost and in total_cost.
        let by_spec =
            |hours_ago, value| tokens("harold.turn.tokens.by_spec", "f", hours_ago, value);
        let mut data = base(vec![
            spent(20 * DAY, 0.0),
            spent(2 * DAY, 2_000_000.0),
            by_spec(20 * DAY, 0.0),
            by_spec(2 * DAY, 1_000_000.0),
        ]);
        data.gauge.push(effort("f", "fix"));
        let got = columns(&logq, &data);
        assert_eq!(
            (&got[2], &got[3], &got[7]),
            (&"6".into(), &"40".into(), &"20".into()),
            "{got:?}"
        );

        // An other spec with S and a merge in the 7 days adds 1, and one merged 8 days ago adds nothing.
        let other = |hours_ago| Data {
            sum: used("a", 2),
            gauge: vec![weight("a", "m"), weight("o", "s"), effort("o", "other")],
            histogram: vec![merged("o", hours_ago)],
            settings: RATES.to_vec(),
            ..Data::default()
        };
        assert_eq!(columns(&logq, &other(2 * DAY))[2], "3");
        assert_eq!(columns(&logq, &other(8 * DAY))[2], "2");

        // A feature spec with a value and a merge in the 7 days and no use adds nothing.
        let mut data = other(2 * DAY);
        data.gauge = vec![weight("a", "m"), weight("g", "l"), effort("g", "feature")];
        data.histogram = vec![merged("g", 2 * DAY)];
        assert_eq!(columns(&logq, &data)[2], "2");

        // A used feature with no value counts 1 and is in unvalued.
        let mut data = base(vec![]);
        data.gauge = vec![weight("a", "m")];
        let got = columns(&logq, &data);
        assert_eq!((&got[2], &got[9]), (&"3".into(), &"1".into()), "{got:?}");

        // No exchange rate gives the attention cost only.
        let mut data = base(vec![spent(20 * DAY, 0.0), spent(2 * DAY, 1_000_000.0)]);
        data.logs = vec![(10, 3600)];
        data.settings = RATES[1..].to_vec();
        let got = columns(&logq, &data);
        assert_eq!(&got[3..7], ["100", NULL, "100", NULL], "{got:?}");

        // A rise of harold.workflow.cost adds to cloud_cost as it is, in AUD: 6.
        let workflow = |hours_ago, value| {
            with(
                row("harold.workflow.cost", "", hours_ago, value),
                &[("workflow", "watch"), ("model", "m1")],
            )
        };
        let data = base(vec![workflow(20 * DAY, 0.0), workflow(2 * DAY, 6.0)]);
        let got = columns(&logq, &data);
        assert_eq!((&got[3], &got[6]), (&"6".into(), &"6".into()), "{got:?}");

        // A watch run is in harold.workflow.cost and counts once: watch.cost_usd adds nothing.
        let watch = |hours_ago, value| {
            with(
                row("watch.cost_usd", "", hours_ago, value),
                &[("folder", "x")],
            )
        };
        let data = base(vec![
            workflow(20 * DAY, 0.0),
            workflow(2 * DAY, 6.0),
            watch(20 * DAY, 0.0),
            watch(2 * DAY, 3.0),
        ]);
        let got = columns(&logq, &data);
        assert_eq!((&got[3], &got[6]), (&"6".into(), &"6".into()), "{got:?}");

        // A model with workflow tokens and no workflow cost is named in unpriced_models.
        let mut data = base(vec![workflow(20 * DAY, 0.0), workflow(2 * DAY, 6.0)]);
        data.sum.push(with(
            row("harold.workflow.tokens", "", 2 * DAY, 5.0),
            &[
                ("workflow", "watch"),
                ("model", "wf-unpriced"),
                ("kind", "input"),
            ],
        ));
        assert_eq!(columns(&logq, &data)[8], "wf-unpriced");

        // Build hours of a spec add nothing to the cost.
        let mut data = base(vec![]);
        data.gauge.push(with(
            row("pod.spec.effort", "a", 2 * DAY, 9.0),
            &[("measure", "build_worker_hours"), ("spec.kind", "feature")],
        ));
        assert_eq!(columns(&logq, &data)[3], NULL);

        // No value gives a NULL cost_per_value and a total_cost.
        let data = Data {
            sum: vec![spent(20 * DAY, 0.0), spent(2 * DAY, 1_000_000.0)],
            settings: RATES.to_vec(),
            ..Data::default()
        };
        let got = columns(&logq, &data);
        assert_eq!(
            (&got[0], &got[2], &got[3]),
            (&NULL.to_owned(), &"0".into(), &"20".into()),
            "{got:?}"
        );

        // An unpriced model is named.
        let mut data = base(vec![]);
        data.sum.push(with(
            row("harold.turn.tokens", "", 2 * DAY, 5.0),
            &[("model", "zz"), ("kind", "input"), ("role", "x")],
        ));
        assert_eq!(columns(&logq, &data)[8], "zz");

        // The cost per value of this week against last week: both 10, so the change is 0.
        let mut data = base(vec![
            spent(20 * DAY, 0.0),
            spent(10 * DAY, 1_000_000.0),
            spent(2 * DAY, 2_000_000.0),
        ]);
        data.sum.truncate(2);
        data.sum.extend(used("b", 10));
        data.sum.extend([
            spent(20 * DAY, 0.0),
            spent(10 * DAY, 1_000_000.0),
            spent(2 * DAY, 2_000_000.0),
        ]);
        data.gauge = vec![weight("a", "m"), weight("b", "m")];
        let got = columns(&logq, &data);
        assert_eq!(&got[..3], ["10", "0", "2"], "{got:?}");
    }

    #[test]
    #[ignore = "opens a port: runs in the container test set and in CI"]
    fn fix_cost_by_cause_sql_on_a_clickhouse_fixture() {
        let Some(logq) = logq() else {
            eprintln!("skipped: bin/logq or ClickHouse cannot be reached");
            return;
        };
        let fix = |spec, cause, hours_ago| {
            with(
                row("pod.spec.effort", spec, hours_ago, 0.0),
                &[
                    ("spec.kind", "fix"),
                    ("spec.cause", cause),
                    ("state", "completed"),
                ],
            )
        };
        let spent =
            |spec, hours_ago, value| tokens("harold.turn.tokens.by_spec", spec, hours_ago, value);
        let data = Data {
            gauge: vec![
                fix("f1", "decision-tool", 2 * DAY),
                fix("f2", "decision-tool", 3 * DAY),
            ],
            sum: vec![
                spent("f1", 20 * DAY, 0.0),
                spent("f1", 2 * DAY, 1_000_000.0),
                spent("f2", 20 * DAY, 0.0),
                spent("f2", 2 * DAY, 500_000.0),
            ],
            settings: RATES.to_vec(),
            ..Data::default()
        };
        let text = answer(&logq, "fix-cost-by-cause.sql", &data);
        let fields: Vec<&str> = text.split('\t').collect();
        // Two fixes with one cause give one row: 1.5 million tokens at 10 USD and a rate of 2.
        assert_eq!(text.lines().count(), 1, "{text}");
        assert_eq!(&fields[..3], ["decision-tool", "2", "30"], "{text}");
    }
    #[test]
    #[ignore = "opens a port: runs in the container test set and in CI"]
    fn slicing_and_sizing_sql_on_a_clickhouse_fixture() {
        let Some(logq) = logq() else {
            eprintln!("skipped: bin/logq or ClickHouse cannot be reached");
            return;
        };
        let hours = |spec, size, value| {
            with(
                row("pod.spec.effort", spec, 2 * DAY, value),
                &[
                    ("measure", "build_worker_hours"),
                    ("spec.size", size),
                    ("state", "completed"),
                ],
            )
        };
        // Two unsized specs with 9 and 3 build hours and one small spec with 5.
        let data = Data {
            gauge: vec![
                hours("a", "unsized", 9.0),
                hours("b", "unsized", 3.0),
                hours("c", "small", 5.0),
            ],
            ..Data::default()
        };
        let text = std::fs::read_to_string(format!("{QUERIES}/slicing-and-sizing.sql")).unwrap();
        let got = answer_text(&logq, &text.replace("$__filters", "1"), &data);
        let find = |series: &str| {
            got.lines()
                .find(|line| line.starts_with(series))
                .unwrap_or_else(|| panic!("no row {series}:\n{got}"))
                .split('\t')
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        // One of the three specs is over 8 hours.
        let over = find("specs over 8 build hours, share of specs (%)");
        assert!(over[1].starts_with("33.33"), "{over:?}");
        // The share rows count only the small spec: 5 hours is over the limit of 4.
        assert_eq!(find("specs counted: small")[1], "1", "{got}");
        assert_eq!(
            find("over limit, share of specs (%): small")[1],
            "100",
            "{got}"
        );
        assert!(!got.contains("specs counted: unsized"), "{got}");
    }
}
