#![allow(
    dead_code,
    reason = "the projector consumes this pure reducer in the next monitor slice"
)]

use events::EventStreamVersion;

use super::domain::{
    AgentEvent, AgentIncarnation, AgentPaneProjection, EffectiveAgentState, ObservedAgentState,
    ProjectionChange, WorkSummaryUpdate,
};

pub(crate) const DEFAULT_HOOK_GRACE_MS: u64 = 2_000;
pub(crate) const DEFAULT_REPORT_GRACE_MS: u64 = 30_000;

/// How long a state report stands against the screen reading.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GraceWindows {
    /// For a turn-complete on a pane no lifecycle adapter reports on.
    pub hook_ms: u64,
    /// For a pane a lifecycle adapter reports on.
    pub report_ms: u64,
}

impl GraceWindows {
    #[cfg(test)]
    pub(crate) const fn same(ms: u64) -> Self {
        Self {
            hook_ms: ms,
            report_ms: ms,
        }
    }

    pub(crate) fn for_pane(self, has_lifecycle_adapter: bool) -> i64 {
        let ms = if has_lifecycle_adapter {
            self.report_ms
        } else {
            self.hook_ms
        };
        i64::try_from(ms).unwrap_or(i64::MAX)
    }
}

impl Default for GraceWindows {
    fn default() -> Self {
        Self {
            hook_ms: DEFAULT_HOOK_GRACE_MS,
            report_ms: DEFAULT_REPORT_GRACE_MS,
        }
    }
}

/// This classifier certifies a new submitted occurrence, including identical repeated text.
pub(crate) const SUBMITTED_PROMPT_CLASSIFIER_ID: &str = "tmux-submitted-prompt-v1";

pub(crate) fn reduce_agent_event(
    current: Option<AgentPaneProjection>,
    event: &AgentEvent,
    event_version: EventStreamVersion,
    graces: GraceWindows,
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

            // The same adapter repeating the pane's state, with no summary, says only that
            // it is still there. It restarts the grace and is not new work.
            let is_repeat = projection.hook_state == Some(lifecycle.state)
                && projection.lifecycle_adapter_id.as_deref()
                    == Some(lifecycle.adapter_id.as_str())
                && lifecycle.work_summary == WorkSummaryUpdate::Unchanged;
            if !is_repeat {
                invalidate_generated(&mut projection, event_version);
            }
            projection.lifecycle_adapter_id = Some(lifecycle.adapter_id.clone());
            set_hook_state(&mut projection, lifecycle.state, lifecycle.observed_at_ms);
            apply_work_summary(
                &mut projection,
                &lifecycle.work_summary,
                lifecycle.observed_at_ms,
            );
            ProjectionChange::Upsert(reconcile(
                projection,
                lifecycle.observed_at_ms,
                event_version,
                graces,
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

            if screen_changes_activity(
                projection.effective_state,
                projection.screen_work_summary.as_deref(),
                screen,
            ) {
                invalidate_generated(&mut projection, event_version);
            }
            if let Some(state) = screen.state {
                let state_needs_revalidation = projection.screen_state != Some(state)
                    || projection.effective_state != observed_to_effective(state)
                    || revalidates_hook_epoch(&projection, screen.observed_at_ms, graces);
                if state_needs_revalidation {
                    projection.screen_state = Some(state);
                    projection.screen_classifier_id = Some(screen.classifier_id.clone());
                    projection.screen_observed_at_ms = Some(screen.observed_at_ms);
                }
            }
            if let Some(summary) = &screen.fallback_summary
                && (projection.screen_work_summary.as_deref() != Some(summary)
                    || screen.classifier_id == SUBMITTED_PROMPT_CLASSIFIER_ID)
            {
                projection.screen_work_summary = Some(summary.clone());
                projection.screen_work_summary_updated_at_ms = Some(screen.observed_at_ms);
            }
            ProjectionChange::Upsert(reconcile(
                projection,
                screen.observed_at_ms,
                event_version,
                graces,
            ))
        }
        AgentEvent::ActivitySummaryGenerated(generated) => {
            let Some(mut projection) = current else {
                return ProjectionChange::Ignore;
            };
            if projection.pane.incarnation != generated.incarnation
                || projection.summary_basis_version != generated.basis_version
            {
                return ProjectionChange::Ignore;
            }
            let Some(description) = super::summary::normalize_work_summary(&generated.description)
            else {
                return ProjectionChange::Ignore;
            };
            projection.generated_work_summary = Some(description.clone());
            projection.generated_summary_basis_version = Some(generated.basis_version);
            projection.work_summary = Some(description);
            projection.last_event_version = event_version;
            ProjectionChange::Upsert(projection)
        }
        AgentEvent::MonitorHealthChanged(_) => ProjectionChange::Ignore,
    }
}

