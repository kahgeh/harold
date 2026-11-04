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
