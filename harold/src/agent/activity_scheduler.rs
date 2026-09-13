use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use events::EventStreamVersion;
use tokio::task::JoinSet;

use crate::activity_summary::{ActivitySummarizer, ActivitySummaryInput};
use crate::settings::ActivitySummarySettings;

use super::super::domain::AgentIncarnation;

pub(super) struct SummaryJob {
    pub incarnation: AgentIncarnation,
    pub basis_version: EventStreamVersion,
    pub input: ActivitySummaryInput,
}

pub(super) struct SummaryResult {
    pub incarnation: AgentIncarnation,
    pub basis_version: EventStreamVersion,
    pub description: Option<String>,
}

/// One running request per pane and one latest pending replacement per pane.
/// Both total active requests and total pending evidence are bounded.
pub(super) struct ActivityScheduler {
    provider: Arc<dyn ActivitySummarizer>,
    settings: ActivitySummarySettings,
    pending: VecDeque<SummaryJob>,
    active: HashMap<tokio::task::Id, String>,
    tasks: JoinSet<SummaryResult>,
}

impl ActivityScheduler {
    pub fn new(provider: Arc<dyn ActivitySummarizer>, settings: ActivitySummarySettings) -> Self {
        Self {
            provider,
            settings,
            pending: VecDeque::new(),
            active: HashMap::new(),
            tasks: JoinSet::new(),
        }
    }

    pub fn input(&self, instruction: &str, assistant_reply: Option<&str>) -> ActivitySummaryInput {
        ActivitySummaryInput {
            instruction: instruction
                .chars()
                .take(self.settings.max_instruction_chars)
                .collect(),
            assistant_reply: assistant_reply
                .map(|reply| reply.chars().take(self.settings.max_reply_chars).collect()),
        }
    }

    pub fn invalidate(&mut self, pane_id: &str) {
        self.pending
            .retain(|job| job.incarnation.pane_id != pane_id);
    }

    pub fn enqueue(&mut self, job: SummaryJob) {
        self.invalidate(&job.incarnation.pane_id);
        if self.pending.len() >= self.settings.max_pending {
            self.pending.pop_front();
        }
        self.pending.push_back(job);
        self.start_available();
    }

    fn start_available(&mut self) {
        while self.tasks.len() < self.settings.max_concurrent {
            let Some(index) = self.pending.iter().position(|job| {
                !self
                    .active
                    .values()
                    .any(|pane_id| pane_id == &job.incarnation.pane_id)
            }) else {
                break;
            };
            let Some(job) = self.pending.remove(index) else {
                break;
            };
            let pane_id = job.incarnation.pane_id.clone();
            let provider = Arc::clone(&self.provider);
            let timeout =
                std::time::Duration::from_millis(self.settings.timeout_ms.saturating_add(1_000));
            let task = self.tasks.spawn(async move {
                let description = tokio::time::timeout(timeout, provider.summarize(job.input))
                    .await
                    .ok()
                    .and_then(Result::ok);
                SummaryResult {
                    incarnation: job.incarnation,
                    basis_version: job.basis_version,
                    description,
                }
            });
            self.active.insert(task.id(), pane_id);
        }
    }

    pub async fn next(&mut self) -> SummaryResult {
        loop {
            if self.tasks.is_empty() {
                std::future::pending::<()>().await;
            }
            match self.tasks.join_next_with_id().await {
                Some(Ok((id, result))) => {
                    self.active.remove(&id);
                    self.start_available();
                    return result;
                }
                Some(Err(error)) => {
                    self.active.remove(&error.id());
                    self.start_available();
                }
                None => {}
            }
        }
    }

