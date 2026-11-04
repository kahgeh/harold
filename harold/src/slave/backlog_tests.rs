use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use super::*;

fn temp_db(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("harold-backlog-{name}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("report-backlog.db")
}

fn t0() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000)
}

#[tokio::test]
async fn numbers_rise_and_survive_reopen() {
    let path = temp_db("reopen");
    let backlog = ReportBacklog::open(&path).await.unwrap();
    assert_eq!(
        backlog
            .append(ReportKind::Turn, vec![1], t0())
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        backlog
            .append(ReportKind::Lock, vec![2], t0())
            .await
            .unwrap(),
        2
    );
    assert_eq!(backlog.last_number(), 2);
    assert_eq!(backlog.len(), 2);
    backlog.ack(2).await.unwrap();
    assert_eq!(backlog.len(), 0);
    drop(backlog);
    let backlog = ReportBacklog::open(&path).await.unwrap();
    assert_eq!(backlog.last_number(), 2);
    assert_eq!(
        backlog
            .append(ReportKind::Turn, vec![3], t0())
            .await
            .unwrap(),
        3
    );
}

#[tokio::test]
async fn ack_deletes_covered_reports() {
    let backlog = ReportBacklog::open(&temp_db("ack")).await.unwrap();
    for i in 0..4u8 {
        backlog
            .append(ReportKind::Turn, vec![i], t0())
            .await
            .unwrap();
    }
    backlog.ack(2).await.unwrap();
    assert_eq!(backlog.len(), 2);
    let kept = backlog.after(0).await.unwrap();
    assert_eq!(kept, vec![(3, vec![2]), (4, vec![3])]);
    assert_eq!(backlog.after(3).await.unwrap(), vec![(4, vec![3])]);
}

#[tokio::test]
async fn trim_drops_reports_older_than_24h() {
    let backlog = ReportBacklog::open(&temp_db("age")).await.unwrap();
    backlog
        .append(ReportKind::Turn, vec![1], t0())
        .await
        .unwrap();
    let later = t0() + Duration::from_secs(23 * 3600);
    backlog
        .append(ReportKind::Turn, vec![2], later)
        .await
        .unwrap();
    let now = t0() + Duration::from_secs(25 * 3600);
    let dropped = backlog.trim(now).await.unwrap();
    assert_eq!(
        dropped,
        Dropped {
            too_old: 1,
            too_many: 0
        }
    );
    assert_eq!(backlog.after(0).await.unwrap(), vec![(2, vec![2])]);
    assert_eq!(backlog.len(), 1);
}

#[tokio::test]
async fn trim_keeps_at_most_1000_and_counts_dropped() {
    let backlog = ReportBacklog::open(&temp_db("many")).await.unwrap();
    for i in 0..1005u32 {
        backlog
            .append(ReportKind::Turn, i.to_le_bytes().to_vec(), t0())
            .await
            .unwrap();
    }
    let dropped = backlog.trim(t0()).await.unwrap();
    assert_eq!(
        dropped,
        Dropped {
            too_old: 0,
            too_many: 5
        }
    );
    assert_eq!(backlog.len(), 1000);
    assert_eq!(backlog.after(0).await.unwrap()[0].0, 6);
    assert_eq!(backlog.last_number(), 1005);
}

#[tokio::test]
async fn rebase_numbers_above_welcome() {
    let backlog = ReportBacklog::open(&temp_db("rebase")).await.unwrap();
    backlog.rebase(50);
    assert_eq!(
        backlog
            .append(ReportKind::Turn, vec![1], t0())
            .await
            .unwrap(),
        51
    );
    backlog.rebase(10);
    assert_eq!(backlog.last_number(), 51);
}

#[tokio::test]
async fn rebase_between_append_steps_never_lowers_the_number() {
    for _ in 0..20 {
        let backlog = std::sync::Arc::new(ReportBacklog::open(&temp_db("race")).await.unwrap());
        for _ in 0..5 {
            backlog
                .append(ReportKind::Turn, vec![0], t0())
                .await
                .unwrap();
        }
        let appending = {
            let backlog = backlog.clone();
            tokio::spawn(async move {
                backlog
                    .append(ReportKind::Turn, vec![1], t0())
                    .await
                    .unwrap()
            })
        };
        backlog.rebase(50);
        appending.await.unwrap();
        assert!(backlog.last_number() >= 50);
        assert!(
            backlog
                .append(ReportKind::Turn, vec![2], t0())
                .await
                .unwrap()
                > 50
        );
    }
}

