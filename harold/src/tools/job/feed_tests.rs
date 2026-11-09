use super::*;
use crate::tools::job::store::NewJob;
use crate::tools::job::test_support::Temp;

fn new_job(name: &str) -> NewJob {
    NewJob {
        name: name.into(),
        what: "tests".into(),
        pane_id: "%4".into(),
        pid: 4121,
        process_start_s: 1,
        started_at_ms: 1_000,
        due_at_ms: 2_000,
    }
}

#[tokio::test]
async fn the_snapshot_carries_the_running_jobs_with_the_fields_the_proto_names() {
    let dir = Temp::new();
    let store = dir.store().await;
    store.add(&new_job("macos-tests")).await.unwrap();
    let feed = JobsFeed::new();
    feed.refresh(&store, 1_500).await;
    let view = feed.current();
    assert_eq!(view.jobs.len(), 1);
    let job = &view.jobs[0];
    assert_eq!(
        (job.name.as_str(), job.what.as_str(), job.pane_id.as_str(), job.host.as_str()),
        ("macos-tests", "tests", "%4", "")
    );
    assert_eq!((job.pid, job.started_at_ms, job.due_at_ms, job.ended_at_ms), (4121, 1_000, 2_000, 0));
    assert_eq!(job.state, "running");
}

#[tokio::test]
async fn a_job_change_raises_through_event_version_and_no_change_does_not() {
    let dir = Temp::new();
    let store = dir.store().await;
    let feed = JobsFeed::new();
    let base = AgentStateSnapshot {
        through_event_version: 40,
        ..Default::default()
    };
    feed.refresh(&store, 0).await;
    assert_eq!(with_jobs(base.clone(), &feed.current()).through_event_version, 40);
    store.add(&new_job("a")).await.unwrap();
    feed.refresh(&store, 0).await;
    assert_eq!(with_jobs(base.clone(), &feed.current()).through_event_version, 41);
    feed.refresh(&store, 0).await;
    assert_eq!(with_jobs(base.clone(), &feed.current()).through_event_version, 41, "same list");
    let id = store.running().await.unwrap()[0].id;
    store.mark_ended(id, 3_000).await.unwrap();
    feed.refresh(&store, 3_000).await;
    let ended = with_jobs(base.clone(), &feed.current());
    assert_eq!(ended.through_event_version, 42);
    assert_eq!((ended.jobs[0].state.as_str(), ended.jobs[0].ended_at_ms), ("ended", 3_000));
}

#[tokio::test]
async fn an_ended_job_leaves_the_snapshot_after_10_minutes_and_a_removed_job_at_once() {
    let dir = Temp::new();
    let store = dir.store().await;
    let feed = JobsFeed::new();
    store.add(&new_job("a")).await.unwrap();
    store.add(&new_job("b")).await.unwrap();
    let id = store.running().await.unwrap()[0].id;
    store.mark_ended(id, 3_000).await.unwrap();
    store.remove("b", 3_000).await.unwrap();
    feed.refresh(&store, 3_000).await;
    assert_eq!(feed.current().jobs.len(), 1);
    feed.refresh(&store, 3_000 + 10 * 60_000).await;
    assert!(feed.current().jobs.is_empty());
}

#[tokio::test]
async fn a_subscriber_wakes_on_a_change_and_each_feed_is_its_own() {
    let dir = Temp::new();
    let store = dir.store().await;
    let (feed, other) = (JobsFeed::new(), JobsFeed::new());
    let mut receiver = feed.subscribe();
    store.add(&new_job("a")).await.unwrap();
    feed.refresh(&store, 0).await;
    assert!(receiver.has_changed().unwrap());
    assert!(other.current().jobs.is_empty());
}
