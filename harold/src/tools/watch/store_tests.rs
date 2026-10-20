use super::*;

struct Temp(std::path::PathBuf);

impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("harold-watchdb-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn new_watch(name: &str) -> NewWatch {
    NewWatch {
        name: name.into(),
        folder: "/work/chief".into(),
        skill: "pod-watch".into(),
        report_to: "chief".into(),
        every_minutes: 10,
        every_run: false,
        run_budget_usd_micros: None,
        started_at_ms: 1_000,
        ends_at_ms: 1_000 + 120 * 60_000,
        next_run_at_ms: 1_000,
    }
}

fn run(started: i64, outcome: &str, cost: Option<i64>, failed: bool, next: i64) -> RunRecord {
    RunRecord {
        started_at_ms: started,
        outcome: outcome.into(),
        cost_usd_micros: cost,
        failed,
        next_run_at_ms: next,
    }
}

#[tokio::test]
async fn a_watch_round_trips_and_a_second_open_applies_nothing_twice() {
    let temp = Temp::new();
    {
        let store = WatchStore::open(&temp.0).await.unwrap();
        store.put_watch(&new_watch("a")).await.unwrap();
    }
    let store = WatchStore::open(&temp.0).await.unwrap();
    let row = store.get("a").await.unwrap().unwrap();
    assert_eq!(row.folder, "/work/chief");
    assert_eq!(
        (row.running, row.runs, row.reports, row.cost_usd_micros),
        (true, 0, 0, 0)
    );
    assert_eq!(row.next_run_at_ms, 1_000);
    assert_eq!(row.run_budget_usd_micros, None);
    assert_eq!(row.last_outcome, None);
    assert_eq!(store.get("missing").await.unwrap(), None);
    assert_eq!(store.running().await.unwrap().len(), 1);
}

#[tokio::test]
async fn runs_update_the_counters_and_the_next_run() {
    let temp = Temp::new();
    let store = WatchStore::open(&temp.0).await.unwrap();
    store.put_watch(&new_watch("a")).await.unwrap();

    let row = store
        .record_run(
            "a",
            1_000,
            &run(1_000, "report", Some(12_000), false, 601_000),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!((row.runs, row.reports, row.failed_in_a_row), (1, 1, 0));
    assert_eq!(row.cost_usd_micros, 12_000);
    assert_eq!(row.next_run_at_ms, 601_000);
    assert_eq!(row.last_outcome.as_deref(), Some("report"));

    let row = store
        .record_run("a", 1_000, &run(601_000, "timeout", None, true, 1_201_000))
        .await
        .unwrap()
        .unwrap();
    assert_eq!((row.runs, row.reports, row.failed_in_a_row), (2, 1, 1));
    assert_eq!(row.cost_usd_micros, 12_000, "an unknown cost adds nothing");
    let row = store
        .record_run(
            "a",
            1_000,
            &run(1_201_000, "quiet", Some(10_000), false, 1_801_000),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!((row.runs, row.failed_in_a_row), (3, 0));
    assert_eq!(row.cost_usd_micros, 22_000);
}

#[tokio::test]
async fn replacing_a_watch_restarts_its_counters_but_not_the_day_s_spend() {
    let temp = Temp::new();
    let store = WatchStore::open(&temp.0).await.unwrap();
    store.put_watch(&new_watch("a")).await.unwrap();
    store
        .record_run("a", 1_000, &run(1_000, "quiet", Some(10_000), false, 2_000))
        .await
        .unwrap();

    let mut again = new_watch("a");
    again.started_at_ms = 5_000;
    store.put_watch(&again).await.unwrap();

    let row = store.get("a").await.unwrap().unwrap();
    assert_eq!((row.runs, row.cost_usd_micros), (0, 0));
    assert_eq!(store.spent_micros(0, 100_000).await.unwrap(), 10_000);
    // A run of the replaced watch finishing now finds a different start: not recorded in it.
    assert_eq!(
        store
            .record_run("a", 1_000, &run(9_000, "quiet", Some(10_000), false, 9_999))
            .await
            .unwrap(),
        None
    );
    assert_eq!(store.get("a").await.unwrap().unwrap().runs, 0);
}

#[tokio::test]
async fn spend_sums_the_window_and_ignores_unknown_costs() {
    let temp = Temp::new();
    let store = WatchStore::open(&temp.0).await.unwrap();
    store.put_watch(&new_watch("a")).await.unwrap();
    for (at, cost) in [
        (100, Some(10_000)),
        (200, None),
        (300, Some(5_000)),
        (400, Some(7_000)),
    ] {
        store
            .record_run("a", 1_000, &run(at, "quiet", cost, false, 9_999))
            .await
            .unwrap();
    }
    assert_eq!(store.spent_micros(0, 400).await.unwrap(), 15_000);
    assert_eq!(store.spent_micros(300, 401).await.unwrap(), 12_000);
    assert_eq!(store.spent_micros(1_000, 2_000).await.unwrap(), 0);
}

#[tokio::test]
async fn ending_a_watch_keeps_it_for_the_day_and_only_once() {
    let temp = Temp::new();
    let store = WatchStore::open(&temp.0).await.unwrap();
    store.put_watch(&new_watch("a")).await.unwrap();
    store.put_watch(&new_watch("b")).await.unwrap();

    let ended = store
        .end("a", 1_000, "stopped", 50_000)
        .await
        .unwrap()
        .unwrap();
    assert!(!ended.running);
    assert_eq!(ended.stop_reason.as_deref(), Some("stopped"));
    assert_eq!(store.end("a", 1_000, "ended", 60_000).await.unwrap(), None);
    assert_eq!(store.end("a", 999, "ended", 60_000).await.unwrap(), None);

    let running: Vec<String> = store
        .running()
        .await
        .unwrap()
        .into_iter()
        .map(|w| w.name)
        .collect();
    assert_eq!(running, ["b"]);
    let today = store.ended_since(40_000).await.unwrap();
    assert_eq!(today.len(), 1);
    assert_eq!(today[0].ended_at_ms, Some(50_000));
    assert!(store.ended_since(50_001).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_run_recorded_for_a_stopped_watch_is_not_added_to_it_but_its_cost_is_spent() {
    let temp = Temp::new();
    let store = WatchStore::open(&temp.0).await.unwrap();
    store.put_watch(&new_watch("a")).await.unwrap();
    store.end("a", 1_000, "stopped", 2_000).await.unwrap();
    assert_eq!(
        store
            .record_run("a", 1_000, &run(1_500, "report", Some(9_000), false, 3_000))
            .await
            .unwrap(),
        None
    );
    store
        .record_orphan_run(
            "a",
            "/work/chief",
            1,
            &run(1_500, "report", Some(9_000), false, 3_000),
        )
        .await
        .unwrap();
    assert_eq!(store.spent_micros(0, 10_000).await.unwrap(), 9_000);
    assert_eq!(store.get("a").await.unwrap().unwrap().runs, 0);
}
