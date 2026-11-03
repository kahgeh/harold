use std::collections::BTreeMap;
use std::sync::atomic::{AtomicI64, Ordering};

use super::*;
use crate::tools::work::schema::Status;
use crate::tools::work::store::{Patch, Refusal};
use crate::tools::work::test_support::{TempFolder, decision, record};

/// A clock that moves only when a test sets it.
struct FakeClock(AtomicI64);

impl FakeClock {
    fn at(day: &str) -> Arc<Self> {
        Arc::new(Self(AtomicI64::new(ms_of(day))))
    }

    fn set(&self, day: &str) {
        self.0.store(ms_of(day), Ordering::SeqCst);
    }
}

impl Clock for FakeClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// Noon UTC of `YYYY-MM-DD`, in milliseconds.
fn ms_of(day: &str) -> i64 {
    let date = date_of(day).unwrap();
    date.with_hms(12, 0, 0)
        .unwrap()
        .assume_utc()
        .unix_timestamp()
        * 1000
}

fn name_of(day: &str) -> String {
    format!("work-{day}.db")
}

const START: &str = "2026-11-01";

/// A work file that starts on `START`: `a` is open with two decisions and one
/// link, `b` is closed with one decision and one link.
async fn seed(folder: &TempFolder) {
    let file = WorkFile::open(&folder.file(&name_of(START))).await.unwrap();
    let t = ms_of(START);
    let mut a = record("a");
    a.decisions = vec![decision("one"), decision("two")];
    a.links = vec!["note:a".into()];
    file.put(a, t).await.unwrap().unwrap();
    let mut b = record("b");
    b.decisions = vec![decision("closed")];
    b.links = vec!["note:b".into()];
    file.put(b, t).await.unwrap().unwrap();
    let patch = Patch {
        status: Some(Status::CheckedByTheUser),
        ..Patch::default()
    };
    file.set("b", patch, t + 1).await.unwrap().unwrap();
}

fn rotator(folder: &TempFolder, clock: &Arc<FakeClock>) -> Rotator {
    Rotator::new(WorkFiles::new(folder.0.clone()), clock.clone())
}

fn drop_a() -> Option<CopyHook> {
    Some(Box::new(|record: Record| {
        (record.name != "a").then_some(record)
    }))
}

/// Every file name in the folder with its bytes.
fn snapshot(folder: &TempFolder) -> BTreeMap<String, Vec<u8>> {
    std::fs::read_dir(&folder.0)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            let bytes = std::fs::read(entry.path()).unwrap_or_default();
            (entry.file_name().into_string().unwrap(), bytes)
        })
        .collect()
}

fn names(folder: &TempFolder) -> Vec<String> {
    snapshot(folder).into_keys().collect()
}

async fn read(folder: &TempFolder, day: &str) -> WorkFile {
    WorkFile::open_read_only(&folder.file(&name_of(day)))
        .await
        .unwrap()
}

#[tokio::test]
async fn rotates_at_14_days_and_moves_open_only() {
    let folder = TempFolder::new();
    seed(&folder).await;
    let clock = FakeClock::at("2026-11-14");
    let rotator = rotator(&folder, &clock);
    assert_eq!(rotator.tick(14).await, Outcome::NotDue);
    clock.set("2026-11-15");
    let outcome = rotator.tick(14).await;
    assert_eq!(
        outcome,
        Outcome::Done {
            file: name_of(START),
            new_file: name_of("2026-11-15"),
            counts: Counts {
                open_records: 1,
                decisions: 2,
                links: 1,
                moves: 1,
            },
        }
    );
    let files = WorkFiles::new(folder.0.clone());
    assert_eq!(files.current_name(), Some(name_of("2026-11-15")));
    let new = read(&folder, "2026-11-15").await;
    let a = new.get("a").await.unwrap().unwrap();
    assert_eq!(a.decisions, vec![decision("one"), decision("two")]);
    assert_eq!(a.links, vec!["note:a".to_string()]);
    assert_eq!(new.get("b").await.unwrap(), None);
    assert!(!names(&folder).iter().any(|n| n.contains(".tmp")));
}

#[tokio::test]
async fn closed_records_stay_in_old_file() {
    let folder = TempFolder::new();
    seed(&folder).await;
    let clock = FakeClock::at("2026-11-15");
    let rotator = rotator(&folder, &clock);
    assert!(matches!(rotator.tick(14).await, Outcome::Done { .. }));
    let old = read(&folder, START).await;
    let b = old.get("b").await.unwrap().unwrap();
    assert_eq!(b.status, Status::CheckedByTheUser);
    assert_eq!(b.decisions, vec![decision("closed")]);
    assert!(old.get("a").await.unwrap().is_some());
}

