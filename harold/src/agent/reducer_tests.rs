use events::EventStreamVersion;

use super::domain::{
    AgentEvent, AgentIncarnation, AgentLifecycleObserved, AgentPaneDeparted, AgentPaneObservation,
    AgentPaneObserved, AgentScreenObserved, EffectiveAgentState, ObservedAgentState,
    ProjectionChange, WorkSummaryUpdate,
};
use super::reducer::{DEFAULT_HOOK_GRACE_MS, reduce_agent_event};

fn version(value: i64) -> EventStreamVersion {
    EventStreamVersion::new(value).unwrap()
}

fn incarnation(agent_started_at_ms: i64) -> AgentIncarnation {
    AgentIncarnation {
        pane_id: "%7".into(),
        pane_pid: 10,
        agent_pid: 20,
        agent_started_at_ms,
        provider_id: "codex".into(),
    }
}

fn pane(agent_started_at_ms: i64, observed_at_ms: i64) -> AgentPaneObservation {
    AgentPaneObservation {
        incarnation: incarnation(agent_started_at_ms),
        tmux_target: "harold:2.1".into(),
        session_name: "harold".into(),
        window_index: 2,
        pane_index: 1,
        working_directory: "/work/harold".into(),
        provider_display_name: "Codex".into(),
        observed_at_ms,
    }
}

fn upsert(change: ProjectionChange) -> super::domain::AgentPaneProjection {
    match change {
        ProjectionChange::Upsert(projection) => projection,
        ProjectionChange::Remove(_) | ProjectionChange::Ignore => panic!("expected pane upsert"),
    }
}

fn apply(
    current: Option<super::domain::AgentPaneProjection>,
    event: AgentEvent,
    event_version: i64,
) -> super::domain::AgentPaneProjection {
    upsert(reduce_agent_event(
        current,
        &event,
        version(event_version),
        DEFAULT_HOOK_GRACE_MS,
    ))
}

#[test]
fn pane_observation_starts_an_unknown_projection() {
    let projection = apply(
        None,
        AgentEvent::PaneObserved(AgentPaneObserved {
            pane: pane(1_000, 100),
        }),
        1,
    );

    assert_eq!(projection.pane.incarnation, incarnation(1_000));
    assert_eq!(projection.effective_state, EffectiveAgentState::Unknown);
    assert_eq!(projection.work_summary, None);
    assert_eq!(projection.last_transition_at_ms, 100);
    assert_eq!(projection.last_event_version, version(1));
}

#[test]
fn hook_wins_during_grace_then_later_screen_repairs_state() {
    let initial = apply(
        None,
        AgentEvent::PaneObserved(AgentPaneObserved {
            pane: pane(1_000, 100),
        }),
        1,
    );
    let hooked = apply(
        Some(initial),
        AgentEvent::LifecycleObserved(AgentLifecycleObserved {
            incarnation: incarnation(1_000),
            state: ObservedAgentState::Busy,
            adapter_id: "hook-v1".into(),
            work_summary: WorkSummaryUpdate::Unchanged,
            observed_at_ms: 200,
        }),
        2,
    );
    let during_grace = apply(
        Some(hooked),
        AgentEvent::ScreenObserved(AgentScreenObserved {
            incarnation: incarnation(1_000),
            state: Some(ObservedAgentState::Idle),
            classifier_id: "screen-v1".into(),
            fallback_summary: None,
            observed_at_ms: 201,
        }),
        3,
    );
    let repaired = apply(
        Some(during_grace),
        AgentEvent::ScreenObserved(AgentScreenObserved {
            incarnation: incarnation(1_000),
            state: Some(ObservedAgentState::Idle),
            classifier_id: "screen-v1".into(),
            fallback_summary: None,
            observed_at_ms: 2_200,
        }),
        4,
    );

    assert_eq!(repaired.hook_state, Some(ObservedAgentState::Busy));
    assert_eq!(repaired.effective_state, EffectiveAgentState::Idle);
    assert_eq!(repaired.last_transition_at_ms, 2_200);
}

