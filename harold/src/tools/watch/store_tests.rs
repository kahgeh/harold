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
        run_limit_secs: 300,
        started_at_ms: 1_000,
        ends_at_ms: Some(1_000 + 120 * 60_000),
        next_run_at_ms: 1_000,
    }
}

fn run(started: i64, outcome: &str, cost: Option<i64>, failed: bool, next: i64) -> RunRecord {
    let streak = if failed {
        Streak::Failed
    } else {
        Streak::Worked
    };
    RunRecord {
        started_at_ms: started,
        outcome: outcome.into(),
        cost_usd_micros: cost,
        streak,
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
    assert_eq!(row.run_limit_secs, 300);
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

#[tokio::test]
async fn a_skipped_run_leaves_the_failed_in_a_row_count_alone() {
    let temp = Temp::new();
    let store = WatchStore::open(&temp.0).await.unwrap();
    store.put_watch(&new_watch("a")).await.unwrap();
    store
        .record_run("a", 1_000, &run(1_000, "timeout", None, true, 2_000))
        .await
        .unwrap();
    let mut skipped = run(2_000, "skipped", None, false, 3_000);
    skipped.streak = Streak::Unchanged;
    let row = store
        .record_run("a", 1_000, &skipped)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((row.runs, row.failed_in_a_row), (2, 1));
}

#[tokio::test]
async fn skipped_runs_counts_only_this_watchs_skipped_rows() {
    let temp = Temp::new();
    let store = WatchStore::open(&temp.0).await.unwrap();
    store.put_watch(&new_watch("a")).await.unwrap();
    store
        .record_run("a", 1_000, &run(1_000, "skipped", None, false, 2_000))
        .await
        .unwrap();
    store
        .record_run("a", 1_000, &run(2_000, "quiet", Some(1_000), false, 3_000))
        .await
        .unwrap();

    let mut again = new_watch("a");
    again.started_at_ms = 5_000;
    store.put_watch(&again).await.unwrap();
    store
        .record_run("a", 5_000, &run(5_000, "skipped", None, false, 6_000))
        .await
        .unwrap();
    store
        .record_run("a", 5_000, &run(6_000, "skipped", None, false, 7_000))
        .await
        .unwrap();
    store.put_watch(&new_watch("b")).await.unwrap();
    store
        .record_run("b", 1_000, &run(6_000, "skipped", None, false, 7_000))
        .await
        .unwrap();

    assert_eq!(store.skipped_runs("a", 5_000).await.unwrap(), 2);
    assert_eq!(store.skipped_runs("a", 1_000).await.unwrap(), 3);
}

#[tokio::test]
async fn budget_stopped_since_is_true_for_a_budget_stop_today_only() {
    let temp = Temp::new();
    let store = WatchStore::open(&temp.0).await.unwrap();
    assert!(!store.budget_stopped_since(10_000).await.unwrap());

    store.put_watch(&new_watch("old")).await.unwrap();
    store
        .end("old", 1_000, "daily_budget_spent", 9_000)
        .await
        .unwrap();
    store.put_watch(&new_watch("other")).await.unwrap();
    store.end("other", 1_000, "ended", 20_000).await.unwrap();
    assert!(!store.budget_stopped_since(10_000).await.unwrap());

    store.put_watch(&new_watch("today")).await.unwrap();
    store
        .end("today", 1_000, "daily_budget_spent", 10_000)
        .await
        .unwrap();
    assert!(store.budget_stopped_since(10_000).await.unwrap());
}

#[tokio::test]
async fn a_database_with_rows_from_before_migration_002_is_upgraded_and_reads_back_120_seconds() {
    let temp = Temp::new();
    {
        // A database as the first version left it: migration 001 only, with a row.
        let path = temp.0.join(DATABASE);
        let database = turso::Builder::new_local(path.to_str().unwrap())
            .build()
            .await
            .unwrap();
        let conn = database.connect().unwrap();
        let first = include_str!("migrations/001_watches.sql");
        conn.execute_batch(first).await.unwrap();
        conn.execute(
            "CREATE TABLE _migrations (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE, \
             checksum TEXT NOT NULL, applied_at_ms INTEGER NOT NULL)",
            (),
        )
        .await
        .unwrap();
        conn.execute(
            "INSERT INTO _migrations (name, checksum, applied_at_ms) VALUES (?1, ?2, 0)",
            ("001_watches", hex::encode(Sha256::digest(first.as_bytes()))),
        )
        .await
        .unwrap();
        conn.execute(
            "INSERT INTO watches (name, folder, skill, report_to, every_minutes, every_run, \
             state, started_at_ms, ends_at_ms, next_run_at_ms) \
             VALUES ('old', '/work/chief', 'pod-watch', 'chief', 10, 0, 'running', 1, 2, 1)",
            (),
        )
        .await
        .unwrap();
    }
    let store = WatchStore::open(&temp.0).await.unwrap();
    assert_eq!(store.get("old").await.unwrap().unwrap().run_limit_secs, 120);
    // The upgraded database takes a new watch with its own limit.
    store.put_watch(&new_watch("new")).await.unwrap();
    assert_eq!(store.get("new").await.unwrap().unwrap().run_limit_secs, 300);
}

/// Opens a database as an older Harold did: only migrations 001 and 002.
async fn open_at_v002(folder: &std::path::Path) -> (turso::Database, Connection) {
    let path = folder.join(DATABASE);
    let database = turso::Builder::new_local(path.to_str().unwrap())
        .build()
        .await
        .unwrap();
    let conn = database.connect().unwrap();
    conn.execute(
        "CREATE TABLE _migrations (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE, \
         checksum TEXT NOT NULL, applied_at_ms INTEGER NOT NULL)",
        (),
    )
    .await
    .unwrap();
    for (name, sql) in &MIGRATIONS[..2] {
        conn.execute_batch(sql).await.unwrap();
        let checksum = hex::encode(Sha256::digest(sql.as_bytes()));
        conn.execute(
            "INSERT INTO _migrations (name, checksum, applied_at_ms) VALUES (?1, ?2, 0)",
            (*name, checksum),
        )
        .await
        .unwrap();
    }
    (database, conn)
}

#[tokio::test]
async fn migration_003_keeps_every_row_of_a_v002_database() {
    let temp = Temp::new();
    {
        let (_database, conn) = open_at_v002(&temp.0).await;
        for (index, name) in ["a", "b", "c"].into_iter().enumerate() {
            let index = index as i64;
            conn.execute(
                "INSERT INTO watches (name, folder, skill, report_to, every_minutes, every_run, \
                 state, started_at_ms, ends_at_ms, next_run_at_ms, runs, cost_usd_micros, \
                 run_limit_secs) \
                 VALUES (?1, '/work', 'pod-watch', 'chief', 10, 0, 'running', ?2, ?3, ?2, 1, 5, 77)",
                (name, 1_000 + index, 9_000 + index),
            )
            .await
            .unwrap();
            conn.execute(
                "INSERT INTO runs (watch, folder, run, started_at_ms, outcome, cost_usd_micros) \
                 VALUES (?1, '/work', 1, ?2, 'quiet', 5)",
                (name, 1_000 + index),
            )
            .await
            .unwrap();
        }
    }
    let store = WatchStore::open(&temp.0).await.unwrap();
    let rows = store.running().await.unwrap();
    assert_eq!(rows.len(), 3);
    for (index, row) in rows.iter().enumerate() {
        let index = index as i64;
        assert_eq!(row.started_at_ms, 1_000 + index);
        assert_eq!(row.ends_at_ms, Some(9_000 + index));
        assert_eq!(
            (row.runs, row.cost_usd_micros, row.run_limit_secs),
            (1, 5, 77)
        );
        assert_eq!(row.state, WatchState::Running);
        assert_eq!((row.paused_until_ms, row.pause_how.clone()), (None, None));
    }
    assert_eq!(store.spent_micros(0, 10_000).await.unwrap(), 15);
    assert_eq!(store.skipped_runs("a", 0).await.unwrap(), 0);
    // The runs index survives the rebuild.
    let conn = store.conn.lock().await;
    let mut found = conn
        .query(
            "SELECT COUNT(*) FROM sqlite_master WHERE name = 'runs_by_start'",
            (),
        )
        .await
        .unwrap();
    assert_eq!(
        integer(&found.next().await.unwrap().unwrap(), 0).unwrap(),
        1
    );
}

#[tokio::test]
async fn an_open_ended_watch_stores_no_end() {
    let temp = Temp::new();
    let store = WatchStore::open(&temp.0).await.unwrap();
    let mut watch = new_watch("open");
    watch.ends_at_ms = None;
    store.put_watch(&watch).await.unwrap();
    let row = store.get("open").await.unwrap().unwrap();
    assert_eq!(row.ends_at_ms, None);
    assert!(row.is_live());
    assert_eq!(store.running().await.unwrap().len(), 1);
}

#[tokio::test]
async fn pause_and_resume_round_trip() {
    let temp = Temp::new();
    let store = WatchStore::open(&temp.0).await.unwrap();
    store.put_watch(&new_watch("a")).await.unwrap();
    store.put_watch(&new_watch("b")).await.unwrap();

    let row = store
        .pause("a", 1_000, Some(5_000), "cli")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.state, WatchState::Paused);
    assert!(!row.running);
    assert!(row.is_live());
    assert_eq!(row.paused_until_ms, Some(5_000));
    assert_eq!(row.pause_how.as_deref(), Some("cli"));
    assert_eq!(store.running().await.unwrap().len(), 1);
    assert_eq!(store.live().await.unwrap().len(), 2);
    assert!(store.ended_since(0).await.unwrap().is_empty());
    assert_eq!(store.pause("a", 1_000, None, "cli").await.unwrap(), None);
    assert_eq!(store.pause("a", 9, None, "cli").await.unwrap(), None);

    let row = store.resume("a", 1_000, 4_000).await.unwrap().unwrap();
    assert_eq!(row.state, WatchState::Running);
    assert!(row.running);
    assert_eq!(row.next_run_at_ms, 4_000);
    assert_eq!((row.paused_until_ms, row.pause_how), (None, None));
    assert_eq!(store.resume("a", 1_000, 4_000).await.unwrap(), None);

    // A watch paused with no end time ends from the paused state.
    store.pause("b", 1_000, None, "budget_day").await.unwrap();
    let row = store
        .end("b", 1_000, "stopped", 6_000)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.state, WatchState::Stopped);
}