#[tokio::test]
async fn a_report_exactly_24h_old_is_kept() {
    let backlog = ReportBacklog::open(&temp_db("edge-age")).await.unwrap();
    backlog
        .append(ReportKind::Turn, vec![1], t0())
        .await
        .unwrap();
    let dropped = backlog.trim(t0() + MAX_AGE).await.unwrap();
    assert_eq!(dropped, Dropped::default());
    assert_eq!(backlog.len(), 1);
}

#[tokio::test]
async fn exactly_1000_reports_are_kept() {
    let backlog = ReportBacklog::open(&temp_db("edge-count")).await.unwrap();
    for _ in 0..1000 {
        backlog
            .append(ReportKind::Turn, vec![0], t0())
            .await
            .unwrap();
    }
    assert_eq!(backlog.trim(t0()).await.unwrap(), Dropped::default());
    assert_eq!(backlog.len(), 1000);
}

#[tokio::test]
async fn rebase_then_reopen_keeps_numbers_above_the_base() {
    let path = temp_db("rebase-reopen");
    let backlog = ReportBacklog::open(&path).await.unwrap();
    backlog.rebase(50);
    assert_eq!(
        backlog
            .append(ReportKind::Turn, vec![1], t0())
            .await
            .unwrap(),
        51
    );
    backlog.ack(51).await.unwrap();
    drop(backlog);
    let backlog = ReportBacklog::open(&path).await.unwrap();
    assert_eq!(backlog.last_number(), 51);
}

#[tokio::test]
async fn trim_counts_too_old_and_too_many_together() {
    let backlog = ReportBacklog::open(&temp_db("both")).await.unwrap();
    for _ in 0..3 {
        backlog
            .append(ReportKind::Turn, vec![0], t0())
            .await
            .unwrap();
    }
    let fresh = t0() + Duration::from_secs(30 * 3600);
    for _ in 0..1002 {
        backlog
            .append(ReportKind::Turn, vec![0], fresh)
            .await
            .unwrap();
    }
    let dropped = backlog.trim(fresh).await.unwrap();
    assert_eq!(
        dropped,
        Dropped {
            too_old: 3,
            too_many: 2
        }
    );
    assert_eq!(backlog.len(), 1000);
}

/// Opens `path`, appends `old` rows, and closes it: they are old rows for the next open.
async fn with_old_rows(path: &std::path::Path, old: u8) {
    let backlog = ReportBacklog::open(path).await.unwrap();
    for payload in 1..=old {
        backlog
            .append(ReportKind::Turn, vec![payload], t0())
            .await
            .unwrap();
    }
    backlog.welcome_first(0).await.unwrap();
}

async fn rows(backlog: &ReportBacklog) -> Vec<(u64, Vec<u8>)> {
    backlog.after(0).await.unwrap()
}

#[tokio::test]
async fn welcome_first_numbers_new_rows_above_a_master_that_is_ahead() {
    let path = temp_db("wf-ahead");
    with_old_rows(&path, 2).await;
    let backlog = ReportBacklog::open(&path).await.unwrap();
    for payload in [30u8, 40] {
        backlog
            .append(ReportKind::Lock, vec![payload], t0())
            .await
            .unwrap();
    }
    assert_eq!(backlog.last_number(), 4);
    backlog.welcome_first(5).await.unwrap();
    // Old rows 1 and 2 go (the master has them); the new rows keep their order above 5.
    assert_eq!(rows(&backlog).await, vec![(6, vec![30]), (7, vec![40])]);
    assert_eq!(backlog.last_number(), 7);
    // After the first Welcome a row is not provisional: it only takes the next number.
    assert_eq!(
        backlog
            .append(ReportKind::Turn, vec![9], t0())
            .await
            .unwrap(),
        8
    );
    backlog.ack(7).await.unwrap();
    assert_eq!(rows(&backlog).await, vec![(8, vec![9])]);
}

