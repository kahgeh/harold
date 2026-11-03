use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::outbound::fake::CapturedLogs;
use crate::telemetry::{WorkCount, WorkNoted, take_noted_work};
use crate::tools::work::files::WorkFiles;
use crate::tools::work::schema::{Priority, Record, Status};
use crate::tools::work::store::WorkFile;
use crate::tools::work::test_support::{HOUR_MS, SetClock, TempFolder, noon_ms, record};

const START: &str = "2026-11-01";

/// A work file that starts on `START` with one open record, `a`.
async fn seed(folder: &TempFolder) {
    let file = WorkFile::open(&folder.file("work-2026-11-01.db"))
        .await
        .unwrap();
    file.put(record("a"), noon_ms(START))
        .await
        .unwrap()
        .unwrap();
    file.close().await.unwrap();
}

fn rotator(folder: &TempFolder, clock: &Arc<SetClock>) -> Arc<Rotator> {
    Arc::new(Rotator::new(
        WorkFiles::new(folder.0.clone()),
        Arc::clone(clock) as Arc<dyn Clock>,
    ))
}

fn file_names(folder: &TempFolder) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(&folder.0)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.ends_with(".db"))
        .collect();
    names.sort();
    names
}

fn rotations(noted: &[WorkNoted]) -> Vec<&'static str> {
    noted
        .iter()
        .filter_map(|n| match n {
            WorkNoted::Rotation(outcome) => Some(*outcome),
            _ => None,
        })
        .collect()
}

fn gauge_notes(noted: &[WorkNoted]) -> usize {
    noted
        .iter()
        .filter(|n| matches!(n, WorkNoted::Records(_)))
        .count()
}

