//! Reminds the chief to hand off and restart. When the chief's Claude Code session is
//! older than `after_hours` or has compacted `after_compactions` times, Harold types one
//! line into the chief's pane at the end of a turn, once per session.
//!
//! The session's start and compactions come from its transcript, a JSON-lines file. Only
//! each line's `timestamp`, `isCompactSummary` and `sessionId` are
//! read: no message text is kept, logged or stored. The file is read in full once, then
//! only from the offset kept in `restart-reminder.json`.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tracing::{info, warn};

use crate::outbound::left_out::{ChiefLookup, resolve_chief};
use crate::settings::RestartReminderSettings;
use crate::timed_prompts::{AgentPane, LineOutcome, Ports, Writer, type_line_once};

/// The state file in Harold's data folder.
const STATE_FILE: &str = "restart-reminder.json";

/// A session not seen for this long is forgotten.
const FORGET_AFTER_SECONDS: i64 = 30 * 24 * 60 * 60;

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// What Harold keeps about one session. It holds no transcript text.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Session {
    transcript_path: String,
    /// Bytes of the transcript already read, always at the end of a whole line.
    offset: u64,
    /// RFC 3339, the first line's `timestamp`.
    started: Option<String>,
    compactions: u32,
    /// RFC 3339, when the reminder was typed.
    reminded_at: Option<String>,
    /// Unix seconds of the last turn end seen.
    last_seen: i64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    sessions: BTreeMap<String, Session>,
}

/// The fields of a transcript line Harold reads (`type` and `subtype` are not needed: a
/// compaction's summary line says it is one). Serde skips every other field, so the
/// message text is never copied out.
#[derive(Deserialize)]
struct Line {
    timestamp: Option<String>,
    #[serde(rename = "isCompactSummary")]
    is_compact_summary: Option<bool>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
}

/// Why the line was not typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum NotTyped {
    PaneBusy,
    NoChief,
    TranscriptUnreadable,
    NoTranscriptPath,
}

impl NotTyped {
    fn reason(self) -> &'static str {
        match self {
            Self::PaneBusy => "pane_busy",
            Self::NoChief => "no_chief",
            Self::TranscriptUnreadable => "transcript_unreadable",
            Self::NoTranscriptPath => "no_transcript_path",
        }
    }
}

/// Learns of the end of an agent's turn. The service hands it each one.
pub(crate) trait TurnEnds: Send + Sync {
    /// Looks at the turn end without making the caller wait.
    fn turn_ended(&self, pane_id: &str, session_id: &str, transcript_path: &str);
}

struct Shared<P> {
    ports: Arc<P>,
    settings: RestartReminderSettings,
    state_path: PathBuf,
    inner: tokio::sync::Mutex<Inner>,
}

struct Inner {
    state: State,
    /// The reasons already logged, once for each session.
    logged: std::collections::BTreeSet<(String, NotTyped)>,
}

pub(crate) struct RestartReminder<P> {
    shared: Arc<Shared<P>>,
}