#[tokio::test]
async fn a_crash_before_the_first_welcome_keeps_the_new_rows() {
    let path = temp_db("wf-crash");
    with_old_rows(&path, 2).await;
    {
        let backlog = ReportBacklog::open(&path).await.unwrap();
        backlog
            .append(ReportKind::Turn, vec![30], t0())
            .await
            .unwrap();
        // The process ends here, before any Welcome.
    }
    // The restarted process opens a backlog whose last number is 3. Row 3 is still provisional.
    let backlog = ReportBacklog::open(&path).await.unwrap();
    assert_eq!(backlog.last_number(), 3);
    backlog.welcome_first(5).await.unwrap();
    assert_eq!(rows(&backlog).await, vec![(6, vec![30])]);
}

#[tokio::test]
async fn a_wiped_backlog_with_a_crash_and_welcome_7_numbers_the_row_8() {
    let path = temp_db("wf-wiped");
    {
        let backlog = ReportBacklog::open(&path).await.unwrap();
        backlog
            .append(ReportKind::Turn, vec![1], t0())
            .await
            .unwrap();
    }
    let backlog = ReportBacklog::open(&path).await.unwrap();
    backlog.welcome_first(7).await.unwrap();
    assert_eq!(rows(&backlog).await, vec![(8, vec![1])]);
}

#[tokio::test]
async fn welcome_first_when_welcome_is_the_highest_old_number() {
    let path = temp_db("wf-equal");
    with_old_rows(&path, 2).await;
    let backlog = ReportBacklog::open(&path).await.unwrap();
    backlog
        .append(ReportKind::Turn, vec![30], t0())
        .await
        .unwrap();
    backlog.welcome_first(2).await.unwrap();
    assert_eq!(rows(&backlog).await, vec![(3, vec![30])]);
}

#[tokio::test]
async fn welcome_first_when_the_master_is_behind_reuses_no_number() {
    let path = temp_db("wf-behind");
    with_old_rows(&path, 4).await;
    let backlog = ReportBacklog::open(&path).await.unwrap();
    backlog
        .append(ReportKind::Turn, vec![50], t0())
        .await
        .unwrap();
    backlog.welcome_first(2).await.unwrap();
    // Rows 3 and 4 are old rows the master lacks. The new row stays above them.
    assert_eq!(
        rows(&backlog).await,
        vec![(3, vec![3]), (4, vec![4]), (5, vec![50])]
    );
}

#[tokio::test]
async fn a_failed_welcome_first_changes_nothing_and_can_be_tried_again() {
    let path = temp_db("wf-fail");
    with_old_rows(&path, 2).await;
    let backlog = ReportBacklog::open(&path).await.unwrap();
    backlog
        .append(ReportKind::Turn, vec![30], t0())
        .await
        .unwrap();
    backlog.fail_welcome_first_once();
    assert!(backlog.welcome_first(5).await.is_err());
    assert_eq!(
        rows(&backlog).await,
        vec![(1, vec![1]), (2, vec![2]), (3, vec![30])]
    );
    backlog.welcome_first(5).await.unwrap();
    assert_eq!(rows(&backlog).await, vec![(6, vec![30])]);
}

#[tokio::test]
async fn a_backlog_file_without_the_provisional_column_is_migrated() {
    let path = temp_db("migrate");
    {
        let db = turso::Builder::new_local(&path.to_string_lossy())
            .build()
            .await
            .unwrap();
        let conn = db.connect().unwrap();
        conn.execute(
            "CREATE TABLE reports (number INTEGER PRIMARY KEY, kind TEXT NOT NULL,
             payload BLOB NOT NULL, at_ms INTEGER NOT NULL)",
            (),
        )
        .await
        .unwrap();
        conn.execute(
            "INSERT INTO reports (number, kind, payload, at_ms) VALUES (1, 'turn', x'07', 0)",
            (),
        )
        .await
        .unwrap();
    }
    let backlog = ReportBacklog::open(&path).await.unwrap();
    assert_eq!(backlog.last_number(), 1);
    assert_eq!(rows(&backlog).await, vec![(1, vec![7])]);
    // The old row is not provisional: the master's Welcome deletes it.
    backlog.welcome_first(1).await.unwrap();
    assert!(rows(&backlog).await.is_empty());
}