#[tokio::test]
async fn status_since_survives_rotation() {
    let folder = TempFolder::new();
    seed(&folder).await;
    let changed_at = ms_of("2026-11-05");
    {
        let file = WorkFile::open(&folder.file(&name_of(START))).await.unwrap();
        let patch = Patch {
            status: Some(Status::Building),
            ..Patch::default()
        };
        file.set("a", patch, changed_at).await.unwrap().unwrap();
    }
    let clock = FakeClock::at("2026-11-15");
    let rotator = rotator(&folder, &clock);
    assert!(matches!(rotator.tick(14).await, Outcome::Done { .. }));
    let a = read(&folder, "2026-11-15")
        .await
        .get("a")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(a.status, Status::Building);
    assert_eq!(a.status_since_ms, changed_at);
}

#[tokio::test]
async fn created_in_and_created_at_survive() {
    let folder = TempFolder::new();
    seed(&folder).await;
    let clock = FakeClock::at("2026-11-15");
    let rotator = rotator(&folder, &clock);
    assert!(matches!(rotator.tick(14).await, Outcome::Done { .. }));
    let a = read(&folder, "2026-11-15")
        .await
        .get("a")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(a.name, "a");
    assert_eq!(a.created_in, name_of(START));
    assert_eq!(a.created_at_ms, ms_of(START));
}

#[tokio::test]
async fn count_mismatch_keeps_old_file_current() {
    let folder = TempFolder::new();
    seed(&folder).await;
    let before = snapshot(&folder);
    let clock = FakeClock::at("2026-11-15");
    let rotator = rotator(&folder, &clock);
    rotator.set_copy_hook(drop_a());
    assert_eq!(
        rotator.tick(14).await,
        Outcome::Failed {
            file: name_of(START),
            reason: FailReason::CountMismatch,
            first: true,
            since_ms: ms_of("2026-11-15"),
        }
    );
    assert_eq!(FailReason::CountMismatch.as_str(), "count_mismatch");
    assert_eq!(FailReason::WriteFailed.as_str(), "write_failed");
    assert_eq!(FailReason::RenameFailed.as_str(), "rename_failed");
    assert_eq!(snapshot(&folder), before);
    assert_eq!(
        WorkFiles::new(folder.0.clone()).current_name(),
        Some(name_of(START))
    );
    let current = rotator.current().await.unwrap();
    assert_eq!(current.file_name(), name_of(START));
}

#[tokio::test]
async fn failure_is_first_once_then_not_first_for_three_retries() {
    let folder = TempFolder::new();
    seed(&folder).await;
    let clock = FakeClock::at("2026-11-15");
    let rotator = rotator(&folder, &clock);
    rotator.set_copy_hook(drop_a());
    let since_ms = ms_of("2026-11-15");
    assert!(matches!(
        rotator.tick(14).await,
        Outcome::Failed { first: true, .. }
    ));
    for day in ["2026-11-16", "2026-11-17", "2026-11-18"] {
        clock.set(day);
        assert_eq!(
            rotator.tick(14).await,
            Outcome::Failed {
                file: name_of(START),
                reason: FailReason::CountMismatch,
                first: false,
                since_ms,
            }
        );
    }
}

#[tokio::test]
async fn a_working_retry_returns_done_and_clears_failure() {
    let folder = TempFolder::new();
    seed(&folder).await;
    let clock = FakeClock::at("2026-11-15");
    let rotator = rotator(&folder, &clock);
    rotator.set_copy_hook(drop_a());
    assert!(matches!(rotator.tick(14).await, Outcome::Failed { .. }));
    rotator.set_copy_hook(None);
    clock.set("2026-11-16");
    assert_eq!(
        rotator.tick(14).await,
        Outcome::Done {
            file: name_of(START),
            new_file: name_of("2026-11-16"),
            counts: Counts {
                open_records: 1,
                decisions: 2,
                links: 1,
                moves: 1,
            },
        }
    );
    assert_eq!(*rotator.failure.lock().unwrap(), None);
}

#[tokio::test]
async fn dry_run_changes_nothing() {
    let folder = TempFolder::new();
    seed(&folder).await;
    let before = snapshot(&folder);
    let clock = FakeClock::at("2026-11-15");
    let rotator = rotator(&folder, &clock);
    let dry = rotator.dry_run().await.unwrap();
    assert_eq!(
        dry,
        DryRun {
            counts: Counts {
                open_records: 1,
                decisions: 2,
                links: 1,
                moves: 1,
            },
            records: vec!["a".into()],
            target: name_of("2026-11-15"),
        }
    );
    assert_eq!(snapshot(&folder), before);
}

