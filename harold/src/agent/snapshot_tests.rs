use events::EventStreamVersion;

use super::domain::{
    AgentIncarnation, AgentPaneObservation, AgentPaneProjection, AgentSnapshot, EffectiveAgentState,
};
use super::snapshot::AgentSnapshotHub;

fn snapshot(revision: i64, summary: Option<&str>) -> AgentSnapshot {
    AgentSnapshot {
        through_event_version: EventStreamVersion::new(revision).expect("valid revision"),
        server_time_ms: 10_000 + revision,
        monitor_health: Vec::new(),
        panes: vec![AgentPaneProjection {
            pane: AgentPaneObservation {
                incarnation: AgentIncarnation {
                    pane_id: "%9".into(),
                    pane_pid: 91,
                    agent_pid: 92,
                    agent_started_at_ms: 9_000,
                    provider_id: "codex".into(),
                },
                tmux_target: "harold:1.2".into(),
                session_name: "harold".into(),
                window_index: 1,
                pane_index: 2,
                working_directory: "/work/harold".into(),
                provider_display_name: "Codex".into(),
                observed_at_ms: 9_500,
            },
            hook_state: None,
            hook_observed_at_ms: None,
            screen_state: None,
            screen_classifier_id: None,
            screen_observed_at_ms: None,
            effective_state: EffectiveAgentState::Busy,
            explicit_work_summary: summary.map(str::to_string),
            explicit_work_summary_updated_at_ms: summary.map(|_| 9_600),
            screen_work_summary: None,
            screen_work_summary_updated_at_ms: None,
            work_summary: summary.map(str::to_string),
            last_transition_at_ms: 9_700,
            last_event_version: EventStreamVersion::new(revision).expect("valid revision"),
        }],
    }
}

#[test]
fn hub_retains_db_seed_with_no_external_subscribers() {
    let hub = AgentSnapshotHub::new(snapshot(4, Some("review projector")));

    let receiver = hub.subscribe();

    assert_eq!(receiver.borrow().through_event_version.get(), 4);
    assert_eq!(
        receiver.borrow().panes[0].work_summary.as_deref(),
        Some("review projector")
    );
}

#[tokio::test]
async fn publish_requires_a_higher_durable_revision_and_coalesces_slow_receivers() {
    let hub = AgentSnapshotHub::new(snapshot(4, None));
    let mut receiver = hub.subscribe();

    hub.publish_committed(snapshot(5, Some("first")));
    hub.publish_committed(snapshot(6, Some("current")));
    hub.publish_committed(snapshot(5, Some("stale")));

    receiver.changed().await.expect("published snapshot");
    assert_eq!(receiver.borrow_and_update().through_event_version.get(), 6);
    assert_eq!(
        receiver.borrow().panes[0].work_summary.as_deref(),
        Some("current")
    );
    assert!(receiver.has_changed().is_ok_and(|changed| !changed));
}
