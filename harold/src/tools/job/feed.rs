//! The jobs that the snapshot shows. The job tool publishes the list here after each
//! change, and the agent state stream reads it. The list is in memory only.

use std::sync::{Arc, OnceLock};

use tokio::sync::watch;

use super::store::{JobRow, JobState, JobStore};
use crate::harold::{AgentJob, AgentStateSnapshot};

/// The jobs to show, and how many times the list changed in this run of Harold.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct JobsView {
    /// Raises `through_event_version` by this much, so every control-tower gets a change.
    pub(crate) revision: u64,
    pub(crate) jobs: Vec<AgentJob>,
}

pub(crate) struct JobsFeed {
    sender: watch::Sender<Arc<JobsView>>,
}

impl JobsFeed {
    /// A feed of its own. Tests use it, so parallel tests never see each other's jobs.
    pub(crate) fn new() -> Self {
        Self {
            sender: watch::channel(Arc::new(JobsView::default())).0,
        }
    }

    /// The feed of this process, the one the agent state stream reads.
    pub(crate) fn global() -> &'static Arc<JobsFeed> {
        static FEED: OnceLock<Arc<JobsFeed>> = OnceLock::new();
        FEED.get_or_init(|| Arc::new(JobsFeed::new()))
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<Arc<JobsView>> {
        self.sender.subscribe()
    }

    pub(crate) fn current(&self) -> Arc<JobsView> {
        Arc::clone(&self.sender.borrow())
    }

    /// Shows `jobs`. The revision rises only when the list is not the one shown now.
    pub(super) fn publish(&self, jobs: Vec<AgentJob>) {
        self.sender.send_if_modified(|view| {
            if view.jobs == jobs {
                return false;
            }
            *view = Arc::new(JobsView {
                revision: view.revision + 1,
                jobs,
            });
            true
        });
    }

    /// Reads the jobs to show from the store and publishes them.
    pub(super) async fn refresh(&self, store: &JobStore, now_ms: i64) {
        if let Ok(rows) = store.shown(now_ms).await {
            self.publish(rows.iter().map(to_agent_job).collect());
        }
    }
}

/// The job as the snapshot carries it. `host` is empty: the job runs on Harold's machine.
pub(super) fn to_agent_job(job: &JobRow) -> AgentJob {
    AgentJob {
        name: job.name.clone(),
        what: job.what.clone(),
        pane_id: job.pane_id.clone(),
        host: String::new(),
        pid: job.pid,
        started_at_ms: job.started_at_ms,
        due_at_ms: job.due_at_ms,
        ended_at_ms: match job.state {
            JobState::Running => 0,
            JobState::Ended | JobState::Removed => job.ended_at_ms,
        },
        state: match job.state {
            JobState::Running => "running",
            JobState::Ended | JobState::Removed => "ended",
        }
        .into(),
    }
}

/// Puts the jobs in `snapshot` and raises `through_event_version` by the revision of the
/// list. `map_agent_snapshot` leaves them out: the agent state does not hold them.
pub(crate) fn with_jobs(mut snapshot: AgentStateSnapshot, view: &JobsView) -> AgentStateSnapshot {
    snapshot.jobs = view.jobs.clone();
    snapshot.through_event_version += view.revision;
    snapshot
}

/// The receiver of the process-wide feed.
pub(crate) fn subscribe() -> watch::Receiver<Arc<JobsView>> {
    JobsFeed::global().subscribe()
}

#[cfg(test)]
#[path = "feed_tests.rs"]
mod tests;
