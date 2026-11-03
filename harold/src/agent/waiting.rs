//! Decides which agents wait for the user, from the agent snapshot and the focus.

use std::collections::BTreeMap;

use super::domain::{AgentSnapshot, EffectiveAgentState};

/// The pane the user looks at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Focus {
    /// The focus is not known. Nothing begins or ends by focus.
    Unknown,
    /// The focus is known. `None` means no agent pane has the focus.
    Pane(Option<String>),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum WaitingEvent {
    Began {
        pane_id: String,
        role: String,
    },
    Seen {
        pane_id: String,
        role: String,
        waited_s: f64,
    },
    /// The agent works again or its pane is gone. No duration is recorded.
    Ended {
        pane_id: String,
    },
}

#[derive(Clone, Default, PartialEq)]
pub(crate) struct WaitingView {
    pub since_ms: BTreeMap<String, i64>,
    pub focus_known: bool,
}

struct Tracked {
    agent_pid: u32,
    agent_started_at_ms: i64,
    last_state: EffectiveAgentState,
}

struct Entry {
    since_ms: i64,
    role: String,
}

#[derive(Default)]
pub(crate) struct WaitingTracker {
    tracked: BTreeMap<String, Tracked>,
    waiting: BTreeMap<String, Entry>,
    focus_known: bool,
}

impl WaitingTracker {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Apply one snapshot and the focus. `role_of` maps a pane id to its role.
    pub(crate) fn observe(
        &mut self,
        snapshot: &AgentSnapshot,
        focus: &Focus,
        now_ms: i64,
        role_of: &dyn Fn(&str) -> String,
    ) -> Vec<WaitingEvent> {
        let mut events = Vec::new();
        self.focus_known = !matches!(focus, Focus::Unknown);

        let gone: Vec<String> = self
            .tracked
            .keys()
            .filter(|id| {
                !snapshot
                    .panes
                    .iter()
                    .any(|p| &p.pane.incarnation.pane_id == *id)
            })
            .cloned()
            .collect();
        for id in gone {
            self.tracked.remove(&id);
            if self.waiting.remove(&id).is_some() {
                events.push(WaitingEvent::Ended { pane_id: id });
            }
        }

        for projection in &snapshot.panes {
            let inc = &projection.pane.incarnation;
            let id = inc.pane_id.as_str();
            let state = projection.effective_state;

            let previous = match self.tracked.get(id) {
                Some(t)
                    if t.agent_pid == inc.agent_pid
                        && t.agent_started_at_ms == inc.agent_started_at_ms =>
                {
                    Some(t.last_state)
                }
                Some(_) => {
                    if self.waiting.remove(id).is_some() {
                        events.push(WaitingEvent::Ended {
                            pane_id: id.to_string(),
                        });
                    }
                    None
                }
                None => None,
            };
            self.tracked.insert(
                id.to_string(),
                Tracked {
                    agent_pid: inc.agent_pid,
                    agent_started_at_ms: inc.agent_started_at_ms,
                    last_state: state,
                },
            );

            let remote = id.starts_with("rex:");
            if self.waiting.contains_key(id) {
                if state.is_working() {
                    self.waiting.remove(id);
                    events.push(WaitingEvent::Ended {
                        pane_id: id.to_string(),
                    });
                } else if !remote
                    && let Focus::Pane(Some(focused)) = focus
                    && focused == id
                    && let Some(entry) = self.waiting.remove(id)
                {
                    events.push(WaitingEvent::Seen {
                        pane_id: id.to_string(),
                        role: entry.role,
                        waited_s: (now_ms - entry.since_ms) as f64 / 1000.0,
                    });
                }
            } else if let Some(prev) = previous
                && prev.is_working()
                && state.is_at_rest()
                && let Focus::Pane(focused) = focus
                && focused.as_deref() != Some(id)
            {
                let role = role_of(id);
                self.waiting.insert(
                    id.to_string(),
                    Entry {
                        since_ms: now_ms,
                        role: role.clone(),
                    },
                );
                events.push(WaitingEvent::Began {
                    pane_id: id.to_string(),
                    role,
                });
            }
        }
        events
    }

    pub(crate) fn view(&self) -> WaitingView {
        WaitingView {
            since_ms: self
                .waiting
                .iter()
                .map(|(id, e)| (id.clone(), e.since_ms))
                .collect(),
            focus_known: self.focus_known,
        }
    }
}
