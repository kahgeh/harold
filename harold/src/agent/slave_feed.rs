//! The feeds of the slave's `LocalSource`: the agent snapshot and the screen lock. Each feed
//! publishes only a change. No line written here holds a secret or the text of a turn.

use std::sync::Arc;
use std::time::Duration;

use harold_api::harold::AgentStateSnapshot;
use tokio::sync::mpsc;
use tonic::Status;

use crate::slave::slave::LocalSource;

/// How often the lock check runs.
pub(crate) const LOCK_POLL: Duration = Duration::from_secs(3);

/// Where the feeds publish. `LocalSource` is the real sink; a test gives its own.
pub(crate) trait FeedSink: Send + Sync + 'static {
    fn publish_snapshot(&self, snapshot: AgentStateSnapshot);
    fn publish_lock(&self, locked: bool);
}

impl FeedSink for LocalSource {
    fn publish_snapshot(&self, snapshot: AgentStateSnapshot) {
        LocalSource::publish_snapshot(self, snapshot);
    }
    fn publish_lock(&self, locked: bool) {
        LocalSource::publish_lock(self, locked);
    }
}

/// Publishes each snapshot of the `WatchAgentStates` builder that differs from the last one.
/// It ends when the builder ends.
pub(crate) async fn feed_snapshots(
    mut snapshots: mpsc::Receiver<Result<AgentStateSnapshot, Status>>,
    sink: Arc<dyn FeedSink>,
) {
    let mut last: Option<AgentStateSnapshot> = None;
    while let Some(Ok(snapshot)) = snapshots.recv().await {
        if last.as_ref() == Some(&snapshot) {
            continue;
        }
        last = Some(snapshot.clone());
        sink.publish_snapshot(snapshot);
    }
}

/// Remembers the last lock state, so only a change is published.
#[derive(Default)]
pub(crate) struct LockFeed {
    last: Option<bool>,
}

impl LockFeed {
    /// Publishes `locked` when it is the first reading or differs from the last. Returns
    /// whether it published.
    pub(crate) fn observe(&mut self, locked: bool, sink: &dyn FeedSink) -> bool {
        if self.last == Some(locked) {
            return false;
        }
        self.last = Some(locked);
        sink.publish_lock(locked);
        true
    }
}

/// Reads the lock now, then every `every`, and publishes each change. `check` runs off the
/// async threads, because it starts commands.
pub(crate) async fn feed_lock(
    sink: Arc<dyn FeedSink>,
    every: Duration,
    check: impl Fn() -> bool + Send + Sync + 'static,
) {
    let check = Arc::new(check);
    let mut feed = LockFeed::default();
    loop {
        let reader = Arc::clone(&check);
        if let Ok(locked) = tokio::task::spawn_blocking(move || reader()).await {
            feed.observe(locked, sink.as_ref());
        }
        tokio::time::sleep(every).await;
    }
}