impl<P: Ports> RestartReminder<P> {
    /// `data_folder` holds `restart-reminder.json`. A state file that is missing or
    /// cannot be read starts empty.
    pub(crate) fn new(
        ports: Arc<P>,
        settings: RestartReminderSettings,
        data_folder: &Path,
    ) -> Self {
        let state_path = data_folder.join(STATE_FILE);
        let state = std::fs::read(&state_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Self {
            shared: Arc::new(Shared {
                ports,
                settings,
                state_path,
                inner: tokio::sync::Mutex::new(Inner {
                    state,
                    logged: Default::default(),
                }),
            }),
        }
    }

    /// Handles one turn end: from the chief's pane it reads the transcript and, when the
    /// session is due and has had no reminder, types the line once.
    pub(crate) async fn on_turn_end(&self, pane_id: &str, session_id: &str, transcript_path: &str) {
        let shared = &self.shared;
        if !shared.settings.enabled {
            return;
        }
        let view = shared.ports.agents();
        let chief = shared.ports.chief();
        let found = resolve_chief(
            view.panes.iter().map(AgentPane::position),
            chief.folder,
            chief.session,
        );
        if view.listing_failed
            || !matches!(found, ChiefLookup::Found(ref chief) if chief == pane_id)
        {
            return;
        }
        // Held to the end, so two turn ends never type the line twice.
        let mut inner = shared.inner.lock().await;
        // A turn end with no session id is named by its pane.
        let key = if session_id.is_empty() {
            pane_id
        } else {
            session_id
        };
        if session_id.is_empty() || transcript_path.is_empty() {
            inner.not_typed(key, NotTyped::NoTranscriptPath);
            return;
        }
        let now = OffsetDateTime::now_utc();
        if inner
            .state
            .sessions
            .get(key)
            .is_some_and(|session| session.reminded_at.is_some())
        {
            return;
        }
        let mut session = inner.state.sessions.get(key).cloned().unwrap_or_default();
        let (path, id) = (transcript_path.to_string(), session_id.to_string());
        let read = tokio::task::spawn_blocking(move || {
            read_new(&mut session, &path, &id).map(|()| session)
        })
        .await;
        let Ok(Ok(mut session)) = read else {
            inner.not_typed(key, NotTyped::TranscriptUnreadable);
            return;
        };
        session.last_seen = now.unix_timestamp();
        inner
            .state
            .sessions
            .insert(key.to_string(), session.clone());
        let started = session.started.as_deref().and_then(parse_time);
        let due = is_due(&session, started, now, &shared.settings);
        if !due {
            inner.save(&shared.state_path, now);
            return;
        }
        // `due` holds only with a session start or enough compactions; the line needs
        // the start, so a session without a readable one waits.
        let Some(started) = started else {
            inner.save(&shared.state_path, now);
            inner.not_typed(key, NotTyped::TranscriptUnreadable);
            return;
        };
        let line = reminder_line(started, session.compactions);
        let outcome = type_when_idle(&*shared.ports, pane_id, &line).await;
        match outcome {
            // Enter failing leaves the line in the box, so it counts as typed.
            LineOutcome::Typed | LineOutcome::Failed => {
                if let Some(session) = inner.state.sessions.get_mut(key) {
                    session.reminded_at = now.format(&Rfc3339).ok();
                }
                inner.save(&shared.state_path, now);
                let age_hours = (now - started).whole_hours();
                info!(
                    session.id = %key,
                    age_hours,
                    compactions = session.compactions,
                    "chief restart reminder typed"
                );
            }
            LineOutcome::PaneBusy => {
                inner.save(&shared.state_path, now);
                inner.not_typed(key, NotTyped::PaneBusy);
            }
            LineOutcome::NoPane => {
                inner.save(&shared.state_path, now);
                inner.not_typed(key, NotTyped::NoChief);
            }
        }
    }
}

impl<P: Ports> TurnEnds for RestartReminder<P> {
    fn turn_ended(&self, pane_id: &str, session_id: &str, transcript_path: &str) {
        let reminder = Self {
            shared: Arc::clone(&self.shared),
        };
        let (pane_id, session_id, transcript_path) = (
            pane_id.to_string(),
            session_id.to_string(),
            transcript_path.to_string(),
        );
        tokio::spawn(async move {
            reminder
                .on_turn_end(&pane_id, &session_id, &transcript_path)
                .await;
        });
    }
}

impl Inner {
    /// Logs why the line was not typed, once for each session and reason.
    fn not_typed(&mut self, session_id: &str, reason: NotTyped) {
        if self.logged.insert((session_id.to_string(), reason)) {
            info!(
                session.id = %session_id,
                reason = reason.reason(),
                "chief restart reminder not typed"
            );
        }
    }

