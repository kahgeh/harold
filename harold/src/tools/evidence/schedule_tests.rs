//! Tests of the daily schedule, on paused time. No test reads ClickHouse.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use super::schedule::{PERIOD, run_every};

#[test]
fn the_period_is_24_hours() {
    assert_eq!(PERIOD, Duration::from_secs(24 * 60 * 60));
}

#[tokio::test(start_paused = true)]
async fn it_runs_once_at_the_start_and_then_each_period() {
    let count = Arc::new(AtomicU32::new(0));
    let (stop, shutdown) = tokio::sync::watch::channel(());
    let counted = Arc::clone(&count);
    let task = tokio::spawn(run_every(PERIOD, shutdown, move || {
        let counted = Arc::clone(&counted);
        async move {
            counted.fetch_add(1, Ordering::SeqCst);
        }
    }));
    // Let the task start: the first pass runs with no wait.
    tokio::task::yield_now().await;
    assert_eq!(count.load(Ordering::SeqCst), 1);
    tokio::time::advance(PERIOD - Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    assert_eq!(count.load(Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    assert_eq!(count.load(Ordering::SeqCst), 2);
    drop(stop);
    task.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_shutdown_ends_the_wait_with_no_further_pass() {
    let count = Arc::new(AtomicU32::new(0));
    let (stop, shutdown) = tokio::sync::watch::channel(());
    let counted = Arc::clone(&count);
    let task = tokio::spawn(run_every(PERIOD, shutdown, move || {
        let counted = Arc::clone(&counted);
        async move {
            counted.fetch_add(1, Ordering::SeqCst);
        }
    }));
    tokio::task::yield_now().await;
    stop.send(()).unwrap();
    task.await.unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 1);
}
