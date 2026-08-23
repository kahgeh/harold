#![allow(
    dead_code,
    reason = "domain types are consumed by later monitor slices"
)]

use events::EventStreamVersion;
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
    Busy,
    Idle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum EffectiveAgentState {
    Busy,
    Idle,
    Unknown,
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
    WorkSummaryCandidatesRepaired(AgentWorkSummaryCandidatesRepaired),
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
    pub state: ObservedAgentState,
    pub adapter_id: String,
    pub work_summary: WorkSummaryUpdate,
    pub observed_at_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AgentScreenObserved {
    pub incarnation: AgentIncarnation,
    pub state: Option<ObservedAgentState>,
    pub classifier_id: String,
    pub fallback_summary: Option<String>,
    pub observed_at_ms: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum AgentWorkSummaryRepairReason {
    ConfiguredIdlePlaceholder,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AgentWorkSummaryCandidatesRepaired {
    pub incarnation: AgentIncarnation,
    pub clear_explicit: bool,
    pub clear_screen: bool,
    pub reason: AgentWorkSummaryRepairReason,
    pub observed_at_ms: i64,
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
    pub screen_state: Option<ObservedAgentState>,
    pub screen_classifier_id: Option<String>,
    pub screen_observed_at_ms: Option<i64>,
    pub effective_state: EffectiveAgentState,
    pub explicit_work_summary: Option<String>,
    pub explicit_work_summary_updated_at_ms: Option<i64>,
    pub screen_work_summary: Option<String>,
    pub screen_work_summary_updated_at_ms: Option<i64>,
    pub work_summary: Option<String>,
    pub last_transition_at_ms: i64,
    pub last_event_version: EventStreamVersion,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MonitorHealthProjection {
    pub component: String,
    pub healthy: bool,
    pub reason_code: String,
    pub observed_at_ms: i64,
    pub last_event_version: EventStreamVersion,
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
    pub through_event_version: EventStreamVersion,
    pub server_time_ms: i64,
    pub monitor_health: Vec<MonitorHealthProjection>,
    pub panes: Vec<AgentPaneProjection>,
}
