use std::path::{Path, PathBuf};
use std::time::Duration;

use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, UtcOffset};
use tokio::sync::watch;

use super::{PERIOD, publish, record_releases, register, run};
use crate::outbound::fake::CapturedLogs;
use crate::pod_stats::{
    Git, GitOutput, INVENTORY_ITEM_DAYS, INVENTORY_ITEMS, INVENTORY_OLDEST_HOURS, POD_FOLDER,
};
use crate::telemetry::test_support;
use crate::tools::release::gather::gather_tests::{FakeGit, Fixture};
use crate::tools::release::service::ReleaseToolService;
use crate::tools::tests::TempFolder;

const NOW: &str = "2026-11-03T00:00:00Z";

fn now() -> OffsetDateTime {
    OffsetDateTime::parse(NOW, &Rfc3339).expect("a valid time")
}

fn write(path: PathBuf, text: &str) {
    std::fs::create_dir_all(path.parent().expect("a parent folder")).expect("create the folder");
    std::fs::write(path, text).expect("write the file");
}

/// Lets the spawned task run to its next wait.
async fn let_task_run() {
    for _ in 0..5 {
        tokio::task::yield_now().await;
    }
}

/// Waits, in real time, until `expected` publishes have finished. Paused time does not move.
async fn wait_for_done(expected: u32) {
    let started = std::time::Instant::now();
    loop {
        let done = super::PUBLISHES_DONE.with(|count| count.get());
        if done >= expected {
            return;
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "publishes done {done}, expected {expected}, started {}",
            super::PUBLISHES.with(|count| count.get())
        );
        tokio::task::yield_now().await;
    }
}

/// A fresh product name, so a test reads only its own series from the shared store.
fn unique_name() -> String {
    format!("release-test-{}", uuid::Uuid::new_v4())
}

/// A registry with one product `name` on the repo of `fixture`.
fn registry(folder: &TempFolder, fixture: &Fixture, name: &str, paused: bool) -> PathBuf {
    let path = folder.0.join("products.toml");
    let text = format!(
        "[products.{name}]\nrepo = \"{}\"\nbase = \"main\"\npaused = {paused}\n",
        fixture.repo.display()
    );
    std::fs::write(&path, text).expect("write the registry");
    path
}

fn merge_on(fixture: &Fixture, id: &str, time: &str) {
    fixture.spec(id, &format!("# {id}: Title\n"));
    write(
        fixture
            .repo
            .join(POD_FOLDER)
            .join(format!("merged/{id}.md")),
        &format!(
            "# Merged: {id}\n\nmerged: {time}\ncommit: 0123456789abcdef0123456789abcdef01234567\n"
        ),
    );
}

/// The arguments of the git call that finds the newest release tag.
const DESCRIBE: [&str; 6] = [
    "describe",
    "--tags",
    "--abbrev=0",
    "--match",
    "v[0-9]*",
    "origin/main",
];

/// The answers of git for a product whose newest release is `v1.0.0`, tagged before both merges.
fn with_tag(git: FakeGit) -> FakeGit {
    let unix = OffsetDateTime::parse("2026-10-01T00:00:00Z", &Rfc3339)
        .expect("a valid time")
        .unix_timestamp()
        .to_string();
    git.answer(&DESCRIBE, true, "v1.0.0\n").answer(
        &["log", "-1", "--format=%ct", "v1.0.0"],
        true,
        &unix,
    )
}

/// A service on a product `name` with two merged specs, 2 and 4 days old at `NOW`, and a
/// release tag. The fixture owns the folders, so the caller keeps it until the end of the test.
fn two_merged(folder: &TempFolder, name: &str) -> (ReleaseToolService<FakeGit>, Fixture) {
    let (_, fixture) = two_merged_no_tag(folder, name);
    let registry = folder.0.join("products.toml");
    let service = ReleaseToolService::with_git(Some(registry), with_tag(fixture.git()));
    (service, fixture)
}

/// The same product with no release tag.
fn two_merged_no_tag(folder: &TempFolder, name: &str) -> (ReleaseToolService<FakeGit>, Fixture) {
    let fixture = Fixture::new();
    merge_on(&fixture, "alpha", "2026-11-01T00:00:00Z");
    merge_on(&fixture, "beta", "2026-10-30T00:00:00Z");
    let registry = registry(folder, &fixture, name, false);
    let service = ReleaseToolService::with_git(Some(registry), fixture.git());
    (service, fixture)
}

