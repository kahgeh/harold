//! The recap of an agent session: a short text the agent's own transcript holds.
//!
//! One function per provider adapter reads a transcript line. `Recaps` keeps the last recap
//! of each pane in memory only.

// Task 4 wires the check loop that calls this module; remove the allow then.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::sync::watch;

use super::summary::normalize_recap;
use crate::settings::RecapAdapter;
use crate::transcript::Tail;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RecapLine {
    pub(crate) text: String,
    pub(crate) at_ms: i64,
}

/// Reads one transcript line with the adapter. Returns `None` for a line that is not a recap
/// of `session_id`.
pub(crate) fn read_recap_line(
    adapter: RecapAdapter,
    line: &[u8],
    session_id: &str,
) -> Option<RecapLine> {
    match adapter {
        RecapAdapter::None => None,
        RecapAdapter::ClaudeTranscriptV1 => {
            const MARKER: &[u8] = b"\"away_summary\"";
            const NOTE: &str = "(disable recaps in /config)";
            if !line.windows(MARKER.len()).any(|window| window == MARKER) {
                return None;
            }
            let value: serde_json::Value = serde_json::from_slice(line).ok()?;
            if value.get("subtype")?.as_str()? != "away_summary"
                || value.get("sessionId")?.as_str()? != session_id
            {
                return None;
            }
            let content = value.get("content")?.as_str()?.trim_end();
            let content = content.strip_suffix(NOTE).unwrap_or(content);
            let text = normalize_recap(content)?;
            let at = OffsetDateTime::parse(value.get("timestamp")?.as_str()?, &Rfc3339).ok()?;
            let at_ms = i64::try_from(at.unix_timestamp_nanos() / 1_000_000).ok()?;
            Some(RecapLine { text, at_ms })
        }
    }
}

/// What the check loop needs to read the transcript of one pane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RecapTarget {
    pub(crate) pane_id: String,
    pub(crate) agent_pid: u32,
    pub(crate) provider_id: String,
    pub(crate) session_id: String,
    pub(crate) transcript_path: String,
    pub(crate) tail: Tail,
}

struct Entry {
    agent_pid: u32,
    provider_id: String,
    session_id: String,
    transcript_path: String,
    tail: Tail,
    recap: Option<RecapLine>,
}

struct RecapsInner {
    entries: Mutex<HashMap<String, Entry>>,
    version: watch::Sender<u64>,
}

impl Default for RecapsInner {
    fn default() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            version: watch::channel(0).0,
        }
    }
}

/// The last recap of each pane, in memory only. The version rises only when a stored recap
/// changes or goes away.
#[derive(Clone, Default)]
pub(crate) struct Recaps {
    inner: Arc<RecapsInner>,
}

impl Recaps {
    pub(crate) fn subscribe(&self) -> watch::Receiver<u64> {
        self.inner.version.subscribe()
    }

    /// Keeps the session id and path for the incarnation. A new agent pid or session id drops
    /// the old recap and the read offset.
    pub(crate) fn note_turn_end(
        &self,
        pane_id: &str,
        agent_pid: u32,
        provider_id: &str,
        session_id: &str,
        transcript_path: &str,
    ) {
        let changed = {
            let mut entries = self.lock();
            let same = entries.get(pane_id).is_some_and(|entry| {
                entry.agent_pid == agent_pid && entry.session_id == session_id
            });
            if same {
                let entry = entries.get_mut(pane_id).expect("entry checked above");
                if entry.transcript_path != transcript_path {
                    entry.transcript_path = transcript_path.to_string();
                    entry.tail = Tail::default();
                }
                entry.provider_id = provider_id.to_string();
                false
            } else {
                entries
                    .insert(
                        pane_id.to_string(),
                        Entry {
                            agent_pid,
                            provider_id: provider_id.to_string(),
                            session_id: session_id.to_string(),
                            transcript_path: transcript_path.to_string(),
                            tail: Tail::default(),
                            recap: None,
                        },
                    )
                    .is_some_and(|old| old.recap.is_some())
            }
        };
        if changed {
            self.bump();
        }
    }

    /// The recap of the pane, only when the stored agent pid is `agent_pid`.
    pub(crate) fn get(&self, pane_id: &str, agent_pid: u32) -> Option<RecapLine> {
        self.lock()
            .get(pane_id)
            .filter(|entry| entry.agent_pid == agent_pid)
            .and_then(|entry| entry.recap.clone())
    }

    /// Drops the panes that are not in `live_pane_ids`.
    pub(crate) fn forget_missing(&self, live_pane_ids: &[&str]) {
        let changed = {
            let mut entries = self.lock();
            let mut changed = false;
            entries.retain(|pane_id, entry| {
                let keep = live_pane_ids.contains(&pane_id.as_str());
                if !keep && entry.recap.is_some() {
                    changed = true;
                }
                keep
            });
            changed
        };
        if changed {
            self.bump();
        }
    }

    /// A copy of what the check loop needs for each pane.
    pub(crate) fn targets(&self) -> Vec<RecapTarget> {
        self.lock()
            .iter()
            .map(|(pane_id, entry)| RecapTarget {
                pane_id: pane_id.clone(),
                agent_pid: entry.agent_pid,
                provider_id: entry.provider_id.clone(),
                session_id: entry.session_id.clone(),
                transcript_path: entry.transcript_path.clone(),
                tail: entry.tail,
            })
            .collect()
    }

    /// Keeps the new read offset, and the new recap when there is one. A read for another
    /// agent pid writes nothing. The version rises only when the recap changes.
    pub(crate) fn update_read(
        &self,
        pane_id: &str,
        agent_pid: u32,
        tail: Tail,
        recap: Option<RecapLine>,
    ) {
        let changed = {
            let mut entries = self.lock();
            let Some(entry) = entries
                .get_mut(pane_id)
                .filter(|entry| entry.agent_pid == agent_pid)
            else {
                return;
            };
            entry.tail = tail;
            match recap {
                Some(recap) if entry.recap.as_ref() != Some(&recap) => {
                    entry.recap = Some(recap);
                    true
                }
                _ => false,
            }
        };
        if changed {
            self.bump();
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        self.inner
            .entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn bump(&self) {
        self.inner.version.send_modify(|version| *version += 1);
    }
}