#[test]
fn explicit_summary_set_unchanged_and_clear_select_the_correct_candidate() {
    let initial = apply(
        None,
        AgentEvent::PaneObserved(AgentPaneObserved {
            pane: pane(1_000, 100),
        }),
        1,
    );
    let fallback = apply(
        Some(initial),
        AgentEvent::ScreenObserved(AgentScreenObserved {
            incarnation: incarnation(1_000),
            state: None,
            classifier_id: "screen-v1".into(),
            fallback_summary: Some("Review tests".into()),
            observed_at_ms: 110,
        }),
        2,
    );
    let unchanged = apply(
        Some(fallback),
        AgentEvent::LifecycleObserved(AgentLifecycleObserved {
            incarnation: incarnation(1_000),
            state: ObservedAgentState::Busy,
            adapter_id: "hook-v1".into(),
            work_summary: WorkSummaryUpdate::Unchanged,
            observed_at_ms: 120,
        }),
        3,
    );
    let explicit = apply(
        Some(unchanged.clone()),
        AgentEvent::LifecycleObserved(AgentLifecycleObserved {
            incarnation: incarnation(1_000),
            state: ObservedAgentState::Busy,
            adapter_id: "hook-v1".into(),
            work_summary: WorkSummaryUpdate::Set("Fix projector".into()),
            observed_at_ms: 130,
        }),
        4,
    );
    let cleared = apply(
        Some(explicit.clone()),
        AgentEvent::LifecycleObserved(AgentLifecycleObserved {
            incarnation: incarnation(1_000),
            state: ObservedAgentState::Idle,
            adapter_id: "hook-v1".into(),
            work_summary: WorkSummaryUpdate::Clear,
            observed_at_ms: 140,
        }),
        5,
    );

    assert_eq!(unchanged.work_summary.as_deref(), Some("Review tests"));
    assert_eq!(explicit.work_summary.as_deref(), Some("Fix projector"));
    assert_eq!(explicit.explicit_work_summary_updated_at_ms, Some(130));
    assert_eq!(cleared.explicit_work_summary, None);
    assert_eq!(cleared.explicit_work_summary_updated_at_ms, None);
    assert_eq!(cleared.work_summary.as_deref(), Some("Review tests"));
}

#[test]
fn clear_without_a_fallback_makes_the_effective_summary_absent() {
    let initial = apply(
        None,
        AgentEvent::PaneObserved(AgentPaneObserved {
            pane: pane(1_000, 100),
        }),
        1,
    );
    let explicit = apply(
        Some(initial),
        AgentEvent::LifecycleObserved(AgentLifecycleObserved {
            incarnation: incarnation(1_000),
            state: ObservedAgentState::Busy,
            adapter_id: "hook-v1".into(),
            work_summary: WorkSummaryUpdate::Set("Fix projector".into()),
            observed_at_ms: 120,
        }),
        2,
    );
    let cleared = apply(
        Some(explicit),
        AgentEvent::LifecycleObserved(AgentLifecycleObserved {
            incarnation: incarnation(1_000),
            state: ObservedAgentState::Idle,
            adapter_id: "hook-v1".into(),
            work_summary: WorkSummaryUpdate::Clear,
            observed_at_ms: 130,
        }),
        3,
    );

    assert_eq!(cleared.work_summary, None);
}

#[test]
fn screen_facts_update_independently_and_absence_preserves_candidates() {
    let initial = apply(
        None,
        AgentEvent::PaneObserved(AgentPaneObserved {
            pane: pane(1_000, 100),
        }),
        1,
    );
    let summary_only = apply(
        Some(initial),
        AgentEvent::ScreenObserved(AgentScreenObserved {
            incarnation: incarnation(1_000),
            state: None,
            classifier_id: "screen-v1".into(),
            fallback_summary: Some("Review tests".into()),
            observed_at_ms: 110,
        }),
        2,
    );
    let state_only = apply(
        Some(summary_only.clone()),
        AgentEvent::ScreenObserved(AgentScreenObserved {
            incarnation: incarnation(1_000),
            state: Some(ObservedAgentState::Idle),
            classifier_id: "screen-v1".into(),
            fallback_summary: None,
            observed_at_ms: 120,
        }),
        3,
    );
    let both = apply(
        Some(state_only.clone()),
        AgentEvent::ScreenObserved(AgentScreenObserved {
            incarnation: incarnation(1_000),
            state: Some(ObservedAgentState::Busy),
            classifier_id: "screen-v2".into(),
            fallback_summary: Some("Ship release".into()),
            observed_at_ms: 130,
        }),
        4,
    );

    assert_eq!(summary_only.effective_state, EffectiveAgentState::Unknown);
    assert_eq!(
        state_only.screen_work_summary.as_deref(),
        Some("Review tests")
    );
    assert_eq!(state_only.effective_state, EffectiveAgentState::Idle);
    assert_eq!(both.screen_state, Some(ObservedAgentState::Busy));
    assert_eq!(both.screen_work_summary.as_deref(), Some("Ship release"));
    assert_eq!(
        reduce_agent_event(
            Some(both),
            &AgentEvent::ScreenObserved(AgentScreenObserved {
                incarnation: incarnation(1_000),
                state: None,
                classifier_id: "screen-v2".into(),
                fallback_summary: None,
                observed_at_ms: 140,
            }),
            version(5),
            DEFAULT_HOOK_GRACE_MS,
        ),
        ProjectionChange::Ignore
    );
}

