#![allow(
    dead_code,
    reason = "domain types are consumed by later monitor slices"
)]

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum WorkSummaryUpdate {
    #[default]
    Unchanged,
    Clear,
    Set(String),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum CompletionSummaryUpdate {
    #[default]
    Unchanged,
    Set(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ObservedAgentState {
    Busy,
    Idle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EffectiveAgentState {
    Busy,
    Idle,
    Unknown,
}

pub(crate) const UNKNOWN_PROVIDER_ID: &str = "unknown";
pub(crate) const UNKNOWN_PROVIDER_DISPLAY_NAME: &str = "Unknown";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AgentIncarnation {
    pub pane_id: String,
    pub pane_pid: u32,
    pub agent_pid: u32,
    pub agent_started_at_ms: i64,
    pub provider_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ScreenObservation {
    pub incarnation: AgentIncarnation,
    pub state: Option<ObservedAgentState>,
    pub fallback_summary: Option<String>,
    pub classifier_id: String,
    pub observed_at_ms: i64,
}