    pub async fn shutdown(&mut self) {
        self.pending.clear();
        self.tasks.abort_all();
        while self.tasks.join_next().await.is_some() {}
        self.active.clear();
        self.provider.shutdown().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::activity_summary::SummaryError;
    use tokio::sync::{Semaphore, mpsc};

    struct BlockingProvider {
        started: mpsc::UnboundedSender<String>,
        release: Arc<Semaphore>,
    }
    #[tonic::async_trait]
    impl ActivitySummarizer for BlockingProvider {
        async fn summarize(&self, input: ActivitySummaryInput) -> Result<String, SummaryError> {
            self.started.send(input.instruction).unwrap();
            self.release.acquire().await.unwrap().forget();
            Ok("Investigating failures".into())
        }
    }
    fn job(pane: &str, basis: i64, instruction: &str) -> SummaryJob {
        SummaryJob {
            incarnation: AgentIncarnation {
                pane_id: pane.into(),
                pane_pid: 1,
                agent_pid: 2,
                agent_started_at_ms: 3,
                provider_id: "test".into(),
            },
            basis_version: EventStreamVersion::new(basis).unwrap(),
            input: ActivitySummaryInput {
                instruction: instruction.into(),
                assistant_reply: None,
            },
        }
    }
    #[tokio::test]
    async fn bounds_concurrency_and_keeps_only_latest_pending_per_pane() {
        let (started, mut calls) = mpsc::unbounded_channel();
        let release = Arc::new(Semaphore::new(0));
        let mut scheduler = ActivityScheduler::new(
            Arc::new(BlockingProvider {
                started,
                release: release.clone(),
            }),
            ActivitySummarySettings {
                max_concurrent: 1,
                max_pending: 2,
                ..Default::default()
            },
        );
        scheduler.enqueue(job("%1", 1, "first"));
        assert_eq!(calls.recv().await.unwrap(), "first");
        scheduler.enqueue(job("%1", 2, "superseded"));
        scheduler.enqueue(job("%1", 3, "latest"));
        scheduler.enqueue(job("%2", 4, "second pane"));
        assert_eq!(scheduler.pending.len(), 2);
        assert!(calls.try_recv().is_err());
        release.add_permits(1);
        assert_eq!(scheduler.next().await.basis_version.get(), 1);
        assert_eq!(calls.recv().await.unwrap(), "latest");
        assert_eq!(scheduler.tasks.len(), 1);
        scheduler.shutdown().await;
        assert!(scheduler.pending.is_empty());
        assert!(scheduler.tasks.is_empty());
    }
    #[tokio::test]
    async fn bounds_evidence_before_queueing_and_drops_departed_pending_work() {
        let (started, _) = mpsc::unbounded_channel();
        let mut scheduler = ActivityScheduler::new(
            Arc::new(BlockingProvider {
                started,
                release: Arc::new(Semaphore::new(0)),
            }),
            ActivitySummarySettings {
                max_instruction_chars: 3,
                max_reply_chars: 4,
                ..Default::default()
            },
        );
        let input = scheduler.input("日本語の依頼", Some("結果は未検証です"));
        assert_eq!(input.instruction, "日本語");
        assert_eq!(input.assistant_reply.as_deref(), Some("結果は未"));
        scheduler.pending.push_back(job("%1", 1, "discard"));
        scheduler.invalidate("%1");
        assert!(scheduler.pending.is_empty());
    }
    struct PanicOnce(std::sync::atomic::AtomicBool);
    #[tonic::async_trait]
    impl ActivitySummarizer for PanicOnce {
        async fn summarize(&self, _: ActivitySummaryInput) -> Result<String, SummaryError> {
            assert!(
                !self.0.swap(false, std::sync::atomic::Ordering::SeqCst),
                "provider panic fixture"
            );
            Ok("Recovered provider".into())
        }
    }
    #[tokio::test]
    async fn provider_panic_releases_pane_slot_and_runs_latest_pending() {
        let mut scheduler = ActivityScheduler::new(
            Arc::new(PanicOnce(std::sync::atomic::AtomicBool::new(true))),
            ActivitySummarySettings {
                max_concurrent: 1,
                ..Default::default()
            },
        );
        scheduler.enqueue(job("%1", 1, "panic"));
        scheduler.enqueue(job("%1", 2, "recover"));
        let result = tokio::time::timeout(std::time::Duration::from_millis(100), scheduler.next())
            .await
            .expect("panic must release pane slot");
        assert_eq!(result.basis_version.get(), 2);
        assert_eq!(result.description.as_deref(), Some("Recovered provider"));
    }

    #[tokio::test]
    async fn enforces_pending_capacity_while_using_configured_parallel_slots() {
        let (started, mut calls) = mpsc::unbounded_channel();
        let release = Arc::new(Semaphore::new(0));
        let mut scheduler = ActivityScheduler::new(
            Arc::new(BlockingProvider {
                started,
                release: release.clone(),
            }),
            ActivitySummarySettings {
                max_concurrent: 2,
                max_pending: 2,
                ..Default::default()
            },
        );
        scheduler.enqueue(job("%1", 1, "active one"));
        scheduler.enqueue(job("%2", 2, "active two"));
        let mut active = vec![calls.recv().await.unwrap(), calls.recv().await.unwrap()];
        active.sort();
        assert_eq!(active, ["active one", "active two"]);
        scheduler.enqueue(job("%3", 3, "oldest pending"));
        scheduler.enqueue(job("%4", 4, "pending four"));
        scheduler.enqueue(job("%5", 5, "pending five"));
        assert_eq!(scheduler.pending.len(), 2);
        assert_eq!(scheduler.pending.front().unwrap().incarnation.pane_id, "%4");
        assert_eq!(scheduler.tasks.len(), 2);
        assert!(calls.try_recv().is_err());
        scheduler.shutdown().await;
    }
}
