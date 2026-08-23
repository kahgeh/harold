#![allow(
    dead_code,
    reason = "the projector consumes this pure reducer in the next monitor slice"
)]

use events::EventStreamVersion;

use super::domain::{
    AgentEvent, AgentPaneProjection, EffectiveAgentState, ProjectionChange, WorkSummaryUpdate,
};

pub(crate) const DEFAULT_HOOK_GRACE_MS: u64 = 2_000;

pub(crate) fn reduce_agent_event(
    current: Option<AgentPaneProjection>,
    event: &AgentEvent,
    event_version: EventStreamVersion,
    hook_grace_ms: u64,
) -> ProjectionChange {
    match event {
        AgentEvent::PaneObserved(observed) => {
            reduce_pane_observed(current, observed, event_version)
        }
        AgentEvent::PaneDeparted(departed) => match current {
            Some(current) if current.pane.incarnation == departed.incarnation => {
                ProjectionChange::Remove(departed.incarnation.clone())
            }
            Some(_) | None => ProjectionChange::Ignore,
        },
        AgentEvent::LifecycleObserved(lifecycle) => {
            let Some(mut projection) = current else {
                return ProjectionChange::Ignore;
            };
            if projection.pane.incarnation != lifecycle.incarnation {
                return ProjectionChange::Ignore;
            }

            projection.hook_state = Some(lifecycle.state);
            projection.hook_observed_at_ms = Some(lifecycle.observed_at_ms);
            projection.screen_state = None;
            projection.screen_classifier_id = None;
            projection.screen_observed_at_ms = None;
            match &lifecycle.work_summary {
                WorkSummaryUpdate::Unchanged => {}
                WorkSummaryUpdate::Clear => {
                    projection.explicit_work_summary = None;
                    projection.explicit_work_summary_updated_at_ms = None;
                }
                WorkSummaryUpdate::Set(summary) => {
                    projection.explicit_work_summary = Some(summary.clone());
                    projection.explicit_work_summary_updated_at_ms = Some(lifecycle.observed_at_ms);
                }
            }
            ProjectionChange::Upsert(reconcile(
                projection,
                lifecycle.observed_at_ms,
                event_version,
                hook_grace_ms,
            ))
        }
        AgentEvent::ScreenObserved(screen) => {
            if screen.state.is_none() && screen.fallback_summary.is_none() {
                return ProjectionChange::Ignore;
            }
            let Some(mut projection) = current else {
                return ProjectionChange::Ignore;
            };
            if projection.pane.incarnation != screen.incarnation {
                return ProjectionChange::Ignore;
            }

            if let Some(state) = screen.state {
                let state_needs_revalidation = projection.screen_state != Some(state)
                    || projection.effective_state != observed_to_effective(state)
                    || revalidates_hook_epoch(&projection, screen.observed_at_ms, hook_grace_ms);
                if state_needs_revalidation {
                    projection.screen_state = Some(state);
                    projection.screen_classifier_id = Some(screen.classifier_id.clone());
                    projection.screen_observed_at_ms = Some(screen.observed_at_ms);
                }
            }
            if let Some(summary) = &screen.fallback_summary
                && projection.screen_work_summary.as_deref() != Some(summary)
            {
                projection.screen_work_summary = Some(summary.clone());
                projection.screen_work_summary_updated_at_ms = Some(screen.observed_at_ms);
            }
            ProjectionChange::Upsert(reconcile(
                projection,
                screen.observed_at_ms,
                event_version,
                hook_grace_ms,
            ))
        }
        AgentEvent::MonitorHealthChanged(_) => ProjectionChange::Ignore,
    }
}

fn revalidates_hook_epoch(
    projection: &AgentPaneProjection,
    screen_observed_at_ms: i64,
    hook_grace_ms: u64,
) -> bool {
    let Some(hook_observed_at_ms) = projection.hook_observed_at_ms else {
        return false;
    };
    let grace_ms = i64::try_from(hook_grace_ms).unwrap_or(i64::MAX);
    let grace_ends_at_ms = hook_observed_at_ms.saturating_add(grace_ms);
    screen_observed_at_ms >= grace_ends_at_ms
        && projection
            .screen_observed_at_ms
            .is_none_or(|previous| previous < grace_ends_at_ms)
}

