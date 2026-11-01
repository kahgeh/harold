use crate::store::EventVersion;

use super::domain::{
    AgentIncarnation, AgentPaneObservation, AgentPaneProjection, AgentSnapshot, EffectiveAgentState,
};
use super::snapshot::AgentSnapshotHub;

fn snapshot(revision: i64, summary: Option<&str>) -> AgentSnapshot {
    AgentSnapshot {
        tts_muted: false,
        messaging_paused: false,
        through_event_version: EventVersion::from_column(revision).expect("valid revision"),
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
            lifecycle_adapter_id: None,
            screen_state: None,
            screen_classifier_id: None,
            screen_observed_at_ms: None,
            effective_state: EffectiveAgentState::Working,
            explicit_work_summary: summary.map(str::to_string),
            explicit_work_summary_updated_at_ms: summary.map(|_| 9_600),
            screen_work_summary: None,
            screen_work_summary_updated_at_ms: None,
            summary_basis_version: EventVersion::start(),
            generated_work_summary: None,
            generated_summary_basis_version: None,
            work_summary: summary.map(str::to_string),
            last_transition_at_ms: 9_700,
            last_event_version: EventVersion::from_column(revision).expect("valid revision"),
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

fn timed_snapshot(
    revision: i64,
    provider_id: &str,
    panes: &[(&str, EffectiveAgentState, i64)],
) -> AgentSnapshot {
    let mut base = snapshot(revision, None);
    let template = base.panes[0].clone();
    base.panes = panes
        .iter()
        .map(|(pane_id, state, transition_ms)| {
            let mut pane = template.clone();
            pane.pane.incarnation.pane_id = (*pane_id).into();
            pane.pane.incarnation.provider_id = provider_id.into();
            pane.effective_state = *state;
            pane.last_transition_at_ms = *transition_ms;
            pane
        })
        .collect();
    base
}

fn seconds_in(provider_id: &str, state: &str) -> Option<f64> {
    crate::telemetry::test_support::histogram_sum(
        "harold.agent.state.duration",
        &[("provider_id", provider_id), ("state", state)],
    )
}

#[test]
fn a_state_change_records_the_seconds_in_the_state_left() {
    let provider = "state-change-provider";
    let hub = AgentSnapshotHub::new(timed_snapshot(
        1,
        provider,
        &[("%1", EffectiveAgentState::Working, 10_000)],
    ));

    hub.publish_committed(timed_snapshot(
        2,
        provider,
        &[("%1", EffectiveAgentState::Idle, 100_000)],
    ));

    assert_eq!(seconds_in(provider, "working"), Some(90.0));
    assert_eq!(seconds_in(provider, "idle"), None);
}

#[test]
fn a_new_a_departed_and_an_unchanged_pane_record_nothing() {
    let provider = "quiet-provider";
    let hub = AgentSnapshotHub::new(timed_snapshot(
        1,
        provider,
        &[
            ("%1", EffectiveAgentState::Working, 10_000),
            ("%2", EffectiveAgentState::Idle, 20_000),
        ],
    ));

    hub.publish_committed(timed_snapshot(
        2,
        provider,
        &[
            ("%1", EffectiveAgentState::Working, 10_000),
            ("%3", EffectiveAgentState::Working, 50_000),
        ],
    ));

    for state in ["working", "idle", "unknown"] {
        assert_eq!(seconds_in(provider, state), None, "{state}");
    }
}

#[test]
fn a_rejected_stale_snapshot_records_nothing() {
    let provider = "stale-provider";
    let hub = AgentSnapshotHub::new(timed_snapshot(
        5,
        provider,
        &[("%1", EffectiveAgentState::Working, 10_000)],
    ));

    hub.publish_committed(timed_snapshot(
        5,
        provider,
        &[("%1", EffectiveAgentState::Idle, 100_000)],
    ));
    hub.publish_committed(timed_snapshot(
        4,
        provider,
        &[("%1", EffectiveAgentState::Idle, 100_000)],
    ));

    assert_eq!(seconds_in(provider, "working"), None);
}