/// The unit of the metric `name` in the latest export.
fn unit_of(name: &str) -> Option<String> {
    test_support::metrics().iter().rev().find_map(|resource| {
        resource
            .scope_metrics()
            .flat_map(|scope| scope.metrics())
            .find(|metric| metric.name() == name)
            .map(|metric| metric.unit().to_string())
    })
}

#[tokio::test]
async fn merged_not_released_has_items_oldest_age_and_item_days() {
    test_support::install();
    register();
    let folder = TempFolder::new();
    let name = unique_name();
    let (service, _fixture) = two_merged(&folder, &name);

    publish(&service, now(), UtcOffset::UTC).await;

    let attrs = [
        ("pod.product", name.as_str()),
        ("list", "merged_not_released"),
    ];
    assert_eq!(
        test_support::gauge_value(INVENTORY_ITEMS.0, &attrs),
        Some(2)
    );
    // The oldest spec is 4 days (96 h) old. The two specs sum to 6 days.
    assert_eq!(
        test_support::gauge_f64_value(INVENTORY_OLDEST_HOURS.0, &attrs),
        Some(96.0)
    );
    assert_eq!(
        test_support::gauge_f64_value(INVENTORY_ITEM_DAYS.0, &attrs),
        Some(6.0)
    );
}

#[tokio::test]
async fn the_series_uses_the_names_and_units_pod_stats_exports() {
    test_support::install();
    register();
    let folder = TempFolder::new();
    let name = unique_name();
    let (service, _fixture) = two_merged(&folder, &name);
    publish(&service, now(), UtcOffset::UTC).await;

    for (constant, expected_name) in [
        (INVENTORY_ITEMS, "pod.inventory.items"),
        (INVENTORY_OLDEST_HOURS, "pod.inventory.oldest_hours"),
        (INVENTORY_ITEM_DAYS, "pod.inventory.item_days"),
    ] {
        assert_eq!(constant.0, expected_name);
        assert_eq!(
            unit_of(constant.0).as_deref(),
            Some(constant.1),
            "{expected_name}"
        );
    }
}

#[tokio::test]
async fn a_paused_product_publishes_nothing() {
    test_support::install();
    register();
    let folder = TempFolder::new();
    let fixture = Fixture::new();
    merge_on(&fixture, "alpha", "2026-11-01T00:00:00Z");
    let name = unique_name();
    let service = ReleaseToolService::with_git(
        Some(registry(&folder, &fixture, &name, true)),
        fixture.git(),
    );

    publish(&service, now(), UtcOffset::UTC).await;

    assert_eq!(
        test_support::gauge_value(INVENTORY_ITEMS.0, &[("pod.product", name.as_str())]),
        None
    );
}

#[tokio::test]
async fn a_product_with_no_git_answer_publishes_nothing() {
    test_support::install();
    register();
    let folder = TempFolder::new();
    let fixture = Fixture::new();
    merge_on(&fixture, "alpha", "2026-11-01T00:00:00Z");
    let name = unique_name();
    let git = fixture
        .git()
        .no_answer(&["worktree", "list", "--porcelain"]);
    let service =
        ReleaseToolService::with_git(Some(registry(&folder, &fixture, &name, false)), git);

    publish(&service, now(), UtcOffset::UTC).await;

    assert_eq!(
        test_support::gauge_value(INVENTORY_ITEMS.0, &[("pod.product", name.as_str())]),
        None
    );
}

#[tokio::test(start_paused = true)]
async fn the_task_publishes_every_ten_minutes() {
    let folder = TempFolder::new();
    let (service, _fixture) = two_merged(&folder, &unique_name());
    let (shutdown, receiver) = watch::channel(());
    super::PUBLISHES.with(|count| count.set(0));
    super::PUBLISHES_DONE.with(|count| count.set(0));

    let task = tokio::spawn(run(service, tips_in(&folder), PERIOD, receiver));
    wait_for_done(1).await;
    let_task_run().await;
    let at_start = super::PUBLISHES.with(|count| count.get());
    tokio::time::advance(PERIOD).await;
    wait_for_done(2).await;
    let_task_run().await;
    let after_one_period = super::PUBLISHES.with(|count| count.get());
    tokio::time::advance(PERIOD).await;
    wait_for_done(3).await;
    let_task_run().await;
    let after_two_periods = super::PUBLISHES.with(|count| count.get());

    assert_eq!((at_start, after_one_period, after_two_periods), (1, 2, 3));
    assert_eq!(PERIOD, Duration::from_secs(600));
    drop(shutdown);
    task.await.expect("the task ends");
}