fn reduce_pane_observed(
    current: Option<AgentPaneProjection>,
    observed: &super::domain::AgentPaneObserved,
    event_version: EventStreamVersion,
) -> ProjectionChange {
    let Some(mut current) = current else {
        return ProjectionChange::Upsert(new_projection(observed, event_version));
    };
    if current.pane.incarnation.pane_id != observed.pane.incarnation.pane_id {
        return ProjectionChange::Ignore;
    }
    if current.pane.incarnation != observed.pane.incarnation {
        return ProjectionChange::Upsert(new_projection(observed, event_version));
    }

    current.pane = observed.pane.clone();
    current.last_event_version = event_version;
    ProjectionChange::Upsert(current)
}

fn new_projection(
    observed: &super::domain::AgentPaneObserved,
    event_version: EventStreamVersion,
) -> AgentPaneProjection {
    AgentPaneProjection {
        pane: observed.pane.clone(),
        hook_state: None,
        hook_observed_at_ms: None,
        screen_state: None,
        screen_classifier_id: None,
        screen_observed_at_ms: None,
        effective_state: EffectiveAgentState::Unknown,
        explicit_work_summary: None,
        explicit_work_summary_updated_at_ms: None,
        screen_work_summary: None,
        screen_work_summary_updated_at_ms: None,
        work_summary: None,
        last_transition_at_ms: observed.pane.observed_at_ms,
        last_event_version: event_version,
    }
}

fn reconcile(
    mut projection: AgentPaneProjection,
    observed_at_ms: i64,
    event_version: EventStreamVersion,
    hook_grace_ms: u64,
) -> AgentPaneProjection {
    let effective_state = effective_state(&projection, hook_grace_ms);
    if projection.effective_state != effective_state {
        projection.effective_state = effective_state;
        projection.last_transition_at_ms = observed_at_ms;
    }
    let explicit_summary = projection
        .explicit_work_summary
        .as_ref()
        .zip(projection.explicit_work_summary_updated_at_ms);
    let screen_summary = projection
        .screen_work_summary
        .as_ref()
        .zip(projection.screen_work_summary_updated_at_ms);
    projection.work_summary = match (explicit_summary, screen_summary) {
        (Some((_, explicit_at_ms)), Some((screen, screen_at_ms)))
            if screen_at_ms > explicit_at_ms =>
        {
            Some(screen.clone())
        }
        (Some((explicit, _)), _) => Some(explicit.clone()),
        (None, Some((screen, _))) => Some(screen.clone()),
        (None, None) => None,
    };
    projection.last_event_version = event_version;
    projection
}

fn effective_state(projection: &AgentPaneProjection, hook_grace_ms: u64) -> EffectiveAgentState {
    let Some(hook_state) = projection.hook_state else {
        return projection
            .screen_state
            .map_or(EffectiveAgentState::Unknown, observed_to_effective);
    };
    let hook_observed_at_ms = projection.hook_observed_at_ms.unwrap_or(i64::MAX);
    let grace_ms = i64::try_from(hook_grace_ms).unwrap_or(i64::MAX);
    let hook_grace_ends_at_ms = hook_observed_at_ms.saturating_add(grace_ms);
    if let (Some(screen_state), Some(screen_observed_at_ms)) =
        (projection.screen_state, projection.screen_observed_at_ms)
        && screen_observed_at_ms >= hook_grace_ends_at_ms
    {
        return observed_to_effective(screen_state);
    }
    observed_to_effective(hook_state)
}

fn observed_to_effective(state: super::domain::ObservedAgentState) -> EffectiveAgentState {
    match state {
        super::domain::ObservedAgentState::Busy => EffectiveAgentState::Busy,
        super::domain::ObservedAgentState::Idle => EffectiveAgentState::Idle,
    }
}
