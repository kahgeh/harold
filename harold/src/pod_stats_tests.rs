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
fn awaiting_is_true_only_for_the_exact_first_line() {
    assert!(spec_awaiting("Waiting for your approval.\n\nText"));
    assert!(spec_awaiting("  Waiting for your approval.  \nText"));
    assert!(!spec_awaiting("# T\nWaiting for your approval.\n"));
    assert!(!spec_awaiting("Waiting for your approval\n"));
    assert!(!spec_awaiting("Starts under the small-fix waiver.\n"));
    assert!(!spec_awaiting(""));
}

#[test]
fn defaults_for_size_and_awaiting() {
    let facts = SpecFacts::default();
    assert_eq!(facts.size, SpecSize::Unsized);
    assert!(!facts.awaiting);
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
    assert_eq!(lists.len(), 3);
    for name in ["awaiting_approval", "approved_not_started", "in_progress"] {
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
            text.matches("pod notices unreadable reason=\"save\"")
                .count(),
            1,
            "{text}"
        );
    }
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
        text.matches("pod notices unreadable reason=\"save\"")
            .count(),
        1,
        "{text}"
    );
    assert_eq!(text.matches("pod notices unreadable").count(), 1, "{text}");
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
