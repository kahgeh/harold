use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use time::{OffsetDateTime, UtcOffset};

use super::*;
use crate::outbound::fake::CapturedLogs;
use crate::pod_stats::{IdeaSignals, POD_FOLDER, file_time};
use crate::tools::backlog::store::{BacklogStore, TursoBacklogStore};
use crate::tools::backlog::test_support::{TempFolder, item};

const P: &str = "demo";

/// A fake handle: it keeps what the check tells pod stats.
#[derive(Default)]
struct FakeSignals {
    counts: Mutex<Vec<(String, u32)>>,
    opened: Mutex<Vec<(String, String, f64)>>,
}

impl IdeaSignals for FakeSignals {
    fn set_idea_count(&self, product: &str, count: u32) {
        self.counts.lock().unwrap().push((product.into(), count));
    }

    fn record_idea_opened(&self, product: &str, id: &str, wait_s: f64) {
        self.opened
            .lock()
            .unwrap()
            .push((product.into(), id.into(), wait_s));
    }
}

impl FakeSignals {
    fn last_count(&self) -> Option<u32> {
        self.counts.lock().unwrap().last().map(|(_, count)| *count)
    }

    fn opened(&self) -> Vec<(String, String, f64)> {
        self.opened.lock().unwrap().clone()
    }
}

struct Setup {
    store: TursoBacklogStore,
    registry: PathBuf,
    repo: PathBuf,
    // Removes the folders at the end.
    _folders: (TempFolder, TempFolder),
}

async fn setup() -> Setup {
    let data = TempFolder::new();
    let repo = TempFolder::new();
    let store = TursoBacklogStore::open(&data.0).await.unwrap();
    let registry = data.0.join("products.toml");
    std::fs::write(
        &registry,
        format!(
            "[products.{P}]\nrepo = \"{}\"\nbase = \"main\"\n",
            repo.0.display()
        ),
    )
    .unwrap();
    Setup {
        store,
        registry,
        repo: repo.0.clone(),
        _folders: (data, repo),
    }
}

