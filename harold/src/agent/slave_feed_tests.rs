use std::sync::{Arc, Mutex};
use std::time::Duration;

use harold_api::harold::{AgentPaneState, AgentStateSnapshot};
use tokio::sync::mpsc;

use super::slave_feed::{FeedSink, LockFeed, feed_lock, feed_snapshots};

#[derive(Default)]
struct Recorder {
    snapshots: Mutex<Vec<u64>>,
    locks: Mutex<Vec<bool>>,
}

impl FeedSink for Recorder {
    fn publish_snapshot(&self, snapshot: AgentStateSnapshot) {
        self.snapshots
            .lock()
            .unwrap()
            .push(snapshot.through_event_version);
    }
    fn publish_lock(&self, locked: bool) {
        self.locks.lock().unwrap().push(locked);
    }
}

fn snapshot(version: u64) -> AgentStateSnapshot {
    AgentStateSnapshot {
        through_event_version: version,
        ..Default::default()
    }
}

#[tokio::test]
async fn a_snapshot_change_reaches_the_sink_once_and_a_repeat_does_not() {
    let sink = Arc::new(Recorder::default());
    let (sender, receiver) = mpsc::channel(8);
    for version in [1, 1, 2, 2, 2] {
        sender.send(Ok(snapshot(version))).await.unwrap();
    }
    drop(sender);
    feed_snapshots(receiver, sink.clone()).await;
    assert_eq!(*sink.snapshots.lock().unwrap(), [1, 2]);
}

#[test]
fn a_lock_change_reaches_the_sink_once_and_a_repeat_does_not() {
    let sink = Recorder::default();
    let mut feed = LockFeed::default();
    assert!(feed.observe(false, &sink), "the first reading is published");
    assert!(!feed.observe(false, &sink));
    assert!(feed.observe(true, &sink));
    assert!(!feed.observe(true, &sink));
    assert!(feed.observe(false, &sink));
    assert_eq!(*sink.locks.lock().unwrap(), [false, true, false]);
}

#[tokio::test(start_paused = true)]
async fn the_lock_loop_publishes_at_start_and_on_change_only() {
    let sink = Arc::new(Recorder::default());
    let readings = Arc::new(Mutex::new(vec![true, true, false].into_iter()));
    let source = Arc::clone(&readings);
    let task = tokio::spawn(feed_lock(sink.clone(), Duration::from_secs(3), move || {
        source.lock().unwrap().next().unwrap_or(false)
    }));
    for _ in 0..6 {
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    task.abort();
    assert_eq!(*sink.locks.lock().unwrap(), [true, false]);
}

/// Keeps the whole snapshot, to read what the master would receive.
#[derive(Default)]
struct Whole(Mutex<Vec<AgentStateSnapshot>>);

impl FeedSink for Whole {
    fn publish_snapshot(&self, snapshot: AgentStateSnapshot) {
        self.0.lock().unwrap().push(snapshot);
    }
    fn publish_lock(&self, _locked: bool) {}
}

#[tokio::test]
async fn the_recap_text_does_not_leave_the_slave() {
    let pane = AgentPaneState {
        pane_id: "block:1".into(),
        recap: Some("a recap".into()),
        recap_at_ms: 99,
        work_summary: Some("a summary".into()),
        current_step: Some("a step".into()),
        ..Default::default()
    };
    let sink = Arc::new(Whole::default());
    let (sender, receiver) = mpsc::channel(2);
    sender
        .send(Ok(AgentStateSnapshot {
            panes: vec![pane],
            ..Default::default()
        }))
        .await
        .unwrap();
    drop(sender);
    feed_snapshots(receiver, sink.clone()).await;
    let published = sink.0.lock().unwrap();
    let pane = &published[0].panes[0];
    assert_eq!(pane.recap, None);
    assert_eq!(pane.recap_at_ms, 0);
    assert_eq!(pane.work_summary, None);
    // The rest of the row stays.
    assert_eq!(pane.pane_id, "block:1");
    assert_eq!(pane.current_step.as_deref(), Some("a step"));
}
