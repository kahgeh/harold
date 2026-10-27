//! The facts handed to the model at each run: one JSON document built from Harold's
//! agent list, the pod snapshot and the run's own numbers. Gathering them reads the
//! input box of each Claude Code pane, and changes nothing.

use std::sync::Arc;

use serde::Serialize;
use time::UtcOffset;
use tokio::task::JoinSet;

use super::clock::local_text;
use crate::agent::domain::EffectiveAgentState;
use crate::pod_stats::PodSnapshot;
use crate::timed_prompts::InputBox;
use crate::tools::{AgentFact, Agents, ChiefPane, Pods};

/// How many characters of an agent's work summary the facts hold.
const WORK_SUMMARY_CHARS: usize = 200;

#[derive(Debug, Serialize, PartialEq)]
pub(super) struct Facts {
    pub now: String,
    pub chief: bool,
    pub agents: Vec<AgentEntry>,
    pub pods: Vec<PodEntry>,
    pub watch: WatchEntry,
}

#[derive(Debug, Serialize, PartialEq)]
pub(super) struct AgentEntry {
    pub folder: String,
    pub pane: String,
    pub provider: String,
    pub state: &'static str,
    pub seconds_in_state: i64,
    pub work_summary: Option<String>,
    /// `empty`, `holds_text` or `not_found`, for a Claude Code pane.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_box: Option<&'static str>,
}

#[derive(Debug, Serialize, PartialEq)]
pub(super) struct PodEntry {
    pub product: String,
    pub slots: Slots,
    pub specs: Vec<SpecEntry>,
}

#[derive(Debug, Serialize, PartialEq)]
pub(super) struct Slots {
    pub busy: u32,
    pub free: u32,
    pub total: u32,
}

#[derive(Debug, Serialize, PartialEq)]
pub(super) struct SpecEntry {
    pub id: String,
    pub state: &'static str,
    pub seconds_in_state: i64,
}

#[derive(Debug, Serialize, PartialEq)]
pub(super) struct WatchEntry {
    pub run: u32,
    pub runs_left: u32,
    pub previous_outcome: Option<String>,
}

pub(super) struct Sources<'a> {
    pub agents: &'a Arc<dyn Agents>,
    pub pods: &'a Arc<dyn Pods>,
    pub chief_pane: &'a Arc<dyn ChiefPane>,
    pub offset: UtcOffset,
}

fn state_name(state: EffectiveAgentState) -> &'static str {
    match state {
        EffectiveAgentState::Busy => "busy",
        EffectiveAgentState::Idle => "idle",
        EffectiveAgentState::Unknown => "unknown",
    }
}

fn box_name(reading: Option<InputBox>) -> &'static str {
    match reading {
        // A Vim box in NORMAL mode is empty here: the typing step presses `i`.
        Some(InputBox::Empty | InputBox::NotInsert) => "empty",
        Some(InputBox::HoldsText) => "holds_text",
        Some(InputBox::NotFound) | None => "not_found",
    }
}

fn cut(text: &str) -> String {
    text.chars().take(WORK_SUMMARY_CHARS).collect()
}

fn seconds_since(now_ms: i64, then_ms: i64) -> i64 {
    ((now_ms - then_ms) / 1000).max(0)
}

/// Gathers the facts for the run described by `watch`, at `now_ms`.
pub(super) async fn gather(sources: &Sources<'_>, now_ms: i64, watch: WatchEntry) -> Facts {
    let listed = sources.agents.agents();
    let boxes = read_boxes(sources.agents, &listed).await;
    let agents = listed
        .into_iter()
        .map(|fact| {
            let input_box = fact
                .reads_as_claude
                .then(|| box_name(boxes.get(&fact.pane_id).copied().flatten()));
            entry(fact, now_ms, input_box)
        })
        .collect();
    Facts {
        now: local_text(now_ms, sources.offset),
        chief: sources.chief_pane.chief_pane().is_some(),
        agents,
        pods: pods(&sources.pods.pods(), now_ms),
        watch,
    }
}

fn entry(fact: AgentFact, now_ms: i64, input_box: Option<&'static str>) -> AgentEntry {
    AgentEntry {
        folder: fact.folder,
        pane: fact.tmux_target,
        provider: fact.provider_id,
        state: state_name(fact.state),
        seconds_in_state: seconds_since(now_ms, fact.state_since_ms),
        work_summary: fact.work_summary.as_deref().map(cut),
        input_box,
    }
}

/// Reads the input boxes of the Claude Code panes together, so a slow screen does not
/// hold up the others.
async fn read_boxes(
    agents: &Arc<dyn Agents>,
    listed: &[AgentFact],
) -> std::collections::HashMap<String, Option<InputBox>> {
    let mut reads = JoinSet::new();
    for fact in listed.iter().filter(|fact| fact.reads_as_claude) {
        let (agents, pane) = (Arc::clone(agents), fact.pane_id.clone());
        reads.spawn(async move {
            let reading = agents.input_box(&pane).await;
            (pane, reading)
        });
    }
    let mut boxes = std::collections::HashMap::new();
    while let Some(Ok((pane, reading))) = reads.join_next().await {
        boxes.insert(pane, reading);
    }
    boxes
}

fn pods(snapshot: &PodSnapshot, now_ms: i64) -> Vec<PodEntry> {
    snapshot
        .slots
        .iter()
        .map(|(product, counts)| PodEntry {
            product: product.clone(),
            slots: Slots {
                busy: counts.busy,
                free: counts.free,
                total: counts.busy + counts.free,
            },
            specs: snapshot
                .specs
                .get(product)
                .map(|specs| {
                    specs
                        .iter()
                        .map(|spec| SpecEntry {
                            id: spec.id.clone(),
                            state: spec.state.as_str(),
                            seconds_in_state: seconds_since(
                                now_ms,
                                (spec.state_at.unix_timestamp_nanos() / 1_000_000) as i64,
                            ),
                        })
                        .collect()
                })
                .unwrap_or_default(),
        })
        .collect()
}

#[cfg(test)]
#[path = "facts_tests.rs"]
mod tests;