#[tokio::test]
async fn rotation_days_0_never_rotates() {
    let folder = TempFolder::new();
    seed(&folder).await;
    let before = snapshot(&folder);
    let clock = FakeClock::at("2028-01-01");
    let rotator = rotator(&folder, &clock);
    assert_eq!(rotator.tick(0).await, Outcome::Off);
    assert_eq!(snapshot(&folder), before);
}

#[tokio::test]
async fn smaller_value_rotates_older_file_at_next_tick() {
    let folder = TempFolder::new();
    seed(&folder).await;
    let clock = FakeClock::at("2026-11-11");
    let rotator = rotator(&folder, &clock);
    assert_eq!(rotator.tick(14).await, Outcome::NotDue);
    assert!(matches!(rotator.tick(7).await, Outcome::Done { .. }));
}

#[tokio::test]
async fn larger_value_waits() {
    let folder = TempFolder::new();
    seed(&folder).await;
    let clock = FakeClock::at("2026-11-16");
    let rotator = rotator(&folder, &clock);
    assert_eq!(rotator.tick(30).await, Outcome::NotDue);
    clock.set("2026-11-30");
    assert_eq!(rotator.tick(30).await, Outcome::NotDue);
    clock.set("2026-12-01");
    assert!(matches!(rotator.tick(30).await, Outcome::Done { .. }));
}

#[tokio::test]
async fn old_file_refuses_write_after_rotation() {
    let folder = TempFolder::new();
    seed(&folder).await;
    let clock = FakeClock::at("2026-11-15");
    let rotator = rotator(&folder, &clock);
    {
        let current = rotator.current().await.unwrap();
        assert_eq!(current.file_name(), name_of(START));
        current.put(record("c"), 1).await.unwrap().unwrap();
    }
    assert!(matches!(rotator.tick(14).await, Outcome::Done { .. }));
    let old = rotator.read_file(&name_of(START)).await.unwrap();
    assert_eq!(
        old.put(record("d"), 2).await.unwrap(),
        Err(Refusal::ReadOnly)
    );
    let current = rotator.current().await.unwrap();
    assert_eq!(current.file_name(), name_of("2026-11-15"));
    current.put(record("d"), 2).await.unwrap().unwrap();
    assert!(current.get("c").await.unwrap().is_some());
    drop(current);
    drop(old);
    drop(rotator);

    // After a restart, a new Rotator writes only to the newest file, and the
    // old file name opens read-only.
    let restarted = Rotator::new(WorkFiles::new(folder.0.clone()), clock.clone());
    let current = restarted.current().await.unwrap();
    assert_eq!(current.file_name(), name_of("2026-11-15"));
    drop(current);
    let old = restarted.read_file(&name_of(START)).await.unwrap();
    assert_eq!(
        old.put(record("e"), 3).await.unwrap(),
        Err(Refusal::ReadOnly)
    );
    assert_eq!(old.get("e").await.unwrap(), None);
}

#[tokio::test]
async fn two_ticks_at_the_same_time_give_one_done_and_one_not_due() {
    let folder = TempFolder::new();
    seed(&folder).await;
    let clock = FakeClock::at("2026-11-15");
    let rotator = rotator(&folder, &clock);
    let (one, two) = tokio::join!(rotator.tick(14), rotator.tick(14));
    let done = [&one, &two]
        .iter()
        .filter(|o| matches!(o, Outcome::Done { .. }))
        .count();
    assert_eq!(done, 1, "{one:?} {two:?}");
    assert!([&one, &two].contains(&&Outcome::NotDue), "{one:?} {two:?}");
    assert_eq!(*rotator.failure.lock().unwrap(), None);
}

#[tokio::test]
async fn a_rotation_from_a_stale_current_name_is_not_due() {
    let folder = TempFolder::new();
    seed(&folder).await;
    let clock = FakeClock::at("2026-11-15");
    let rotator = rotator(&folder, &clock);
    assert!(matches!(rotator.tick(14).await, Outcome::Done { .. }));
    clock.set("2026-11-16");
    // A tick that read the old name before the first rotation ended.
    assert_eq!(
        rotator.rotate(&name_of(START), "2026-11-16").await,
        Outcome::NotDue
    );
    assert_eq!(*rotator.failure.lock().unwrap(), None);
    assert_eq!(
        WorkFiles::new(folder.0.clone()).current_name(),
        Some(name_of("2026-11-15"))
    );
}

