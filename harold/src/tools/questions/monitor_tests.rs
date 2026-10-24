use std::time::Duration;

use tokio::sync::watch;

use super::*;
use crate::tools::questions::test_support::*;

const HOUR_MS: i64 = 3_600_000;

#[test]
fn the_line_is_stale_past_two_hours_with_a_todo_waiting_or_before_any_check() {
    let checked = 10 * HOUR_MS;
    assert!(!is_stale(checked + 2 * HOUR_MS, checked, 1), "2 h exactly");
    assert!(is_stale(checked + 2 * HOUR_MS + 1, checked, 1));
    assert!(
        !is_stale(checked + 5 * HOUR_MS, checked, 0),
        "nothing waits"
    );
    assert!(is_stale(HOUR_MS, 0, 1), "never checked");
    assert!(!is_stale(HOUR_MS, 0, 0));
}

#[test]
fn a_stamp_in_the_future_is_not_stale() {
    assert!(!is_stale(1_000, 1_000 + 5 * HOUR_MS, 1));
}

async fn minutes(count: u64) {
    tokio::time::sleep(Duration::from_secs(60 * count)).await;
}

#[tokio::test(start_paused = true)]
async fn todos_not_checked_is_logged_once_and_again_only_after_a_check() {
    let (logs, _guard) = capture_logs();
    let temp = TempFolder::new();
    let store = open_store(&temp).await;
    store
        .put(
            super::super::schema::from_proto(question("Q1", "s", "abc")).unwrap(),
            1,
        )
        .await
        .unwrap();
    store.mark_checked(clock_ms()).await.unwrap();
    let (stop, shutdown) = watch::channel(());
    let task = tokio::spawn(run(store.clone(), shutdown));

    minutes(119).await;
    assert!(
        logs.with("todos not checked").is_empty(),
        "{:#?}",
        logs.lines()
    );

    minutes(3).await;
    let lines = logs.with("todos not checked");
    assert_eq!(lines.len(), 1, "{:#?}", logs.lines());
    assert_eq!(
        field_names(&lines[0], "todos not checked"),
        ["tool", "waiting", "minutes", "never"]
    );
    assert!(lines[0].contains("waiting=1"), "{}", lines[0]);
    assert!(lines[0].contains("never=false"), "{}", lines[0]);

    minutes(90).await;
    assert_eq!(
        logs.with("todos not checked").len(),
        1,
        "once while it holds"
    );

    store.mark_checked(clock_ms()).await.unwrap();
    minutes(60).await;
    assert_eq!(
        logs.with("todos not checked").len(),
        1,
        "a check cleared it"
    );

    minutes(70).await;
    assert_eq!(
        logs.with("todos not checked").len(),
        2,
        "{:#?}",
        logs.lines()
    );

    drop(stop);
    task.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn before_any_check_the_line_says_never_and_nothing_waiting_logs_nothing() {
    let (logs, _guard) = capture_logs();
    let temp = TempFolder::new();
    let store = open_store(&temp).await;
    let (stop, shutdown) = watch::channel(());
    let task = tokio::spawn(run(store.clone(), shutdown));

    minutes(5).await;
    assert!(logs.with("todos not checked").is_empty(), "no todo waits");

    store
        .put(
            super::super::schema::from_proto(question("Q1", "s", "abc")).unwrap(),
            1,
        )
        .await
        .unwrap();
    minutes(2).await;
    let lines = logs.with("todos not checked");
    assert_eq!(lines.len(), 1, "{:#?}", logs.lines());
    assert!(lines[0].contains("minutes=0"), "{}", lines[0]);
    assert!(lines[0].contains("never=true"), "{}", lines[0]);

    drop(stop);
    task.await.unwrap();
}
