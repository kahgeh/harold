//! The jobs that the snapshot shows. The job tool publishes the list here after each
//! change, and the agent state stream reads it. The list is in memory only.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use tokio::sync::watch;

use super::store::{JobRow, JobState, JobStore, SHOW_AFTER_END_MS};
use crate::harold::{AgentJob, AgentStateSnapshot};

/// The jobs to show, and how many times the list changed in this run of Harold.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct JobsView {
    /// Raises `through_event_version` by this much, so every control-tower gets a change.
    pub(crate) revision: u64,
    pub(crate) jobs: Vec<AgentJob>,
}

/// What the master keeps of one slave's jobs.
#[derive(Default)]
struct SlaveJobs {
    /// The jobs of the slave's last snapshot.
    jobs: Vec<AgentJob>,
    /// When the master's slave link to the slave closed. `None` while it is open.
    closed_at_ms: Option<i64>,
}

#[derive(Default)]
struct FeedState {
    /// The jobs of this machine.
    own: Vec<AgentJob>,
    /// The jobs of each slave, by the slave's name.
    slaves: BTreeMap<String, SlaveJobs>,
}

/// The jobs of one slave as the master last got them, with the host set.
pub(super) struct SlaveJobsView {
    pub(super) closed_at_ms: Option<i64>,
    pub(super) jobs: Vec<AgentJob>,
}

pub(crate) struct JobsFeed {
    sender: watch::Sender<Arc<JobsView>>,
    state: Mutex<FeedState>,
}

impl JobsFeed {
    /// A feed of its own. Tests use it, so parallel tests never see each other's jobs.
    pub(crate) fn new() -> Self {
        Self {
            sender: watch::channel(Arc::new(JobsView::default())).0,
            state: Mutex::new(FeedState::default()),
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

    #[cfg(test)]
    pub(crate) fn current(&self) -> Arc<JobsView> {
        Arc::clone(&self.sender.borrow())
    }

    fn state(&self) -> std::sync::MutexGuard<'_, FeedState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Shows `jobs` of this machine, with the jobs of the slaves. The revision rises only
    /// when the list is not the one shown now.
    pub(super) fn publish(&self, jobs: Vec<AgentJob>) {
        let mut state = self.state();
        state.own = jobs;
        self.send(&state, None);
    }

    /// The master got the jobs of the slave's snapshot.
    pub(crate) fn set_slave_jobs(&self, slave: &str, jobs: Vec<AgentJob>, now_ms: i64) {
        let mut state = self.state();
        state.slaves.entry(slave.to_owned()).or_default().jobs = jobs;
        self.send(&state, Some(now_ms));
    }

    /// The master's slave link to `slave` opened or closed. While it is closed, the jobs
    /// of the slave that run read `unknown`.
    pub(crate) fn slave_link(&self, slave: &str, up: bool, now_ms: i64) {
        let mut state = self.state();
        let entry = state.slaves.entry(slave.to_owned()).or_default();
        entry.closed_at_ms = if up { None } else { Some(now_ms) };
        self.send(&state, Some(now_ms));
    }

    /// Takes the ended jobs of the slaves away 10 minutes after their end. The check calls
    /// it, because a closed link brings no snapshot.
    pub(super) fn age_slaves(&self, now_ms: i64) {
        let state = self.state();
        self.send(&state, Some(now_ms));
    }

    /// The jobs of each slave as the slave last sent them, for the master's end lines.
    pub(super) fn slave_jobs(&self) -> Vec<SlaveJobsView> {
        self.state()
            .slaves
            .iter()
            .map(|(host, slave)| SlaveJobsView {
                closed_at_ms: slave.closed_at_ms,
                jobs: slave
                    .jobs
                    .iter()
                    .map(|job| AgentJob {
                        host: host.clone(),
                        ..job.clone()
                    })
                    .collect(),
            })
            .collect()
    }

    fn send(&self, state: &FeedState, now_ms: Option<i64>) {
        let mut jobs = state.own.clone();
        for (host, slave) in &state.slaves {
            for job in &slave.jobs {
                let mut job = AgentJob {
                    host: host.clone(),
                    ..job.clone()
                };
                if let Some(now_ms) = now_ms
                    && job.state == "ended"
                    && job.ended_at_ms <= now_ms - SHOW_AFTER_END_MS
                {
                    continue;
                }
                if let Some(closed) = slave.closed_at_ms {
                    job.unreached_since_ms = closed;
                    if job.state == "running" {
                        job.state = "unknown".into();
                    }
                }
                jobs.push(job);
            }
        }
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
        unit: job.unit.clone(),
        exit_code: job.exit_code.unwrap_or(0),
        exit_known: job.exit_code.is_some(),
        unreached_since_ms: 0,
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
