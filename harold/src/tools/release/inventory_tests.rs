use std::path::PathBuf;
use std::time::Duration;

use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, UtcOffset};
use tokio::sync::watch;

use super::{PERIOD, publish, register, run};
use crate::pod_stats::{INVENTORY_ITEM_DAYS, INVENTORY_ITEMS, INVENTORY_OLDEST_HOURS, POD_FOLDER};
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

/// A service on a product `name` with two merged specs, 2 and 4 days old at `NOW`. The
/// fixture owns the folders, so the caller keeps it until the end of the test.
fn two_merged(folder: &TempFolder, name: &str) -> (ReleaseToolService<FakeGit>, Fixture) {
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

    let task = tokio::spawn(run(service, PERIOD, receiver));
    let_task_run().await;
    let at_start = super::PUBLISHES.with(|count| count.get());
    tokio::time::advance(PERIOD).await;
    let_task_run().await;
    let after_one_period = super::PUBLISHES.with(|count| count.get());
    tokio::time::advance(PERIOD).await;
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
    let task = tokio::spawn(run(service, PERIOD, receiver));
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