fn write_spec(repo: &Path, id: &str) -> PathBuf {
    let path = repo.join(POD_FOLDER).join("specs").join(format!("{id}.md"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, format!("# {id}: Title\n")).unwrap();
    path
}

async fn run_check(setup: &Setup, signals: &FakeSignals, failed: &mut HashSet<String>) {
    check(
        &setup.store,
        &setup.registry,
        signals,
        OffsetDateTime::now_utc(),
        UtcOffset::UTC,
        failed,
    )
    .await;
}

#[tokio::test]
async fn an_item_with_no_spec_counts_in_idea_and_one_with_a_spec_does_not() {
    let setup = setup().await;
    for id in ["a", "b", "c"] {
        setup
            .store
            .put_item(P, item(id), 1_000)
            .await
            .unwrap()
            .unwrap();
    }
    write_spec(&setup.repo, "b");
    let signals = FakeSignals::default();
    run_check(&setup, &signals, &mut HashSet::new()).await;
    assert_eq!(signals.last_count(), Some(2));
}

#[tokio::test]
async fn a_product_with_no_pod_folder_counts_all_its_items() {
    let setup = setup().await;
    setup
        .store
        .put_item(P, item("a"), 1_000)
        .await
        .unwrap()
        .unwrap();
    let signals = FakeSignals::default();
    run_check(&setup, &signals, &mut HashSet::new()).await;
    assert_eq!(signals.last_count(), Some(1));
    assert!(signals.opened().is_empty());
}

#[tokio::test]
async fn a_spec_that_appears_records_idea_to_open_once_with_the_draft_time() {
    let setup = setup().await;
    setup
        .store
        .put_item(P, item("a"), 1_000)
        .await
        .unwrap()
        .unwrap();
    let spec = write_spec(&setup.repo, "a");
    let draft = file_time(&spec, UtcOffset::UTC).unwrap();
    let signals = FakeSignals::default();
    let logs = CapturedLogs::start();
    run_check(&setup, &signals, &mut HashSet::new()).await;
    run_check(&setup, &signals, &mut HashSet::new()).await;
    let opened = signals.opened();
    assert_eq!(opened.len(), 1);
    let (product, id, wait_s) = &opened[0];
    assert_eq!((product.as_str(), id.as_str()), (P, "a"));
    let expected = (draft.unix_timestamp_nanos() as f64 / 1e9) - 1.0;
    assert!((wait_s - expected).abs() < 0.01, "{wait_s} {expected}");
    let text = logs.text();
    assert!(text.contains("backlog idea opened"), "{text}");
    assert!(text.contains("spec.id=a"), "{text}");
    assert_eq!(signals.last_count(), Some(0));
}

#[tokio::test]
async fn an_item_put_after_its_spec_draft_records_nothing_and_is_not_checked_again() {
    let setup = setup().await;
    let spec = write_spec(&setup.repo, "a");
    let draft_ms = (file_time(&spec, UtcOffset::UTC)
        .unwrap()
        .unix_timestamp_nanos()
        / 1_000_000) as i64;
    setup
        .store
        .put_item(P, item("a"), draft_ms + 60_000)
        .await
        .unwrap()
        .unwrap();
    let signals = FakeSignals::default();
    run_check(&setup, &signals, &mut HashSet::new()).await;
    assert!(signals.opened().is_empty());
    let times = setup.store.idea_times(P).await.unwrap();
    assert!(times[0].opened_at_ms.is_some());
}

#[tokio::test]
async fn a_removed_item_leaves_the_idea_count() {
    let setup = setup().await;
    for id in ["a", "b"] {
        setup
            .store
            .put_item(P, item(id), 1_000)
            .await
            .unwrap()
            .unwrap();
    }
    let signals = FakeSignals::default();
    run_check(&setup, &signals, &mut HashSet::new()).await;
    assert_eq!(signals.last_count(), Some(2));
    setup
        .store
        .remove_item(P, "a", None, 2_000)
        .await
        .unwrap()
        .unwrap();
    run_check(&setup, &signals, &mut HashSet::new()).await;
    assert_eq!(signals.last_count(), Some(1));
}

#[tokio::test]
async fn an_unreadable_specs_folder_logs_once_and_records_nothing() {
    let setup = setup().await;
    setup
        .store
        .put_item(P, item("a"), 1_000)
        .await
        .unwrap()
        .unwrap();
    // A file where the specs folder should be: the folder cannot be read.
    let pod = setup.repo.join(POD_FOLDER);
    std::fs::create_dir_all(&pod).unwrap();
    std::fs::write(pod.join("specs"), "not a folder").unwrap();
    let signals = FakeSignals::default();
    let logs = CapturedLogs::start();
    let mut failed = HashSet::new();
    run_check(&setup, &signals, &mut failed).await;
    run_check(&setup, &signals, &mut failed).await;
    let text = logs.text();
    assert_eq!(
        text.matches("backlog idea check failed").count(),
        1,
        "{text}"
    );
    assert!(text.contains("reason="), "{text}");
    assert!(signals.counts.lock().unwrap().is_empty());
    assert!(signals.opened().is_empty());
    // The next read that works clears the failure, so a later one logs again.
    std::fs::remove_file(pod.join("specs")).unwrap();
    run_check(&setup, &signals, &mut failed).await;
    assert!(failed.is_empty());
    assert_eq!(signals.last_count(), Some(1));
}

#[test]
fn the_check_runs_every_10_minutes() {
    assert_eq!(PERIOD, Duration::from_secs(600));
}

#[test]
fn the_check_opens_no_file_of_the_pod_folder_itself() {
    let source = include_str!("idea_check.rs");
    let code = source.split("#[cfg(test)]").next().unwrap();
    for call in ["read_dir", "File::open", "read_to_string", "fs::read"] {
        assert!(!code.contains(call), "idea_check.rs calls {call}");
    }
}
