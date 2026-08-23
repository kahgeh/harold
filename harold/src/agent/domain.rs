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