#[tokio::test(start_paused = true)]
async fn the_task_ends_at_shutdown() {
    let folder = TempFolder::new();
    let (service, _fixture) = two_merged(&folder, &unique_name());
    let (shutdown, receiver) = watch::channel(());
    let task = tokio::spawn(run(service, tips_in(&folder), PERIOD, receiver));
    let_task_run().await;

    shutdown.send(()).expect("a receiver is listening");

    tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("the task ends within a second")
        .expect("the task does not panic");
}

fn items_of(name: &str) -> Option<u64> {
    test_support::gauge_value(
        INVENTORY_ITEMS.0,
        &[("pod.product", name), ("list", "merged_not_released")],
    )
}

#[tokio::test]
async fn a_product_that_left_the_registry_ends_its_series() {
    test_support::install();
    register();
    let folder = TempFolder::new();
    let name = unique_name();
    let (service, fixture) = two_merged(&folder, &name);
    publish(&service, now(), UtcOffset::UTC).await;
    assert_eq!(items_of(&name), Some(2));

    // The same registry file now lists another product.
    let other = unique_name();
    registry(&folder, &fixture, &other, false);
    publish(&service, now(), UtcOffset::UTC).await;

    assert_eq!(items_of(&name), None);
    assert_eq!(items_of(&other), Some(2));
}

#[tokio::test]
async fn a_failed_read_keeps_the_last_numbers() {
    test_support::install();
    register();
    let folder = TempFolder::new();
    let name = unique_name();
    let (service, _fixture) = two_merged(&folder, &name);
    publish(&service, now(), UtcOffset::UTC).await;
    assert_eq!(items_of(&name), Some(2));

    std::fs::write(folder.0.join("products.toml"), "not [ valid toml").expect("break the registry");
    publish(&service, now(), UtcOffset::UTC).await;

    assert_eq!(items_of(&name), Some(2));
}

/// A `Git` that waits an hour of (paused) time before it asks the `FakeGit`.
struct SlowGit(FakeGit);

impl Git for SlowGit {
    async fn run(&self, repo: &Path, args: &[&str]) -> Option<GitOutput> {
        tokio::time::sleep(Duration::from_secs(3600)).await;
        self.0.run(repo, args).await
    }
}

#[tokio::test(start_paused = true)]
async fn shutdown_drops_a_publish_in_flight() {
    let folder = TempFolder::new();
    let fixture = Fixture::new();
    merge_on(&fixture, "alpha", "2026-11-01T00:00:00Z");
    let registry = registry(&folder, &fixture, &unique_name(), false);
    let service = ReleaseToolService::with_git(Some(registry), SlowGit(fixture.git()));
    let (shutdown, receiver) = watch::channel(());
    super::PUBLISHES.with(|count| count.set(0));

    let task = tokio::spawn(run(service, tips_in(&folder), PERIOD, receiver));
    let_task_run().await;
    // The first publish has started and waits on the slow git.
    assert_eq!(super::PUBLISHES.with(|count| count.get()), 1);
    assert!(!task.is_finished());

    shutdown.send(()).expect("a receiver is listening");
    let_task_run().await;

    // No time passes: the task ends at once and does not wait for the git calls.
    assert!(task.is_finished());
    task.await.expect("the task does not panic");
}

#[tokio::test]
async fn a_product_with_no_release_tag_publishes_no_merged_not_released_series() {
    test_support::install();
    register();
    let folder = TempFolder::new();
    let name = unique_name();
    let (service, _fixture) = two_merged_no_tag(&folder, &name);

    publish(&service, now(), UtcOffset::UTC).await;

    for metric in [
        INVENTORY_ITEMS.0,
        INVENTORY_OLDEST_HOURS.0,
        INVENTORY_ITEM_DAYS.0,
    ] {
        let attrs = [
            ("pod.product", name.as_str()),
            ("list", "merged_not_released"),
        ];
        assert_eq!(test_support::gauge_value(metric, &attrs), None, "{metric}");
        assert_eq!(
            test_support::gauge_f64_value(metric, &attrs),
            None,
            "{metric}"
        );
    }
}

