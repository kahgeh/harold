use super::*;
use crate::activity_summary::SummaryError;

fn job(pane: &str, basis: u64, instruction: &str) -> SummaryJob {
    SummaryJob {
        incarnation: AgentIncarnation {
            pane_id: pane.into(),
            pane_pid: 1,
            agent_pid: 2,
            agent_started_at_ms: 3,
            provider_id: "test".into(),
        },
        basis_version: EventVersion::new(basis).unwrap(),
        input: ActivitySummaryInput {
            instruction: instruction.into(),
            assistant_reply: None,
        },
    }
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
#[tokio::test(start_paused = true)]
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
