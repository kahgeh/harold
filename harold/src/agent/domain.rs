#![allow(
    dead_code,
    reason = "domain types are consumed by later monitor slices"
)]

use crate::store::EventVersion;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum WorkSummaryUpdate {
    #[default]
    Unchanged,
    Clear,
    Set(String),
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum CompletionSummaryUpdate {
    #[default]
    Unchanged,
    Set(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ObservedAgentState {
    #[serde(alias = "Busy")]
    Working,
    Blocked,
    Done,
    Idle,
    Error,
}

impl ObservedAgentState {
    /// True while the agent works. A blocked agent waits inside its turn, so it counts.
    pub(crate) fn is_working(self) -> bool {
        matches!(self, Self::Working | Self::Blocked)
    }

    /// True while the agent rests.
    pub(crate) fn is_at_rest(self) -> bool {
        matches!(self, Self::Done | Self::Idle | Self::Error)
    }

    /// The one map from an observed state to the effective state.
    pub(crate) fn effective(self) -> EffectiveAgentState {
        match self {
            Self::Working => EffectiveAgentState::Working,
            Self::Blocked => EffectiveAgentState::Blocked,
            Self::Done => EffectiveAgentState::Done,
            Self::Idle => EffectiveAgentState::Idle,
            Self::Error => EffectiveAgentState::Error,
        }
    }

    /// True when this state of the agent's own record refines the hook report `hook` of the
    /// same kind: blocked refines working, and error refines done or idle.
    pub(crate) fn refines(self, hook: ObservedAgentState) -> bool {
        matches!(
            (self, hook),
            (Self::Blocked, Self::Working) | (Self::Error, Self::Done | Self::Idle)
        )
    }
}

/// Reads an observed state from an event. A word that is not one of the five states or the old
/// `Busy` (for example a later stage's word) reads as no observation.
pub(crate) fn lenient_observed_state<'de, D>(
    deserializer: D,
) -> Result<Option<ObservedAgentState>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let word = Option::<String>::deserialize(deserializer)?;
    Ok(match word.as_deref() {
        Some("Working" | "Busy") => Some(ObservedAgentState::Working),
        Some("Blocked") => Some(ObservedAgentState::Blocked),
        Some("Done") => Some(ObservedAgentState::Done),
        Some("Idle") => Some(ObservedAgentState::Idle),
        Some("Error") => Some(ObservedAgentState::Error),
        _ => None,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum EffectiveAgentState {
    #[serde(alias = "Busy")]
    Working,
    Blocked,
    Done,
    Idle,
    Error,
    #[serde(other)]
    Unknown,
}

impl EffectiveAgentState {
    /// True while the agent works.
    pub(crate) fn is_working(self) -> bool {
        matches!(self, Self::Working | Self::Blocked)
    }

    /// True while the agent rests. `Unknown` is neither working nor at rest.
    pub(crate) fn is_at_rest(self) -> bool {
        matches!(self, Self::Done | Self::Idle | Self::Error)
    }

    /// The `state` attribute word of the agent metrics.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Blocked => "blocked",
            Self::Done => "done",
            Self::Idle => "idle",
            Self::Error => "error",
            Self::Unknown => "unknown",
        }
    }
}

pub(crate) const UNKNOWN_PROVIDER_ID: &str = "unknown";
pub(crate) const UNKNOWN_PROVIDER_DISPLAY_NAME: &str = "Unknown";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AgentIncarnation {
    pub pane_id: String,
    pub pane_pid: u32,
    pub agent_pid: u32,
    pub agent_started_at_ms: i64,
    pub provider_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AgentPaneObservation {
    pub incarnation: AgentIncarnation,
    pub tmux_target: String,
    pub session_name: String,
    pub window_index: u32,
    pub pane_index: u32,
    pub working_directory: String,
    pub provider_display_name: String,
    pub observed_at_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ScreenObservation {
    pub incarnation: AgentIncarnation,
    pub state: Option<ObservedAgentState>,
    pub fallback_summary: Option<String>,
    pub classifier_id: String,
    pub observed_at_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AgentEvent {
    PaneObserved(AgentPaneObserved),
    PaneDeparted(AgentPaneDeparted),
    LifecycleObserved(AgentLifecycleObserved),
    ScreenObserved(AgentScreenObserved),
    ActivitySummaryGenerated(AgentActivitySummaryGenerated),
    MonitorHealthChanged(AgentMonitorHealthChanged),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AgentPaneObserved {
    pub pane: AgentPaneObservation,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AgentPaneDeparted {
    pub incarnation: AgentIncarnation,
    pub observed_at_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AgentLifecycleObserved {
    pub incarnation: AgentIncarnation,
    /// None means the event changes no hook state (an unknown word reads as none).
    #[serde(default, deserialize_with = "lenient_observed_state")]
    pub state: Option<ObservedAgentState>,
    pub adapter_id: String,
    pub work_summary: WorkSummaryUpdate,
    pub observed_at_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AgentScreenObserved {
    pub incarnation: AgentIncarnation,
    #[serde(default, deserialize_with = "lenient_observed_state")]
    pub state: Option<ObservedAgentState>,
    pub classifier_id: String,
    pub fallback_summary: Option<String>,
    pub observed_at_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AgentActivitySummaryGenerated {
    pub incarnation: AgentIncarnation,
    pub basis_version: EventVersion,
    pub description: String,
    pub generated_at_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AgentMonitorHealthChanged {
    pub component: String,
    pub healthy: bool,
    pub reason_code: String,
    pub observed_at_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AgentPaneProjection {
    pub pane: AgentPaneObservation,
    pub hook_state: Option<ObservedAgentState>,
    pub hook_observed_at_ms: Option<i64>,
    /// The lifecycle adapter that last reported for this agent process; none has when absent.
    pub lifecycle_adapter_id: Option<String>,
    pub screen_state: Option<ObservedAgentState>,
    pub screen_classifier_id: Option<String>,
    pub screen_observed_at_ms: Option<i64>,
    pub effective_state: EffectiveAgentState,
    pub explicit_work_summary: Option<String>,
    pub explicit_work_summary_updated_at_ms: Option<i64>,
    pub screen_work_summary: Option<String>,
    pub screen_work_summary_updated_at_ms: Option<i64>,
    pub summary_basis_version: EventVersion,
    pub generated_work_summary: Option<String>,
    pub generated_summary_basis_version: Option<EventVersion>,
    pub work_summary: Option<String>,
    pub last_transition_at_ms: i64,
    pub last_event_version: EventVersion,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MonitorHealthProjection {
    pub component: String,
    pub healthy: bool,
    pub reason_code: String,
    pub observed_at_ms: i64,
    pub last_event_version: EventVersion,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[expect(
    clippy::large_enum_variant,
    reason = "the projector contract intentionally moves the complete upsert value"
)]
pub(crate) enum ProjectionChange {
    Upsert(AgentPaneProjection),
    Remove(AgentIncarnation),
    Ignore,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AgentSnapshot {
    pub through_event_version: EventVersion,
    pub server_time_ms: i64,
    pub monitor_health: Vec<MonitorHealthProjection>,
    pub panes: Vec<AgentPaneProjection>,
    pub tts_muted: bool,
    pub messaging_paused: bool,
}

#[cfg(test)]
mod tests {
    use super::{EffectiveAgentState, ObservedAgentState};

    #[test]
    fn blocked_refines_working_and_error_refines_done_or_idle_only() {
        use ObservedAgentState::{Blocked, Done, Error, Idle, Working};
        let all = [Working, Blocked, Done, Idle, Error];
        for record in all {
            for hook in all {
                let expected = matches!(
                    (record, hook),
                    (Blocked, Working) | (Error, Done) | (Error, Idle)
                );
                assert_eq!(record.refines(hook), expected, "{record:?} {hook:?}");
            }
        }
    }

    #[test]
    fn the_two_functions_follow_the_state_table() {
        let observed = [
            (ObservedAgentState::Working, (true, false)),
            (ObservedAgentState::Blocked, (true, false)),
            (ObservedAgentState::Done, (false, true)),
            (ObservedAgentState::Idle, (false, true)),
            (ObservedAgentState::Error, (false, true)),
        ];
        for (state, expected) in observed {
            assert_eq!((state.is_working(), state.is_at_rest()), expected);
        }
        let effective = [
            (EffectiveAgentState::Working, (true, false)),
            (EffectiveAgentState::Blocked, (true, false)),
            (EffectiveAgentState::Done, (false, true)),
            (EffectiveAgentState::Idle, (false, true)),
            (EffectiveAgentState::Error, (false, true)),
            (EffectiveAgentState::Unknown, (false, false)),
        ];
        for (state, expected) in effective {
            assert_eq!((state.is_working(), state.is_at_rest()), expected);
        }
    }

    #[test]
    fn effective_is_one_to_one_and_as_str_gives_the_six_words() {
        let pairs = [
            (
                ObservedAgentState::Working,
                EffectiveAgentState::Working,
                "working",
            ),
            (
                ObservedAgentState::Blocked,
                EffectiveAgentState::Blocked,
                "blocked",
            ),
            (ObservedAgentState::Done, EffectiveAgentState::Done, "done"),
            (ObservedAgentState::Idle, EffectiveAgentState::Idle, "idle"),
            (
                ObservedAgentState::Error,
                EffectiveAgentState::Error,
                "error",
            ),
        ];
        for (observed, effective, word) in pairs {
            assert_eq!(observed.effective(), effective);
            assert_eq!(effective.as_str(), word);
        }
        assert_eq!(EffectiveAgentState::Unknown.as_str(), "unknown");
    }
}