    /// Writes the state file, mode 0600, without the sessions not seen for 30 days. A
    /// failed write is logged and the run goes on with the state in memory.
    fn save(&mut self, path: &Path, now: OffsetDateTime) {
        let oldest = now.unix_timestamp() - FORGET_AFTER_SECONDS;
        self.state
            .sessions
            .retain(|_, session| session.last_seen >= oldest);
        if let Err(error) = write_state(path, &self.state) {
            warn!(error = %error, "chief restart reminder state not saved");
        }
    }
}

/// How many times, and how far apart, the line is tried while the pane reads busy.
const IDLE_TRIES: u32 = 6;
const IDLE_RETRY: std::time::Duration = std::time::Duration::from_millis(500);

/// Tries `type_line_once`, again after a short wait while the pane reads busy: Harold's
/// snapshot shows the pane idle only a moment after the turn end is stored.
async fn type_when_idle<P: Ports>(ports: &P, pane_id: &str, line: &str) -> LineOutcome {
    let mut outcome = LineOutcome::PaneBusy;
    for attempt in 0..IDLE_TRIES {
        if attempt > 0 {
            tokio::time::sleep(IDLE_RETRY).await;
        }
        outcome = type_line_once(ports, Writer::Tool("restart_reminder"), pane_id, line).await;
        if outcome != LineOutcome::PaneBusy {
            break;
        }
    }
    outcome
}

fn write_state(path: &Path, state: &State) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let bytes = serde_json::to_vec_pretty(state)?;
    let temporary = path.with_extension("json.tmp");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temporary)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    std::fs::rename(&temporary, path)
}

fn parse_time(text: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(text, &Rfc3339).ok()
}

/// More than `after_hours` old, or at least `after_compactions` compactions.
fn is_due(
    session: &Session,
    started: Option<OffsetDateTime>,
    now: OffsetDateTime,
    settings: &RestartReminderSettings,
) -> bool {
    let old = started.is_some_and(|started| {
        now - started > time::Duration::hours(i64::from(settings.after_hours))
    });
    old || session.compactions >= settings.after_compactions
}

/// The line typed into the chief's pane, with the start in local time.
fn reminder_line(started: OffsetDateTime, compactions: u32) -> String {
    let local = started.to_offset(crate::telemetry::local_offset());
    format!(
        "🔁 Harold: this chief session started {} {} {:02}:{:02} and has compacted {compactions} times. Hand off and restart at your next stopping point.",
        local.day(),
        MONTHS[usize::from(u8::from(local.month())) - 1],
        local.hour(),
        local.minute(),
    )
}

/// Reads the whole lines added to the transcript since `session.offset` into `session`.
/// A transcript that has shrunk or moved is read again from its start. A last line with
/// no newline yet is left for the next read.
fn read_new(session: &mut Session, path: &str, session_id: &str) -> std::io::Result<()> {
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    if session.transcript_path != path || length < session.offset {
        *session = Session {
            transcript_path: path.to_string(),
            last_seen: session.last_seen,
            reminded_at: session.reminded_at.take(),
            ..Session::default()
        };
    }
    file.seek(SeekFrom::Start(session.offset))?;
    let mut reader = BufReader::new(file);
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        let read = reader.read_until(b'\n', &mut buffer)?;
        if read == 0 || buffer.last() != Some(&b'\n') {
            break;
        }
        session.offset += read as u64;
        // A line that is not JSON is skipped.
        let Ok(line) = serde_json::from_slice::<Line>(&buffer) else {
            continue;
        };
        if line
            .session_id
            .as_deref()
            .is_some_and(|id| id != session_id)
        {
            continue;
        }
        if session.started.is_none() {
            session.started = line.timestamp.filter(|stamp| parse_time(stamp).is_some());
        }
        // Each compaction also writes a `compact_boundary` system line: only the summary
        // line is counted.
        if line.is_compact_summary == Some(true) {
            session.compactions += 1;
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "restart_reminder_tests.rs"]
mod tests;
