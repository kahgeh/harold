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

#[tokio::test]
async fn renumber_new_above_keeps_order_and_leaves_old_rows() {
    let path = temp_db("renumber");
    let backlog = ReportBacklog::open(&path).await.unwrap();
    for payload in [1u8, 2] {
        backlog
            .append(ReportKind::Turn, vec![payload], t0())
            .await
            .unwrap();
    }
    // The process opened the backlog at last = 2. These three are new rows 3, 4, 5.
    for payload in [30u8, 40, 50] {
        backlog
            .append(ReportKind::Lock, vec![payload], t0())
            .await
            .unwrap();
    }
    backlog.renumber_new_above(2, 9).await.unwrap();
    let rows = backlog.after(0).await.unwrap();
    assert_eq!(
        rows,
        vec![
            (1, vec![1]),
            (2, vec![2]),
            (10, vec![30]),
            (11, vec![40]),
            (12, vec![50])
        ]
    );
    assert_eq!(backlog.last_number(), 12);
    assert_eq!(backlog.len(), 5);
    // The next report is above the last row, and the numbers survive a reopen.
    assert_eq!(
        backlog
            .append(ReportKind::Turn, vec![9], t0())
            .await
            .unwrap(),
        13
    );
    drop(backlog);
    let backlog = ReportBacklog::open(&path).await.unwrap();
    assert_eq!(backlog.last_number(), 13);
    assert_eq!(backlog.after(9).await.unwrap().len(), 4);
}

#[tokio::test]
async fn renumber_new_above_with_overlapping_numbers_loses_no_row() {
    let path = temp_db("renumber-overlap");
    let backlog = ReportBacklog::open(&path).await.unwrap();
    for payload in [1u8, 2, 3, 4] {
        backlog
            .append(ReportKind::Turn, vec![payload], t0())
            .await
            .unwrap();
    }
    // New rows 2, 3, 4 move to 3, 4, 5: the new numbers overlap the old ones.
    backlog.renumber_new_above(1, 2).await.unwrap();
    let rows = backlog.after(0).await.unwrap();
    assert_eq!(
        rows,
        vec![(1, vec![1]), (3, vec![2]), (4, vec![3]), (5, vec![4])]
    );
}
