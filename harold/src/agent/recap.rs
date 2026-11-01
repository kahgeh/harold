//! The recap of an agent session: a short text the agent's own transcript holds.
//!
//! One function per provider adapter reads a transcript line. `Recaps` keeps the last recap
//! of each pane in memory only.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::sync::watch;

use super::domain::AgentSnapshot;
use super::summary::normalize_recap;
use crate::settings::{AgentProviderSettings, RecapAdapter};
use crate::telemetry;
use crate::transcript::{FIRST_READ_LIMIT, Tail, read_new_lines};

/// A read that takes longer than this logs `agent recap read slow`.
pub(crate) const SLOW_READ: Duration = Duration::from_millis(200);

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct RecapLine {
    pub(crate) text: String,
    pub(crate) at_ms: i64,
}

impl std::fmt::Debug for RecapLine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecapLine")
            .field("at_ms", &self.at_ms)
            .finish_non_exhaustive()
    }
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
            let kept = entries
                .get_mut(pane_id)
                .filter(|entry| entry.agent_pid == agent_pid && entry.session_id == session_id);
            if let Some(entry) = kept {
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

    /// Stores a recap for the agent, as a test needs it.
    #[cfg(test)]
    pub(crate) fn put_for_test(&self, pane_id: &str, agent_pid: u32, text: &str, at_ms: i64) {
        self.note_turn_end(pane_id, agent_pid, "claude", "s-test", "/t/s-test.jsonl");
        self.update_read(
            pane_id,
            agent_pid,
            "s-test",
            "/t/s-test.jsonl",
            Tail::default(),
            Some(RecapLine {
                text: text.to_string(),
                at_ms,
            }),
        );
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
    /// agent pid, session id or path writes nothing. The version rises only when the recap changes.
    pub(crate) fn update_read(
        &self,
        pane_id: &str,
        agent_pid: u32,
        session_id: &str,
        transcript_path: &str,
        tail: Tail,
        recap: Option<RecapLine>,
    ) {
        let changed = {
            let mut entries = self.lock();
            let Some(entry) = entries.get_mut(pane_id).filter(|entry| {
                entry.agent_pid == agent_pid
                    && entry.session_id == session_id
                    && entry.transcript_path == transcript_path
            }) else {
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

/// What the loop remembers of one pane between checks.
struct Seen {
    agent_pid: u32,
    session_id: String,
    transcript_path: String,
    /// The file length at the last check that stat-ed or read the file.
    length: Option<u64>,
    /// The reason of the last logged failure, until a read works again.
    logged_failure: Option<&'static str>,
}

/// Reads the transcripts for recaps. One `check_once` is one pass over all agents.
pub(crate) struct RecapReader {
    recaps: Recaps,
    providers: Vec<AgentProviderSettings>,
    slow_after: Duration,
    seen: HashMap<String, Seen>,
}

fn failure_reason(error: &std::io::Error) -> &'static str {
    match error.kind() {
        ErrorKind::NotFound => "not_found",
        ErrorKind::PermissionDenied => "permission",
        _ => "io",
    }
}

impl RecapReader {
    pub(crate) fn new(
        recaps: Recaps,
        providers: Vec<AgentProviderSettings>,
        slow_after: Duration,
    ) -> Self {
        Self {
            recaps,
            providers,
            slow_after,
            seen: HashMap::new(),
        }
    }

    /// Checks every live agent that has an adapter and a kept session. One `stat` call per
    /// agent; the file is read only when its length changed since the last check.
    pub(crate) async fn check_once(&mut self, snapshot: &AgentSnapshot) {
        let live: Vec<&str> = snapshot
            .panes
            .iter()
            .map(|pane| pane.pane.incarnation.pane_id.as_str())
            .collect();
        self.recaps.forget_missing(&live);
        self.seen
            .retain(|pane_id, _| live.contains(&pane_id.as_str()));
        for target in self.recaps.targets() {
            let Some(pane) = snapshot.panes.iter().find(|pane| {
                pane.pane.incarnation.pane_id == target.pane_id
                    && pane.pane.incarnation.agent_pid == target.agent_pid
            }) else {
                continue;
            };
            let provider_id = pane.pane.incarnation.provider_id.as_str();
            let adapter = self
                .providers
                .iter()
                .find(|provider| provider.id == provider_id)
                .map_or(RecapAdapter::None, |provider| provider.recap_adapter);
            if adapter == RecapAdapter::None {
                continue;
            }
            self.check_pane(&target, provider_id, adapter).await;
        }
    }

    async fn check_pane(&mut self, target: &RecapTarget, provider_id: &str, adapter: RecapAdapter) {
        let seen = self
            .seen
            .entry(target.pane_id.clone())
            .or_insert_with(|| Seen {
                agent_pid: target.agent_pid,
                session_id: target.session_id.clone(),
                transcript_path: target.transcript_path.clone(),
                length: None,
                logged_failure: None,
            });
        if seen.agent_pid != target.agent_pid
            || seen.session_id != target.session_id
            || seen.transcript_path != target.transcript_path
        {
            *seen = Seen {
                agent_pid: target.agent_pid,
                session_id: target.session_id.clone(),
                transcript_path: target.transcript_path.clone(),
                length: None,
                logged_failure: None,
            };
        }
        let path = Path::new(&target.transcript_path);
        let outcome = match std::fs::metadata(path) {
            Err(error) => Err(error),
            Ok(meta) if seen.length == Some(meta.len()) => return,
            Ok(meta) => {
                seen.length = Some(meta.len());
                self.read(target, adapter).await
            }
        };
        let seen = self
            .seen
            .get_mut(&target.pane_id)
            .expect("the entry was inserted above");
        match outcome {
            Ok(found) => {
                seen.logged_failure = None;
                telemetry::record_recap_read(provider_id, if found { "found" } else { "none" });
            }
            Err(error) => {
                // A failed read is tried again at the next check.
                seen.length = None;
                let reason = failure_reason(&error);
                telemetry::record_recap_read(provider_id, "failed");
                if seen.logged_failure != Some(reason) {
                    seen.logged_failure = Some(reason);
                    tracing::info!(
                        pane_id = %target.pane_id,
                        provider_id = %provider_id,
                        reason = %reason,
                        "agent recap read failed"
                    );
                }
            }
        }
    }

    /// Reads the new lines on a blocking thread and stores the last recap. Returns whether
    /// the read found a recap.
    async fn read(&self, target: &RecapTarget, adapter: RecapAdapter) -> std::io::Result<bool> {
        let path = target.transcript_path.clone();
        let session_id = target.session_id.clone();
        let mut tail = target.tail;
        let started = Instant::now();
        let joined = tokio::task::spawn_blocking(move || {
            let mut last = None;
            let bytes = read_new_lines(
                Path::new(&path),
                &mut tail,
                Some(FIRST_READ_LIMIT),
                |line| {
                    if let Some(recap) = read_recap_line(adapter, line, &session_id) {
                        last = Some(recap);
                    }
                },
            )?;
            Ok::<_, std::io::Error>((bytes, tail, last))
        })
        .await;
        let elapsed = started.elapsed();
        let read = joined.map_err(std::io::Error::other).and_then(|read| read);
        if elapsed > self.slow_after {
            tracing::warn!(
                pane_id = %target.pane_id,
                elapsed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
                bytes = read.as_ref().map_or(0, |(bytes, ..)| *bytes),
                "agent recap read slow"
            );
        }
        let (_, tail, last) = read?;
        // Only a recap that differs from the stored one is new.
        let found = last.is_some() && last != self.recaps.get(&target.pane_id, target.agent_pid);
        self.recaps.update_read(
            &target.pane_id,
            target.agent_pid,
            &target.session_id,
            &target.transcript_path,
            tail,
            last,
        );
        Ok(found)
    }
}

/// Spawns the loop. It stops on shutdown.
pub(crate) fn spawn_recap_reader(
    recaps: Recaps,
    snapshots: watch::Receiver<AgentSnapshot>,
    providers: Vec<AgentProviderSettings>,
    interval: Duration,
    mut shutdown: watch::Receiver<()>,
) {
    let mut reader = RecapReader::new(recaps, providers, SLOW_READ);
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let snapshot = snapshots.borrow().clone();
                    reader.check_once(&snapshot).await;
                }
                _ = shutdown.changed() => break,
            }
        }
    });
}