/// Waits, on the paused clock, until the checks noted on this thread reach
/// `runs`, and returns everything noted so far.
async fn wait_for_runs(noted: &mut Vec<WorkNoted>, runs: usize) {
    for _ in 0..200 {
        noted.extend(take_noted_work());
        if gauge_notes(noted) >= runs {
            return;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!("{runs} runs not seen: {noted:?}");
}

#[tokio::test]
async fn a_failure_logs_warn_once() {
    let logs = CapturedLogs::start();
    let folder = TempFolder::new();
    seed(&folder).await;
    let clock = SetClock::at(noon_ms("2026-11-16"));
    let rotator = rotator(&folder, &clock);
    rotator.set_copy_hook(Some(Box::new(|record: Record| {
        (record.name != "a").then_some(record)
    })));
    take_noted_work();

    // The first check fails and logs warn. Three daily retries fail and log no more.
    for day in ["2026-11-16", "2026-11-17", "2026-11-18", "2026-11-19"] {
        clock.set(noon_ms(day));
        check(&rotator, clock.as_ref(), 14).await;
    }
    let text = logs.text();
    let failed: Vec<&str> = text
        .lines()
        .filter(|line| line.contains("work rotation failed"))
        .collect();
    assert_eq!(failed.len(), 1, "{text}");
    let line = failed[0];
    assert!(line.trim_start().starts_with("WARN"), "{line}");
    assert!(line.contains("file=work-2026-11-01.db"), "{line}");
    assert!(line.contains("count_mismatch"), "{line}");
    assert!(line.contains("since=2026-11-16T12:00:00Z"), "{line}");
    assert_eq!(
        text.lines()
            .filter(|l| l.trim_start().starts_with("WARN"))
            .count(),
        1
    );
    assert_eq!(file_names(&folder), ["work-2026-11-01.db"]);

    rotator.set_copy_hook(None);
    clock.set(noon_ms("2026-11-20"));
    check(&rotator, clock.as_ref(), 14).await;
    let text = logs.text();
    let done: Vec<&str> = text
        .lines()
        .filter(|line| line.contains("work rotation done"))
        .collect();
    assert_eq!(done.len(), 1, "{text}");
    let line = done[0];
    assert!(line.trim_start().starts_with("INFO"), "{line}");
    for field in [
        "open_records=1",
        "decisions=0",
        "links=0",
        "file=work-2026-11-01.db",
        "new_file=work-2026-11-20.db",
    ] {
        assert!(line.contains(field), "{field} in {line}");
    }
    assert_eq!(
        text.lines()
            .filter(|l| l.trim_start().starts_with("WARN"))
            .count(),
        1
    );
    assert_eq!(
        rotations(&take_noted_work()),
        ["failed", "failed", "failed", "failed", "done"]
    );
}

#[tokio::test]
async fn a_rotation_that_is_not_due_logs_and_counts_nothing() {
    let logs = CapturedLogs::start();
    let folder = TempFolder::new();
    seed(&folder).await;
    let clock = SetClock::at(noon_ms("2026-11-05"));
    let rotator = rotator(&folder, &clock);
    take_noted_work();
    check(&rotator, clock.as_ref(), 14).await;
    check(&rotator, clock.as_ref(), 0).await;
    assert!(!logs.text().contains("work rotation"), "{}", logs.text());
    let noted = take_noted_work();
    assert!(rotations(&noted).is_empty(), "{noted:?}");
    assert_eq!(gauge_notes(&noted), 2);
}

#[tokio::test]
async fn a_record_at_waiting_for_the_user_for_25_hours_counts_in_waiting_long() {
    let folder = TempFolder::new();
    let start = noon_ms(START);
    let clock = SetClock::at(start);
    let rotator = rotator(&folder, &clock);
    let mut waiting = record("w");
    waiting.priority = Priority::Next;
    waiting.status = Status::WaitingForTheUser;
    rotator
        .current()
        .await
        .unwrap()
        .put(waiting, start)
        .await
        .unwrap()
        .unwrap();
    let mut closed = record("c");
    closed.status = Status::WaitingForTheUser;
    let file = rotator.current().await.unwrap();
    file.put(closed, start).await.unwrap().unwrap();
    let patch = crate::tools::work::store::Patch {
        status: Some(Status::CheckedByTheUser),
        ..Default::default()
    };
    file.set("c", patch, start).await.unwrap().unwrap();
    drop(file);
    take_noted_work();

    clock.set(start + 23 * HOUR_MS);
    check(&rotator, clock.as_ref(), 0).await;
    clock.set(start + 25 * HOUR_MS);
    check(&rotator, clock.as_ref(), 0).await;

    let waiting_long: Vec<Vec<(&'static str, u64)>> = take_noted_work()
        .into_iter()
        .filter_map(|n| match n {
            WorkNoted::WaitingLong(counts) => Some(counts),
            _ => None,
        })
        .collect();
    assert_eq!(waiting_long, vec![vec![], vec![("next", 1)]]);
}

#[tokio::test]
async fn the_daily_check_notes_the_open_records_by_priority_and_status() {
    let folder = TempFolder::new();
    seed(&folder).await;
    let clock = SetClock::at(noon_ms("2026-11-02"));
    let rotator = rotator(&folder, &clock);
    take_noted_work();
    check(&rotator, clock.as_ref(), 14).await;
    let records: Vec<Vec<WorkCount>> = take_noted_work()
        .into_iter()
        .filter_map(|n| match n {
            WorkNoted::Records(counts) => Some(counts),
            _ => None,
        })
        .collect();
    assert_eq!(
        records,
        vec![vec![WorkCount {
            priority: "later",
            status: "idea",
            count: 1,
        }]]
    );
}

#[tokio::test(start_paused = true)]
async fn the_check_runs_at_start_then_each_day_and_ends_on_shutdown() {
    let folder = TempFolder::new();
    seed(&folder).await;
    let clock = SetClock::at(noon_ms("2026-11-02"));
    let rotator = rotator(&folder, &clock);
    let reads = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&reads);
    let read_days: ReadDays = Box::new(move || {
        counted.fetch_add(1, Ordering::SeqCst);
        Some(14)
    });
    let (shutdown, shutdown_rx) = tokio::sync::watch::channel(());
    take_noted_work();
    let task = tokio::spawn(run(
        rotator,
        clock.clone() as Arc<dyn Clock>,
        read_days,
        shutdown_rx,
    ));
    let mut noted = Vec::new();

    wait_for_runs(&mut noted, 1).await;
    assert_eq!(reads.load(Ordering::SeqCst), 1);
    tokio::time::advance(DAY - Duration::from_secs(1)).await;
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert_eq!(reads.load(Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_secs(1)).await;
    wait_for_runs(&mut noted, 2).await;
    assert_eq!(reads.load(Ordering::SeqCst), 2);

    shutdown.send(()).unwrap();
    tokio::time::timeout(Duration::from_millis(500), task)
        .await
        .expect("the timer ends on shutdown")
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_failed_settings_read_keeps_the_last_value() {
    let folder = TempFolder::new();
    seed(&folder).await;
    let clock = SetClock::at(noon_ms("2026-11-20"));
    let rotator = rotator(&folder, &clock);
    let answers = Mutex::new(VecDeque::from([Some(0), None, Some(14)]));
    let read_days: ReadDays = Box::new(move || answers.lock().unwrap().pop_front().flatten());
    let (shutdown, shutdown_rx) = tokio::sync::watch::channel(());
    take_noted_work();
    let task = tokio::spawn(run(
        rotator,
        clock.clone() as Arc<dyn Clock>,
        read_days,
        shutdown_rx,
    ));
    let mut noted = Vec::new();

    wait_for_runs(&mut noted, 1).await;
    assert_eq!(file_names(&folder), ["work-2026-11-01.db"]);
    tokio::time::advance(DAY).await;
    wait_for_runs(&mut noted, 2).await;
    // The read failed, so the rotation stays off, as the last value said.
    assert_eq!(file_names(&folder), ["work-2026-11-01.db"]);
    tokio::time::advance(DAY).await;
    wait_for_runs(&mut noted, 3).await;
    assert_eq!(
        file_names(&folder),
        ["work-2026-11-01.db", "work-2026-11-20.db"]
    );
    assert_eq!(rotations(&noted), ["done"]);

    shutdown.send(()).unwrap();
    tokio::time::timeout(Duration::from_millis(500), task)
        .await
        .expect("the timer ends on shutdown")
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn the_first_failed_settings_read_uses_the_default() {
    let folder = TempFolder::new();
    seed(&folder).await;
    let clock = SetClock::at(noon_ms("2026-11-20"));
    let rotator = rotator(&folder, &clock);
    let read_days: ReadDays = Box::new(|| None);
    let (shutdown, shutdown_rx) = tokio::sync::watch::channel(());
    take_noted_work();
    let task = tokio::spawn(run(
        rotator,
        clock.clone() as Arc<dyn Clock>,
        read_days,
        shutdown_rx,
    ));
    let mut noted = Vec::new();
    wait_for_runs(&mut noted, 1).await;
    // The default is 14 days, and the file is 19 days old.
    assert_eq!(rotations(&noted), ["done"]);
    drop(shutdown);
    tokio::time::timeout(Duration::from_millis(500), task)
        .await
        .expect("the timer ends when the sender is dropped")
        .unwrap();
}

#[test]
fn since_is_a_readable_utc_date_time() {
    assert_eq!(
        readable_utc(noon_ms("2026-11-16") + 345),
        "2026-11-16T12:00:00Z"
    );
}

#[test]
fn the_telemetry_lists_name_every_priority_and_open_status() {
    use crate::telemetry::{WORK_OPEN_STATUSES, WORK_PRIORITIES};
    let priorities: Vec<Priority> = WORK_PRIORITIES
        .iter()
        .map(|word| Priority::parse(word).unwrap())
        .collect();
    assert_eq!(
        priorities,
        [Priority::Focus, Priority::Next, Priority::Later]
    );
    let ordinals: Vec<u8> = WORK_OPEN_STATUSES
        .iter()
        .map(|words| Status::parse(words).unwrap())
        .inspect(|status| assert!(status.is_open()))
        .map(Status::ordinal)
        .collect();
    assert_eq!(ordinals, (0..9).collect::<Vec<u8>>());
}

#[test]
fn a_rotation_days_above_365_counts_as_unreadable() {
    assert_eq!(accept_days(Some(365)), Some(365));
    assert_eq!(accept_days(Some(0)), Some(0));
    assert_eq!(accept_days(Some(366)), None);
    assert_eq!(accept_days(None), None);
}