#[tokio::test]
async fn a_tmp_path_that_cannot_be_written_is_write_failed() {
    let folder = TempFolder::new();
    seed(&folder).await;
    // A folder in the place of the `.tmp` file: the remove and the open fail.
    std::fs::create_dir(folder.file("work-2026-11-15.db.tmp")).unwrap();
    let clock = FakeClock::at("2026-11-15");
    let rotator = rotator(&folder, &clock);
    assert_eq!(
        rotator.tick(14).await,
        Outcome::Failed {
            file: name_of(START),
            reason: FailReason::WriteFailed,
            first: true,
            since_ms: ms_of("2026-11-15"),
        }
    );
    assert_eq!(
        WorkFiles::new(folder.0.clone()).current_name(),
        Some(name_of(START))
    );
}

#[tokio::test]
async fn a_target_that_exists_is_rename_failed() {
    let folder = TempFolder::new();
    seed(&folder).await;
    // A folder with the target name. It is not a work file, so the old file
    // stays current, but the rename must not replace it.
    std::fs::create_dir(folder.file(&name_of("2026-11-15"))).unwrap();
    let clock = FakeClock::at("2026-11-15");
    let rotator = rotator(&folder, &clock);
    assert_eq!(
        rotator.tick(14).await,
        Outcome::Failed {
            file: name_of(START),
            reason: FailReason::RenameFailed,
            first: true,
            since_ms: ms_of("2026-11-15"),
        }
    );
    assert!(folder.file(&name_of("2026-11-15")).is_dir());
    assert!(!names(&folder).iter().any(|n| n.contains(".tmp")));
    assert_eq!(
        WorkFiles::new(folder.0.clone()).current_name(),
        Some(name_of(START))
    );
}

#[tokio::test]
async fn rotation_days_0_clears_the_failure() {
    let folder = TempFolder::new();
    seed(&folder).await;
    let clock = FakeClock::at("2026-11-15");
    let rotator = rotator(&folder, &clock);
    rotator.set_copy_hook(drop_a());
    assert!(matches!(rotator.tick(14).await, Outcome::Failed { .. }));
    assert_eq!(rotator.tick(0).await, Outcome::Off);
    assert_eq!(*rotator.failure.lock().unwrap(), None);
    clock.set("2026-11-16");
    assert!(matches!(
        rotator.tick(14).await,
        Outcome::Failed { first: true, .. }
    ));
}

#[tokio::test]
async fn same_day_target_is_refused_and_old_stays() {
    let folder = TempFolder::new();
    seed(&folder).await;
    let before = snapshot(&folder);
    let clock = FakeClock::at(START);
    let rotator = rotator(&folder, &clock);
    assert_eq!(rotator.tick(1).await, Outcome::NotDue);
    assert_eq!(
        rotator.rotate(&name_of(START), START).await,
        Outcome::NotDue
    );
    assert_eq!(
        rotator.rotate(&name_of(START), "2026-10-30").await,
        Outcome::NotDue
    );
    assert_eq!(snapshot(&folder), before);
}

#[tokio::test]
async fn stale_tmp_file_is_overwritten() {
    let folder = TempFolder::new();
    seed(&folder).await;
    let tmp = folder.file("work-2026-11-15.db.tmp");
    std::fs::write(&tmp, b"not a database").unwrap();
    std::fs::write(folder.file("work-2026-11-15.db.tmp-wal"), b"junk").unwrap();
    let clock = FakeClock::at("2026-11-15");
    let rotator = rotator(&folder, &clock);
    assert!(matches!(rotator.tick(14).await, Outcome::Done { .. }));
    assert!(!tmp.exists());
    assert!(!names(&folder).iter().any(|n| n.contains(".tmp")));
    let new = read(&folder, "2026-11-15").await;
    assert!(new.get("a").await.unwrap().is_some());
}

#[tokio::test]
async fn empty_current_file_rotates_with_zero_counts() {
    let folder = TempFolder::new();
    drop(WorkFile::open(&folder.file(&name_of(START))).await.unwrap());
    let clock = FakeClock::at("2026-11-15");
    let rotator = rotator(&folder, &clock);
    assert_eq!(
        rotator.tick(14).await,
        Outcome::Done {
            file: name_of(START),
            new_file: name_of("2026-11-15"),
            counts: Counts::default(),
        }
    );
    let new = read(&folder, "2026-11-15").await;
    assert_eq!(new.counts().await.unwrap(), Counts::default());
}
