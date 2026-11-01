use std::collections::{BTreeMap, HashMap};

use tokio::sync::watch;

use crate::store::EventVersion;
use crate::telemetry::{AgentCount, note_agent_counts, record_agent_state_duration};

use super::domain::{AgentSnapshot, EffectiveAgentState};

#[derive(Clone)]
pub(crate) struct AgentSnapshotHub {
    sender: watch::Sender<AgentSnapshot>,
    _receiver: watch::Receiver<AgentSnapshot>,
}

impl AgentSnapshotHub {
    pub(crate) fn new(initial: AgentSnapshot) -> Self {
        note_agent_counts(agent_counts(&initial));
        let (sender, receiver) = watch::channel(initial);
        Self {
            sender,
            _receiver: receiver,
        }
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<AgentSnapshot> {
        self.sender.subscribe()
    }

    /// A copy of the snapshot last published.
    pub(crate) fn current(&self) -> AgentSnapshot {
        self._receiver.borrow().clone()
    }

    pub(crate) fn through_event_version(&self) -> EventVersion {
        self._receiver.borrow().through_event_version
    }

    pub(crate) fn tts_muted(&self) -> bool {
        self._receiver.borrow().tts_muted
    }

    pub(crate) fn messaging_paused(&self) -> bool {
        self._receiver.borrow().messaging_paused
    }

    pub(crate) fn publish_committed(&self, snapshot: AgentSnapshot) {
        self.sender.send_if_modified(|current| {
            if snapshot.through_event_version.get() <= current.through_event_version.get() {
                return false;
            }
            record_state_changes(current, &snapshot);
            note_agent_counts(agent_counts(&snapshot));
            *current = snapshot;
            true
        });
    }
}

/// Records the time each pane spent in the state it left. A new or departed pane records nothing.
fn record_state_changes(old: &AgentSnapshot, new: &AgentSnapshot) {
    let before: HashMap<&str, _> = old
        .panes
        .iter()
        .map(|pane| (pane.pane.incarnation.pane_id.as_str(), pane))
        .collect();
    for pane in &new.panes {
        let Some(previous) = before.get(pane.pane.incarnation.pane_id.as_str()) else {
            continue;
        };
        if previous.pane.incarnation != pane.pane.incarnation
            || previous.effective_state == pane.effective_state
        {
            continue;
        }
        let elapsed_ms = pane.last_transition_at_ms - previous.last_transition_at_ms;
        if elapsed_ms < 0 {
            continue;
        }
        let seconds = elapsed_ms as f64 / 1000.0;
        record_agent_state_duration(
            previous.effective_state.as_str(),
            &previous.pane.incarnation.provider_id,
            seconds,
        );
    }
}

/// The number of panes in each state, for each provider in the snapshot. A state with no panes
/// reports 0, because the gauge exports only the values it reads.
pub(super) fn agent_counts(snapshot: &AgentSnapshot) -> Vec<AgentCount> {
    let mut counts: BTreeMap<(&'static str, &str), u64> = BTreeMap::new();
    for pane in &snapshot.panes {
        for state in ALL_STATES {
            counts
                .entry((state.as_str(), pane.pane.incarnation.provider_id.as_str()))
                .or_default();
        }
    }
    for pane in &snapshot.panes {
        *counts
            .entry((
                pane.effective_state.as_str(),
                pane.pane.incarnation.provider_id.as_str(),
            ))
            .or_default() += 1;
    }
    counts
        .into_iter()
        .map(|((state, provider_id), count)| AgentCount {
            state,
            provider_id: provider_id.to_owned(),
            count,
        })
        .collect()
}

const ALL_STATES: [EffectiveAgentState; 6] = [
    EffectiveAgentState::Working,
    EffectiveAgentState::Blocked,
    EffectiveAgentState::Done,
    EffectiveAgentState::Idle,
    EffectiveAgentState::Error,
    EffectiveAgentState::Unknown,
];