#[tokio::test]
async fn the_command_list_of_a_product_with_no_release_tag_keeps_the_last_14_days() {
    let folder = TempFolder::new();
    let name = unique_name();
    let (service, _fixture) = two_merged_no_tag(&folder, &name);

    let products = service
        .products("", now(), UtcOffset::UTC)
        .await
        .expect("the lists");

    let product = products
        .iter()
        .find(|product| product.name == name)
        .expect("the product");
    assert_eq!(product.release, "");
    assert_eq!(product.merged.len(), 2);
    assert_eq!(product.merged_at.len(), 2);
}

#[tokio::test]
async fn a_first_release_tag_brings_the_series_back() {
    test_support::install();
    register();
    let folder = TempFolder::new();
    let name = unique_name();
    let (untagged, fixture) = two_merged_no_tag(&folder, &name);
    publish(&untagged, now(), UtcOffset::UTC).await;
    assert_eq!(items_of(&name), None);

    let registry = folder.0.join("products.toml");
    let tagged = ReleaseToolService::with_git(Some(registry), with_tag(fixture.git()));
    publish(&tagged, now(), UtcOffset::UTC).await;

    assert_eq!(items_of(&name), Some(2));
}

#[tokio::test]
async fn a_product_that_loses_its_tag_ends_its_series() {
    test_support::install();
    register();
    let folder = TempFolder::new();
    let name = unique_name();
    let (tagged, fixture) = two_merged(&folder, &name);
    publish(&tagged, now(), UtcOffset::UTC).await;
    assert_eq!(items_of(&name), Some(2));

    let registry = folder.0.join("products.toml");
    let untagged = ReleaseToolService::with_git(Some(registry), fixture.git());
    publish(&untagged, now(), UtcOffset::UTC).await;

    assert_eq!(items_of(&name), None);
}

#[tokio::test]
async fn this_module_publishes_only_the_merged_not_released_list() {
    test_support::install();
    register();
    let folder = TempFolder::new();
    let name = unique_name();
    let (service, _fixture) = two_merged(&folder, &name);

    publish(&service, now(), UtcOffset::UTC).await;

    // The other three lists belong to pod stats. This module never writes them, so the
    // skip of a product with no tag cannot reach them.
    for list in ["awaiting_approval", "approved_not_started", "in_progress"] {
        let attrs = [("pod.product", name.as_str()), ("list", list)];
        assert_eq!(test_support::gauge_value(INVENTORY_ITEMS.0, &attrs), None);
    }
    assert_eq!(items_of(&name), Some(2));
}

// Tests of `record_releases`: the phase `merged_to_released` and the file `release-tips`.

const ALPHA: &str = "0123456789abcdef0123456789abcdef01234567";
const BETA: &str = "fedcba9876543210fedcba9876543210fedcba98";
const GAMMA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
/// The time of the tag `v1.1.0`.
const TAG_TIME: &str = "2026-11-02T00:00:00Z";
const PHASE: &str = "merged_to_released";

fn merge_commit(fixture: &Fixture, id: &str, merged: Option<&str>, commit: &str) {
    fixture.spec(id, &format!("# {id}: Title\n"));
    let merged = merged
        .map(|time| format!("merged: {time}\n"))
        .unwrap_or_default();
    write(
        fixture
            .repo
            .join(POD_FOLDER)
            .join(format!("merged/{id}.md")),
        &format!("# Merged: {id}\n\n{merged}commit: {commit}\n"),
    );
}

/// A product `name` whose newest tag is `v1.1.0` and whose tag `v1.0.0` reaches every commit
/// but `printed`. Three specs merged: alpha 1 day and beta 3 days before the tag, gamma
/// before the tag too but already in `v1.0.0`.
fn released(
    folder: &TempFolder,
    name: &str,
    printed: &[&str],
) -> (ReleaseToolService<FakeGit>, Fixture) {
    let fixture = Fixture::new();
    merge_commit(&fixture, "alpha", Some("2026-11-01T00:00:00Z"), ALPHA);
    merge_commit(&fixture, "beta", Some("2026-10-30T00:00:00Z"), BETA);
    merge_commit(&fixture, "gamma", Some("2026-10-20T00:00:00Z"), GAMMA);
    let registry = registry(folder, &fixture, name, false);
    let unix = OffsetDateTime::parse(TAG_TIME, &Rfc3339)
        .expect("a valid time")
        .unix_timestamp()
        .to_string();
    let listing: String = printed.iter().map(|hash| format!("{hash}\n")).collect();
    let git = fixture
        .git()
        .answer(&DESCRIBE, true, "v1.1.0\n")
        .answer(
            &[
                "for-each-ref",
                "--format=%(creatordate:unix)",
                "refs/tags/v1.1.0",
            ],
            true,
            &unix,
        )
        .answer(&["rev-list", "v1.1.0", "^v1.0.0"], true, &listing)
        .answer(&["rev-list", "v1.1.0", "^v0.9.0"], false, "");
    (ReleaseToolService::with_git(Some(registry), git), fixture)
}