fn apply_work_summary(
    projection: &mut AgentPaneProjection,
    update: &WorkSummaryUpdate,
    observed_at_ms: i64,
) {
    match update {
        WorkSummaryUpdate::Unchanged => {}
        WorkSummaryUpdate::Clear => {
            projection.explicit_work_summary = None;
            projection.explicit_work_summary_updated_at_ms = None;
        }
        WorkSummaryUpdate::Set(summary) => {
            projection.explicit_work_summary = Some(summary.clone());
            projection.explicit_work_summary_updated_at_ms = Some(observed_at_ms);
        }
    }
}

/// A report replaces the hook state and starts a new epoch for the screen reading.
fn set_hook_state(
    projection: &mut AgentPaneProjection,
    state: ObservedAgentState,
    observed_at_ms: i64,
) {
    projection.hook_state = Some(state);
    projection.hook_observed_at_ms = Some(observed_at_ms);
    projection.screen_state = None;
    projection.screen_classifier_id = None;
    projection.screen_observed_at_ms = None;
}

/// A turn-complete from the Stop hook. It sets the pane idle only while no lifecycle adapter
/// has reported for this agent process; the work-summary update applies either way.
pub(crate) fn reduce_turn_completed(
    current: Option<AgentPaneProjection>,
    incarnation: &AgentIncarnation,
    work_summary: &WorkSummaryUpdate,
    observed_at_ms: i64,
    event_version: EventStreamVersion,
    graces: GraceWindows,
) -> ProjectionChange {
    let Some(mut projection) = current else {
        return ProjectionChange::Ignore;
    };
    if projection.pane.incarnation != *incarnation {
        return ProjectionChange::Ignore;
    }

    invalidate_generated(&mut projection, event_version);
    if projection.lifecycle_adapter_id.is_none() {
        set_hook_state(&mut projection, ObservedAgentState::Idle, observed_at_ms);
    }
    apply_work_summary(&mut projection, work_summary, observed_at_ms);
    ProjectionChange::Upsert(reconcile(projection, observed_at_ms, event_version, graces))
}

fn revalidates_hook_epoch(
    projection: &AgentPaneProjection,
    screen_observed_at_ms: i64,
    graces: GraceWindows,
) -> bool {
    let Some(hook_observed_at_ms) = projection.hook_observed_at_ms else {
        return false;
    };
    let grace_ms = graces.for_pane(projection.lifecycle_adapter_id.is_some());
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
        lifecycle_adapter_id: None,
        screen_state: None,
        screen_classifier_id: None,
        screen_observed_at_ms: None,
        effective_state: EffectiveAgentState::Unknown,
        explicit_work_summary: None,
        explicit_work_summary_updated_at_ms: None,
        screen_work_summary: None,
        screen_work_summary_updated_at_ms: None,
        summary_basis_version: event_version,
        generated_work_summary: None,
        generated_summary_basis_version: None,
        work_summary: None,
        last_transition_at_ms: observed.pane.observed_at_ms,
        last_event_version: event_version,
    }
}

fn reconcile(
    mut projection: AgentPaneProjection,
    observed_at_ms: i64,
    event_version: EventStreamVersion,
    graces: GraceWindows,
) -> AgentPaneProjection {
    let effective_state = effective_state(&projection, graces);
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
    if projection.generated_summary_basis_version == Some(projection.summary_basis_version)
        && let Some(generated) = &projection.generated_work_summary
    {
        projection.work_summary = Some(generated.clone());
    }
    projection.last_event_version = event_version;
    projection
}

fn effective_state(projection: &AgentPaneProjection, graces: GraceWindows) -> EffectiveAgentState {
    let Some(hook_state) = projection.hook_state else {
        return projection
            .screen_state
            .map_or(EffectiveAgentState::Unknown, observed_to_effective);
    };
    let hook_observed_at_ms = projection.hook_observed_at_ms.unwrap_or(i64::MAX);
    let grace_ms = graces.for_pane(projection.lifecycle_adapter_id.is_some());
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

fn invalidate_generated(projection: &mut AgentPaneProjection, event_version: EventStreamVersion) {
    projection.summary_basis_version = event_version;
    projection.generated_work_summary = None;
    projection.generated_summary_basis_version = None;
}

/// A corroborating Busy observation does not denote another submitted turn.
pub(crate) fn screen_changes_activity(
    previous_state: EffectiveAgentState,
    previous_summary: Option<&str>,
    screen: &super::domain::AgentScreenObserved,
) -> bool {
    (screen.classifier_id == SUBMITTED_PROMPT_CLASSIFIER_ID && screen.fallback_summary.is_some())
        || screen
            .fallback_summary
            .as_deref()
            .is_some_and(|summary| previous_summary != Some(summary))
        || (screen.state == Some(super::domain::ObservedAgentState::Busy)
            && previous_state != EffectiveAgentState::Busy)
}
