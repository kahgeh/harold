use std::sync::atomic::{AtomicI64, Ordering};

use super::*;
use crate::outbound::fake::CapturedLogs;
use crate::telemetry::test_support as telemetry_support;
use crate::tools::decision::store::NewDecision;
use crate::tools::decision::test_support::Temp;

/// 2026-11-14 03:10 UTC.
const NOW: i64 = 1_794_625_800_000;
const DAY_MS: i64 = 86_400_000;

fn new_decision(check_on: &str) -> NewDecision {
    NewDecision {
        organisation: "org-check".into(),
        role: "chief".into(),
        kind: "model".into(),
        details_owner: "chief".into(),
        accountable: "user".into(),
        title: "A secret title".into(),
        check_on: check_on.into(),
        details: "vault:decisions.md#d-1".into(),
        ..Default::default()
    }
}

struct Rig {
    checker: Checker,
    store: Arc<DecisionStore>,
    now: Arc<AtomicI64>,
    _dir: Temp,
}

async fn rig() -> Rig {
    rig_for("org-check").await
}

async fn rig_for(organisation: &str) -> Rig {
    let dir = Temp::new();
    let store = dir.store().await;
    let now = Arc::new(AtomicI64::new(NOW));
    let clock = Arc::clone(&now);
    let checker = Checker::new(
        Arc::clone(&store),
        Arc::new(move || clock.load(Ordering::Relaxed)),
        organisation.into(),
        time::UtcOffset::UTC,
    );
    Rig {
        checker,
        store,
        now,
        _dir: dir,
    }
}

#[tokio::test]
async fn an_open_record_becomes_due_on_its_check_date_and_not_before() {
    let rig = rig().await;
    rig.store
        .add(
            "d-20261110-01",
            &new_decision("2026-11-14"),
            NOW - 4 * DAY_MS,
        )
        .await
        .unwrap();
    rig.store
        .add(
            "d-20261110-02",
            &new_decision("2026-11-15"),
            NOW - 4 * DAY_MS,
        )
        .await
        .unwrap();
    let turned = rig.checker.pass().await;
    assert_eq!(
        turned.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
        ["d-20261110-01"]
    );
    assert_eq!(rig.store.counts().await.unwrap(), (1, 1));
    rig.now.store(NOW + DAY_MS, Ordering::Relaxed);
    let next_day = rig.checker.pass().await;
    assert_eq!(
        next_day.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
        ["d-20261110-02"]
    );
}

#[tokio::test]
async fn the_pass_at_start_catches_a_record_already_past_its_date() {
    let rig = rig().await;
    rig.store
        .add(
            "d-20260901-01",
            &new_decision("2026-10-01"),
            NOW - 60 * DAY_MS,
        )
        .await
        .unwrap();
    assert_eq!(rig.checker.pass().await.len(), 1);
}

#[tokio::test]
async fn a_recheck_to_a_past_date_is_due_again_at_the_next_pass() {
    let rig = rig().await;
    let row = rig
        .store
        .add(
            "d-20261110-01",
            &new_decision("2026-11-14"),
            NOW - 4 * DAY_MS,
        )
        .await
        .unwrap()
        .unwrap();
    rig.checker.pass().await;
    rig.store
        .recheck(&row.id, "2026-11-01")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(rig.store.counts().await.unwrap(), (1, 0));
    assert_eq!(rig.checker.pass().await.len(), 1);
}

#[tokio::test]
async fn the_local_date_decides_not_the_utc_date() {
    let dir = Temp::new();
    let store = dir.store().await;
    store
        .add(
            "d-20261110-01",
            &new_decision("2026-11-14"),
            NOW - 4 * DAY_MS,
        )
        .await
        .unwrap();
    // 03:10 UTC on the 14th is 20:10 on the 13th at UTC-7: not yet the 14th.
    let west = time::UtcOffset::from_hms(-7, 0, 0).unwrap();
    let checker = Checker::new(
        Arc::clone(&store),
        Arc::new(|| NOW),
        "org-check".into(),
        west,
    );
    assert!(checker.pass().await.is_empty());
}

#[tokio::test]
async fn a_pass_logs_each_record_once_without_the_title_and_sets_the_gauge() {
    let logs = CapturedLogs::start();
    telemetry_support::install();
    let rig = rig_for("org-check-gauge").await;
    rig.store
        .add(
            "d-20261110-01",
            &new_decision("2026-11-14"),
            NOW - 4 * DAY_MS,
        )
        .await
        .unwrap();
    rig.checker.pass().await;
    rig.checker.pass().await;
    let text = logs.text();
    assert_eq!(text.matches("decision due").count(), 1, "{text}");
    assert!(
        text.contains("decision.id=d-20261110-01") && text.contains("days_open=4"),
        "{text}"
    );
    assert!(!text.contains("secret title"));
    let read = |status: &str| {
        telemetry_support::gauge_value(
            "harold.decisions.open",
            &[("organisation", "org-check-gauge"), ("status", status)],
        )
    };
    assert_eq!((read("open"), read("due")), (Some(0), Some(1)));
}

#[test]
fn the_delay_to_00_05_local_is_right_before_and_after_that_time() {
    let utc = time::UtcOffset::UTC;
    // 00:00 on the 14th: five minutes.
    let midnight = NOW - (3 * 60 + 10) * 60_000;
    assert_eq!(next_run_delay(midnight, utc), Duration::from_secs(300));
    // 03:10: 20 h 55 min to 00:05 the next day.
    assert_eq!(
        next_run_delay(NOW, utc),
        Duration::from_secs(20 * 3600 + 55 * 60)
    );
    // Exactly 00:05: a whole day, not zero.
    assert_eq!(
        next_run_delay(midnight + 300_000, utc),
        Duration::from_secs(86_400)
    );
    // At UTC+11 the local time of NOW is 14:10: 9 h 55 min.
    let sydney = time::UtcOffset::from_hms(11, 0, 0).unwrap();
    assert_eq!(
        next_run_delay(NOW, sydney),
        Duration::from_secs(9 * 3600 + 55 * 60)
    );
}

#[tokio::test(start_paused = true)]
async fn run_passes_at_start_then_each_day_and_stops_on_shutdown() {
    let rig = rig().await;
    rig.store
        .add(
            "d-20261110-01",
            &new_decision("2026-11-14"),
            NOW - 4 * DAY_MS,
        )
        .await
        .unwrap();
    rig.store
        .add(
            "d-20261110-02",
            &new_decision("2026-11-15"),
            NOW - 4 * DAY_MS,
        )
        .await
        .unwrap();
    let (stop, shutdown) = tokio::sync::watch::channel(());
    let now = Arc::clone(&rig.now);
    let task = tokio::spawn(run(rig.checker, shutdown));
    // The start pass.
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(rig.store.counts().await.unwrap(), (1, 1));
    // The next run is 20 h 55 min away. The clock moves with the paused time.
    now.store(NOW + DAY_MS, Ordering::Relaxed);
    tokio::time::sleep(Duration::from_secs(20 * 3600 + 56 * 60)).await;
    assert_eq!(rig.store.counts().await.unwrap(), (0, 2));
    stop.send(()).unwrap();
    task.await.unwrap();
}