#[test]
fn repeated_screen_candidate_does_not_refresh_its_timestamp_or_transition() {
    let initial = apply(
        None,
        AgentEvent::PaneObserved(AgentPaneObserved {
            pane: pane(1_000, 100),
        }),
        1,
    );
    let observed = apply(
        Some(initial),
        AgentEvent::ScreenObserved(AgentScreenObserved {
            incarnation: incarnation(1_000),
            state: Some(ObservedAgentState::Busy),
            classifier_id: "screen-v1".into(),
            fallback_summary: Some("Review tests".into()),
            observed_at_ms: 110,
        }),
        2,
    );
    let duplicate = apply(
        Some(observed),
        AgentEvent::ScreenObserved(AgentScreenObserved {
            incarnation: incarnation(1_000),
            state: Some(ObservedAgentState::Busy),
            classifier_id: "screen-v1".into(),
            fallback_summary: Some("Review tests".into()),
            observed_at_ms: 120,
        }),
        3,
    );

    assert_eq!(duplicate.screen_work_summary_updated_at_ms, Some(110));
    assert_eq!(duplicate.last_transition_at_ms, 110);
    assert_eq!(duplicate.last_event_version, version(3));
}

#[test]
fn replacement_clears_evidence_and_delayed_old_incarnation_events_are_ignored() {
    let original = apply(
        None,
        AgentEvent::PaneObserved(AgentPaneObserved {
            pane: pane(1_000, 100),
        }),
        1,
    );
    let busy = apply(
        Some(original),
        AgentEvent::LifecycleObserved(AgentLifecycleObserved {
            incarnation: incarnation(1_000),
            state: ObservedAgentState::Busy,
            adapter_id: "hook-v1".into(),
            work_summary: WorkSummaryUpdate::Set("Old work".into()),
            observed_at_ms: 110,
        }),
        2,
    );
    let replacement = apply(
        Some(busy),
        AgentEvent::PaneObserved(AgentPaneObserved {
            pane: pane(2_000, 120),
        }),
        3,
    );

    assert_eq!(replacement.effective_state, EffectiveAgentState::Unknown);
    assert_eq!(replacement.work_summary, None);
    assert_eq!(
        reduce_agent_event(
            Some(replacement),
            &AgentEvent::LifecycleObserved(AgentLifecycleObserved {
                incarnation: incarnation(1_000),
                state: ObservedAgentState::Idle,
                adapter_id: "hook-v1".into(),
                work_summary: WorkSummaryUpdate::Set("Stale work".into()),
                observed_at_ms: 130,
            }),
            version(4),
            DEFAULT_HOOK_GRACE_MS,
        ),
        ProjectionChange::Ignore
    );
}

#[test]
fn only_a_matching_departure_removes_the_current_incarnation() {
    let current = apply(
        None,
        AgentEvent::PaneObserved(AgentPaneObserved {
            pane: pane(2_000, 100),
        }),
        1,
    );

    assert_eq!(
        reduce_agent_event(
            Some(current.clone()),
            &AgentEvent::PaneDeparted(AgentPaneDeparted {
                incarnation: incarnation(1_000),
                observed_at_ms: 110,
            }),
            version(2),
            DEFAULT_HOOK_GRACE_MS,
        ),
        ProjectionChange::Ignore
    );
    assert_eq!(
        reduce_agent_event(
            Some(current),
            &AgentEvent::PaneDeparted(AgentPaneDeparted {
                incarnation: incarnation(2_000),
                observed_at_ms: 120,
            }),
            version(3),
            DEFAULT_HOOK_GRACE_MS,
        ),
        ProjectionChange::Remove(incarnation(2_000))
    );
}