fn new_reminder(name: &str) -> NewReminder {
    NewReminder {
        name: name.into(),
        folder: "/work/chief".into(),
        report_to: "chief".into(),
        text: "call back".into(),
        due_at_ms: Some(9_000),
        due_local: Some("2026-11-03 09:00".into()),
    }
}

#[tokio::test]
async fn finish_reminder_is_once_only() {
    let temp = Temp::new();
    let store = WatchStore::open(&temp.0).await.unwrap();
    store.put_reminder(&new_reminder("r"), 1_000).await.unwrap();
    let row = store.reminder("r").await.unwrap().unwrap();
    assert_eq!(row.state, ReminderState::Waiting);
    assert_eq!(row.due_at_ms, Some(9_000));
    assert_eq!(row.text, "call back");
    assert_eq!(store.waiting_reminders().await.unwrap().len(), 1);

    let done = store
        .finish_reminder("r", 1_000, ReminderState::Typed, 9_001)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(done.state, ReminderState::Typed);
    assert_eq!(done.ended_at_ms, Some(9_001));
    assert_eq!(
        store
            .finish_reminder("r", 1_000, ReminderState::Dropped, 9_002)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        store.reminder("r").await.unwrap().unwrap().state,
        ReminderState::Typed
    );
    assert!(store.waiting_reminders().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_reminder_with_no_time_is_refused() {
    let temp = Temp::new();
    let store = WatchStore::open(&temp.0).await.unwrap();
    let mut reminder = new_reminder("r");
    reminder.due_at_ms = None;
    reminder.due_local = None;
    assert!(matches!(
        store.put_reminder(&reminder, 1_000).await,
        Err(StoreError::Data(_))
    ));
    assert!(store.reminder("r").await.unwrap().is_none());

    reminder.due_local = Some("2026-11-03 09:00".into());
    store.put_reminder(&reminder, 1_000).await.unwrap();
    reminder.due_local = None;
    reminder.due_at_ms = Some(9_000);
    store.put_reminder(&reminder, 2_000).await.unwrap();
}
