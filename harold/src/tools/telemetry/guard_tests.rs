use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};

use super::*;
use crate::tools::telemetry::test_support::{FakeChief, FakeFolders, FakeProducts, TempFolder};

struct FakeClock(AtomicI64);

impl Clock for FakeClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

struct Setup {
    tmp: TempFolder,
    folders: Arc<FakeFolders>,
    clock: Arc<FakeClock>,
}

impl Setup {
    fn new() -> Self {
        let tmp = TempFolder::new();
        for sub in ["harold", "turso"] {
            std::fs::create_dir_all(tmp.0.join(sub)).unwrap();
        }
        Self {
            tmp,
            folders: Arc::new(FakeFolders::default()),
            clock: Arc::new(FakeClock(AtomicI64::new(1_000_000))),
        }
    }

    fn path(&self, sub: &str) -> PathBuf {
        self.tmp.0.join(sub)
    }

    fn pane(&self, id: &str, folder: impl AsRef<Path>) {
        self.folders
            .0
            .lock()
            .unwrap()
            .insert(id.into(), folder.as_ref().to_path_buf());
    }

    fn guard_with(&self, registry: Result<Vec<(String, PathBuf)>, &'static str>) -> Guard {
        Guard::new(
            self.folders.clone(),
            Arc::new(FakeProducts(registry)),
            Arc::new(FakeChief(Some("%2".into()))),
            self.clock.clone(),
        )
    }

    fn guard(&self) -> Guard {
        self.guard_with(Ok(vec![
            ("harold".into(), self.path("harold")),
            ("turso".into(), self.path("turso")),
        ]))
    }
}

#[test]
fn the_chief_writes_chief_and_any_product() {
    let g = Setup::new().guard();
    for prefix in ["chief", "harold", "turso"] {
        assert_eq!(g.check_prefix("%2", prefix), Ok(()), "{prefix}");
    }
}

#[test]
fn a_pane_in_a_product_folder_writes_that_product_only() {
    let s = Setup::new();
    s.pane("%40", s.path("harold"));
    let g = s.guard();
    assert_eq!(g.check_prefix("%40", "harold"), Ok(()));
    let refused = g.check_prefix("%40", "turso").unwrap_err();
    assert_eq!(refused.reason(), "wrong_prefix");
    assert_eq!(
        refused.text(),
        "turso. is for panes in turso's folder and the chief"
    );
}

#[test]
fn a_pane_in_another_folder_gets_the_exact_wrong_prefix_text() {
    let s = Setup::new();
    s.pane("%51", s.path("turso"));
    let g = s.guard();
    let harold = g.check_prefix("%51", "harold").unwrap_err();
    assert_eq!(
        harold.text(),
        "harold. is for panes in harold's folder and the chief"
    );
    let chief = g.check_prefix("%51", "chief").unwrap_err();
    assert_eq!(chief.reason(), "wrong_prefix");
    assert_eq!(chief.text(), "chief. is for the chief's pane");
}

#[test]
fn a_pane_with_no_known_folder_is_wrong_prefix() {
    let s = Setup::new();
    let refused = s.guard().check_prefix("%99", "harold").unwrap_err();
    assert_eq!(refused.reason(), "wrong_prefix");
}

#[test]
fn an_unknown_prefix_is_refused_with_its_text() {
    let refused = Setup::new().guard().check_prefix("%2", "web").unwrap_err();
    assert_eq!(refused.reason(), "unknown_prefix");
    assert_eq!(
        refused.text(),
        "unknown prefix web.: use chief. or a product name"
    );
}

#[test]
fn an_empty_pane_is_no_pane() {
    let g = Setup::new().guard();
    assert_eq!(g.check_prefix("", "harold"), Err(Refused::NoPane));
    assert_eq!(g.check_prefix("", "chief"), Err(Refused::NoPane));
}

#[test]
fn an_unreadable_registry_is_a_refusal_not_a_panic() {
    let s = Setup::new();
    s.pane("%40", s.path("harold"));
    let g = s.guard_with(Err("broken"));
    let refused = g.check_prefix("%40", "harold").unwrap_err();
    assert_eq!(refused.reason(), "invalid");
    assert_eq!(g.check_prefix("%2", "chief"), Ok(()));
    assert!(g.registry_prefixes().is_empty());
}

#[test]
fn the_registry_prefixes_are_the_product_names() {
    assert_eq!(
        Setup::new().guard().registry_prefixes(),
        ["harold", "turso"]
    );
}

#[test]
fn the_61st_write_in_a_minute_is_rate_limited() {
    let s = Setup::new();
    let g = s.guard();
    for _ in 0..60 {
        assert_eq!(g.take_write("harold"), Ok(()));
    }
    let refused = g.take_write("harold").unwrap_err();
    assert_eq!(refused.reason(), "rate_limited");
    assert_eq!(refused.text(), "harold. wrote 60 times this minute: wait");
}

#[test]
fn the_minute_after_it_passes_writes_again() {
    let s = Setup::new();
    let g = s.guard();
    for _ in 0..60 {
        g.take_write("harold").unwrap();
    }
    s.clock.0.fetch_add(59_999, Ordering::SeqCst);
    assert!(g.take_write("harold").is_err());
    s.clock.0.fetch_add(1, Ordering::SeqCst);
    assert_eq!(g.take_write("harold"), Ok(()));
}

#[test]
fn the_window_slides() {
    let s = Setup::new();
    let g = s.guard();
    for _ in 0..30 {
        g.take_write("harold").unwrap();
    }
    s.clock.0.fetch_add(30_000, Ordering::SeqCst);
    for _ in 0..30 {
        g.take_write("harold").unwrap();
    }
    assert!(g.take_write("harold").is_err());
    s.clock.0.fetch_add(30_000, Ordering::SeqCst);
    for _ in 0..30 {
        assert_eq!(g.take_write("harold"), Ok(()));
    }
    assert!(g.take_write("harold").is_err());
}

#[test]
fn the_limit_is_for_each_prefix() {
    let g = Setup::new().guard();
    for _ in 0..60 {
        g.take_write("harold").unwrap();
    }
    assert!(g.take_write("harold").is_err());
    assert_eq!(g.take_write("chief"), Ok(()));
    assert_eq!(g.take_write("turso"), Ok(()));
}

#[test]
fn the_system_clock_gives_a_recent_time() {
    assert!(SystemClock.now_ms() > 1_700_000_000_000);
}