fn tips_in(folder: &TempFolder) -> PathBuf {
    folder.0.join("pod/release-tips")
}

fn recorded(name: &str, id: &str) -> Option<f64> {
    test_support::histogram_sum(
        "pod.spec.lead_time",
        &[("pod.product", name), ("phase", PHASE), ("spec.id", id)],
    )
}

fn saved(folder: &TempFolder) -> String {
    std::fs::read_to_string(tips_in(folder)).unwrap_or_default()
}

#[tokio::test]
async fn a_new_tag_records_the_wait_of_each_spec_it_newly_reaches() {
    test_support::install();
    let folder = TempFolder::new();
    let name = unique_name();
    // The new tag reaches alpha and beta. Gamma is already in the counted tag.
    let (service, _fixture) = released(&folder, &name, &[ALPHA, BETA]);
    write(tips_in(&folder), &format!("{name} v1.0.0\n"));
    let logs = CapturedLogs::start();

    record_releases(&service, &tips_in(&folder), now(), UtcOffset::UTC).await;

    assert_eq!(recorded(&name, "alpha"), Some(86_400.0));
    assert_eq!(recorded(&name, "beta"), Some(259_200.0));
    assert_eq!(recorded(&name, "gamma"), None);
    assert_eq!(saved(&folder), format!("{name} v1.1.0\n"));
    let text = logs.text();
    assert!(text.contains("release phase recorded"), "{text}");
    assert!(text.contains("tag=v1.1.0"), "{text}");
    assert!(text.contains("specs=2"), "{text}");
}

#[tokio::test]
async fn a_product_with_no_counted_tag_records_nothing_and_saves_the_tag() {
    test_support::install();
    let folder = TempFolder::new();
    let name = unique_name();
    let (service, _fixture) = released(&folder, &name, &[ALPHA, BETA]);

    record_releases(&service, &tips_in(&folder), now(), UtcOffset::UTC).await;

    assert_eq!(recorded(&name, "alpha"), None);
    assert_eq!(saved(&folder), format!("{name} v1.1.0\n"));
}

#[tokio::test]
async fn a_restart_with_the_file_in_place_records_nothing_again() {
    test_support::install();
    let folder = TempFolder::new();
    let name = unique_name();
    let (service, fixture) = released(&folder, &name, &[ALPHA]);
    write(tips_in(&folder), &format!("{name} v1.0.0\n"));
    record_releases(&service, &tips_in(&folder), now(), UtcOffset::UTC).await;
    assert_eq!(recorded(&name, "alpha"), Some(86_400.0));

    // A new service value reads the same file, as after a restart.
    let again = ReleaseToolService::with_git(
        service.registry().map(PathBuf::from),
        released_git(&fixture, &[ALPHA]),
    );
    record_releases(&again, &tips_in(&folder), now(), UtcOffset::UTC).await;

    assert_eq!(recorded(&name, "alpha"), Some(86_400.0));
}

/// The git answers of `released`, for a second service on the same fixture.
fn released_git(fixture: &Fixture, printed: &[&str]) -> FakeGit {
    let unix = OffsetDateTime::parse(TAG_TIME, &Rfc3339)
        .expect("a valid time")
        .unix_timestamp()
        .to_string();
    let listing: String = printed.iter().map(|hash| format!("{hash}\n")).collect();
    fixture
        .git()
        .answer(&DESCRIBE, true, "v1.1.0\n")
        .answer(
            &[
                "for-each-ref",
                "--format=%(creatordate:unix)",
                "refs/tags/v1.1.0",
            ],
            true,
            &unix,
        )
        .answer(&["rev-list", "v1.1.0", "^v1.0.0"], true, &listing)
}

