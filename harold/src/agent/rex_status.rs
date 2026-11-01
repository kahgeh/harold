//! The pure parts of the Rex program status source.
//!
//! A Rex block keeps program status records. The record with no `id` is the one the program
//! writes about itself. This module parses that record and the `rex events` lines about it,
//! maps the record state to an agent state, and keeps each agent's current step in memory.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::Value;
use tokio::sync::watch;

use super::super::domain::ObservedAgentState;
use super::super::summary::normalize_work_summary;
use super::{RexError, parse_json};

/// The state word of a record. A word that is not in this list makes the record unusable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(
    dead_code,
    reason = "the observer and the streams use these in later tasks"
)]
pub(crate) enum RecordState {
    Working,
    Blocked,
    Done,
    Idle,
    Error,
}

impl RecordState {
    fn from_word(word: &str) -> Option<Self> {
        match word {
            "working" => Some(Self::Working),
            "blocked" => Some(Self::Blocked),
            "done" => Some(Self::Done),
            "idle" => Some(Self::Idle),
            "error" => Some(Self::Error),
            _ => None,
        }
    }

    /// Stage 2 has two agent states. A blocked agent counts as working.
    #[allow(dead_code, reason = "the observer uses this in a later task")]
    pub(crate) fn observed(self) -> ObservedAgentState {
        match self {
            Self::Working | Self::Blocked => ObservedAgentState::Working,
            Self::Done | Self::Idle | Self::Error => ObservedAgentState::Idle,
        }
    }
}

/// The root record of a block.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(
    dead_code,
    reason = "the observer and the streams use these in later tasks"
)]
pub(crate) struct StatusRecord {
    pub state: RecordState,
    pub pid: u32,
    /// The cleaned `msg`. Only a `working` or `blocked` record gives one.
    pub msg: Option<String>,
}

/// Reads one record object. The result is `None` when the state word is unknown or the
/// owner pid is missing.
fn record_from_value(value: &Value) -> Option<StatusRecord> {
    let state = RecordState::from_word(value.get("state")?.as_str()?)?;
    let pid = u32::try_from(value.get("owner")?.get("pid")?.as_u64()?).ok()?;
    let msg = match state {
        RecordState::Working | RecordState::Blocked => value
            .get("msg")
            .and_then(Value::as_str)
            .and_then(normalize_work_summary),
        RecordState::Done | RecordState::Idle | RecordState::Error => None,
    };
    Some(StatusRecord { state, pid, msg })
}

fn has_id(value: &Value) -> bool {
    value
        .get("id")
        .and_then(Value::as_str)
        .is_some_and(|id| !id.is_empty())
}

/// Parses the output of `program_status` and returns the root record.
#[allow(dead_code, reason = "the observer uses this in a later task")]
pub(crate) fn parse_program_status(text: &str) -> Result<Option<StatusRecord>, RexError> {
    let value = parse_json(text)?;
    let records = value
        .get("records")
        .and_then(Value::as_array)
        .ok_or(RexError::Malformed)?;
    Ok(records
        .iter()
        .find(|record| !has_id(record))
        .and_then(record_from_value))
}

/// An event about the program status of a block.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code, reason = "the event streams use these in a later task")]
pub(crate) enum StatusEvent {
    /// The root record changed. `record` is `None` when the new record is unusable.
    Changed {
        block_id: String,
        record: Option<StatusRecord>,
    },
    Removed {
        block_id: String,
    },
    BlockClosed {
        block_id: String,
    },
}

/// Parses one line of `rex events -s <session> --json`.
///
/// The layout is assumed, because no status event arrived while the shape was captured. The
/// line is a `session_event` envelope: `payload.event` holds `type` and `payload`. A status
/// event has `block_id` and, for a change, `record` in its payload. The parser also reads
/// an event object that sits at the top of the line.
#[allow(dead_code, reason = "the event streams use this in a later task")]
pub(crate) fn parse_event_line(line: &str) -> Option<StatusEvent> {
    let value: Value = serde_json::from_str(line.trim()).ok()?;
    let event = value
        .get("payload")
        .and_then(|payload| payload.get("event"))
        .unwrap_or(&value);
    let kind = event.get("type")?.as_str()?;
    let payload = event.get("payload")?;
    let block_id = payload.get("block_id")?.as_str()?.to_string();
    match kind {
        "program_status_changed" => {
            let record = payload.get("record").unwrap_or(payload);
            if has_id(record) {
                return None;
            }
            Some(StatusEvent::Changed {
                block_id,
                record: record_from_value(record),
            })
        }
        "program_status_removed" => {
            let record = payload.get("record").unwrap_or(payload);
            if has_id(record) {
                return None;
            }
            Some(StatusEvent::Removed { block_id })
        }
        "block_closed" => Some(StatusEvent::BlockClosed { block_id }),
        _ => None,
    }
}

struct StepsInner {
    steps: Mutex<HashMap<String, (u32, String)>>,
    version: watch::Sender<u64>,
}

impl Default for StepsInner {
    fn default() -> Self {
        Self {
            steps: Mutex::new(HashMap::new()),
            version: watch::channel(0).0,
        }
    }
}

/// The current step of each agent, in memory only. It is keyed by block id and holds the
/// pid of the agent that owns the step.
#[derive(Clone, Default)]
#[allow(
    dead_code,
    reason = "the streams and the watch stream use this in later tasks"
)]
pub(crate) struct CurrentSteps {
    inner: Arc<StepsInner>,
}

#[allow(
    dead_code,
    reason = "the streams and the watch stream use this in later tasks"
)]
impl CurrentSteps {
    /// Stores the step, or removes it when `step` is `None`. The version rises only when
    /// the stored value changes.
    pub(crate) fn set(&self, block_id: &str, pid: u32, step: Option<String>) {
        let changed = {
            let mut steps = self
                .inner
                .steps
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            match step {
                Some(step) => {
                    let new = (pid, step);
                    if steps.get(block_id) == Some(&new) {
                        false
                    } else {
                        steps.insert(block_id.to_string(), new);
                        true
                    }
                }
                None => steps.remove(block_id).is_some(),
            }
        };
        if changed {
            self.bump();
        }
    }

    pub(crate) fn clear_block(&self, block_id: &str) {
        let removed = self
            .inner
            .steps
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(block_id)
            .is_some();
        if removed {
            self.bump();
        }
    }

    /// The step of the block, only when the stored pid is `pid`.
    pub(crate) fn get(&self, block_id: &str, pid: u32) -> Option<String> {
        let steps = self
            .inner
            .steps
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        steps
            .get(block_id)
            .filter(|(stored, _)| *stored == pid)
            .map(|(_, step)| step.clone())
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<u64> {
        self.inner.version.subscribe()
    }

    fn bump(&self) {
        self.inner.version.send_modify(|version| *version += 1);
    }
}