#[tokio::test]
async fn a_failed_write_records_nothing_and_a_later_read_records_once() {
    test_support::install();
    let folder = TempFolder::new();
    let name = unique_name();
    let (service, _fixture) = released(&folder, &name, &[ALPHA]);
    write(tips_in(&folder), &format!("{name} v1.0.0\n"));
    // A folder in the place of the temporary file makes the write fail.
    let blocker = folder.0.join("pod/release-tips.tmp");
    std::fs::create_dir_all(&blocker).expect("create the blocker");

    record_releases(&service, &tips_in(&folder), now(), UtcOffset::UTC).await;

    assert_eq!(recorded(&name, "alpha"), None);
    assert_eq!(saved(&folder), format!("{name} v1.0.0\n"));

    std::fs::remove_dir(&blocker).expect("remove the blocker");
    record_releases(&service, &tips_in(&folder), now(), UtcOffset::UTC).await;
    record_releases(&service, &tips_in(&folder), now(), UtcOffset::UTC).await;

    assert_eq!(recorded(&name, "alpha"), Some(86_400.0));
    assert_eq!(saved(&folder), format!("{name} v1.1.0\n"));
}

#[tokio::test]
async fn an_unreadable_line_resets_the_product_and_logs_the_reason() {
    test_support::install();
    let folder = TempFolder::new();
    let name = unique_name();
    let (service, _fixture) = released(&folder, &name, &[ALPHA]);
    // The line has a product and no tag.
    write(tips_in(&folder), &format!("{name}\n"));
    let logs = CapturedLogs::start();

    record_releases(&service, &tips_in(&folder), now(), UtcOffset::UTC).await;

    assert_eq!(recorded(&name, "alpha"), None);
    assert_eq!(saved(&folder), format!("{name} v1.1.0\n"));
    let text = logs.text();
    assert!(text.contains("release tips reset"), "{text}");
    assert!(text.contains("reason=\"unreadable\""), "{text}");
    assert!(text.contains(&name), "{text}");
}

#[tokio::test]
async fn a_counted_tag_that_git_cannot_find_resets_the_product_and_logs_the_reason() {
    test_support::install();
    let folder = TempFolder::new();
    let name = unique_name();
    let (service, _fixture) = released(&folder, &name, &[ALPHA]);
    write(tips_in(&folder), &format!("{name} v0.9.0\n"));
    let logs = CapturedLogs::start();

    record_releases(&service, &tips_in(&folder), now(), UtcOffset::UTC).await;

    assert_eq!(recorded(&name, "alpha"), None);
    assert_eq!(saved(&folder), format!("{name} v1.1.0\n"));
    let text = logs.text();
    assert!(text.contains("release tips reset"), "{text}");
    assert!(text.contains("reason=\"missing_tag\""), "{text}");
}

#[tokio::test]
async fn a_missing_or_later_merge_time_is_skipped() {
    test_support::install();
    let folder = TempFolder::new();
    let name = unique_name();
    let (service, fixture) = released(&folder, &name, &[ALPHA, BETA, GAMMA]);
    // Alpha merged after the tag time. Beta has no `merged:` line.
    merge_commit(&fixture, "alpha", Some("2026-11-05T00:00:00Z"), ALPHA);
    merge_commit(&fixture, "beta", None, BETA);
    write(tips_in(&folder), &format!("{name} v1.0.0\n"));

    record_releases(&service, &tips_in(&folder), now(), UtcOffset::UTC).await;

    assert_eq!(recorded(&name, "alpha"), None);
    assert_eq!(recorded(&name, "beta"), None);
    assert_eq!(recorded(&name, "gamma"), Some(13.0 * 86_400.0));
}

#[tokio::test]
async fn the_file_has_mode_0600_and_leaves_no_temporary_file() {
    use std::os::unix::fs::PermissionsExt;
    test_support::install();
    let folder = TempFolder::new();
    let name = unique_name();
    let (service, _fixture) = released(&folder, &name, &[ALPHA]);

    record_releases(&service, &tips_in(&folder), now(), UtcOffset::UTC).await;

    let mode = std::fs::metadata(tips_in(&folder))
        .expect("the file")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    assert!(!folder.0.join("pod/release-tips.tmp").exists());
}

#[tokio::test]
async fn a_new_tag_with_no_new_spec_logs_zero_specs() {
    test_support::install();
    let folder = TempFolder::new();
    let name = unique_name();
    let (service, _fixture) = released(&folder, &name, &[]);
    write(tips_in(&folder), &format!("{name} v1.0.0\n"));
    let logs = CapturedLogs::start();

    record_releases(&service, &tips_in(&folder), now(), UtcOffset::UTC).await;

    let text = logs.text();
    assert!(text.contains("release phase recorded"), "{text}");
    assert!(text.contains("specs=0"), "{text}");
}
