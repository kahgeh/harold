use std::io::Write as _;
use std::sync::{Arc, Mutex};

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::*;
use crate::agent::domain::EffectiveAgentState;
use crate::timed_prompts::tests::{CHIEF, Fake, Key};

const CHIEF_PANE: &str = "%2";
const SESSION: &str = "89c1a353";
/// Text that must never reach a log or the state file.
const SECRET: &str = "the launch codes are 0000";

/// A scratch folder holding the transcript and the state file, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("harold-restart-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn transcript(&self) -> String {
        self.0.join("session.jsonl").to_string_lossy().into_owned()
    }

    fn state_file(&self) -> PathBuf {
        self.0.join(STATE_FILE)
    }

    fn append(&self, lines: &[String]) {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.transcript())
            .unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn hours_ago(hours: i64) -> OffsetDateTime {
    OffsetDateTime::now_utc() - time::Duration::hours(hours)
}

fn user_line(at: OffsetDateTime) -> String {
    format!(
        r#"{{"type":"user","timestamp":"{}","sessionId":"{SESSION}","message":{{"content":"{SECRET}"}}}}"#,
        at.format(&Rfc3339).unwrap()
    )
}

fn compaction_lines(at: OffsetDateTime) -> Vec<String> {
    let stamp = at.format(&Rfc3339).unwrap();
    vec![
        format!(
            r#"{{"type":"system","subtype":"compact_boundary","timestamp":"{stamp}","sessionId":"{SESSION}","content":"{SECRET}"}}"#
        ),
        format!(
            r#"{{"type":"user","isCompactSummary":true,"timestamp":"{stamp}","sessionId":"{SESSION}","message":{{"content":"{SECRET}"}}}}"#
        ),
    ]
}

fn reminder(scratch: &Scratch, fake: &Fake) -> RestartReminder<Fake> {
    RestartReminder::new(
        Arc::new(fake.clone()),
        RestartReminderSettings::default(),
        &scratch.0,
    )
}

fn chief_fake() -> Fake {
    Fake::at("2026-10-23", "10:00:00").with_chief(CHIEF, None)
}

fn typed_lines(fake: &Fake) -> Vec<String> {
    fake.keys()
        .into_iter()
        .filter_map(|key| match key {
            Key::Text(_, text) => Some(text),
            Key::Enter(_) => None,
        })
        .collect()
}

#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl Write for Logs {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Logs {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }

    fn count(&self, message: &str) -> usize {
        self.text()
            .lines()
            .filter(|line| line.contains(message))
            .count()
    }
}

/// Captures this thread's log lines; use it from a current-thread runtime.
fn capture_logs() -> (Logs, tracing::subscriber::DefaultGuard) {
    crate::test_logs::keep_second_dispatcher();
    let logs = Logs::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::INFO)
        .finish();
    (logs, tracing::subscriber::set_default(subscriber))
}

fn session_of(scratch: &Scratch, id: &str) -> Session {
    let state: State =
        serde_json::from_slice(&std::fs::read(scratch.state_file()).unwrap()).unwrap();
    state.sessions[id].clone()
}

#[tokio::test]
async fn a_session_25_hours_old_with_no_compaction_is_due() {
    let scratch = Scratch::new();
    scratch.append(&[user_line(hours_ago(25))]);
    let fake = chief_fake();

    reminder(&scratch, &fake)
        .on_turn_end(CHIEF_PANE, SESSION, &scratch.transcript())
        .await;

    assert_eq!(typed_lines(&fake).len(), 1);
}

#[tokio::test]
async fn a_session_2_hours_old_with_two_compactions_is_due() {
    let scratch = Scratch::new();
    let mut lines = vec![user_line(hours_ago(2))];
    lines.extend(compaction_lines(hours_ago(1)));
    lines.extend(compaction_lines(hours_ago(1)));
    scratch.append(&lines);
    let fake = chief_fake();

    reminder(&scratch, &fake)
        .on_turn_end(CHIEF_PANE, SESSION, &scratch.transcript())
        .await;

    assert_eq!(typed_lines(&fake).len(), 1);
    assert!(typed_lines(&fake)[0].contains("has compacted 2 times."));
    assert_eq!(session_of(&scratch, SESSION).compactions, 2);
}

#[tokio::test]
async fn a_session_2_hours_old_with_one_compaction_is_not_due() {
    let scratch = Scratch::new();
    let mut lines = vec![user_line(hours_ago(2))];
    lines.extend(compaction_lines(hours_ago(1)));
    scratch.append(&lines);
    let fake = chief_fake();

    reminder(&scratch, &fake)
        .on_turn_end(CHIEF_PANE, SESSION, &scratch.transcript())
        .await;

    assert!(fake.keys().is_empty());
    assert_eq!(session_of(&scratch, SESSION).compactions, 1);
}

#[tokio::test]
async fn a_reminded_session_is_not_due_again_even_after_a_reload() {
    let scratch = Scratch::new();
    scratch.append(&[user_line(hours_ago(30))]);
    let fake = chief_fake();
    reminder(&scratch, &fake)
        .on_turn_end(CHIEF_PANE, SESSION, &scratch.transcript())
        .await;
    assert_eq!(typed_lines(&fake).len(), 1);

    // The same reminder, then a new one that loads the state file, as after a restart.
    let first = reminder(&scratch, &fake);
    first
        .on_turn_end(CHIEF_PANE, SESSION, &scratch.transcript())
        .await;
    reminder(&scratch, &fake)
        .on_turn_end(CHIEF_PANE, SESSION, &scratch.transcript())
        .await;

    assert_eq!(typed_lines(&fake).len(), 1);
}

#[tokio::test]
async fn a_new_session_id_starts_fresh() {
    let scratch = Scratch::new();
    scratch.append(&[user_line(hours_ago(30))]);
    let fake = chief_fake();
    let reminder = reminder(&scratch, &fake);
    reminder
        .on_turn_end(CHIEF_PANE, SESSION, &scratch.transcript())
        .await;

    // The chief restarted: a new session with a new transcript, two hours old.
    let fresh = Scratch::new();
    let line = user_line(hours_ago(2)).replace(SESSION, "second");
    fresh.append(&[line]);
    reminder
        .on_turn_end(CHIEF_PANE, "second", &fresh.transcript())
        .await;
    assert_eq!(typed_lines(&fake).len(), 1);

    // And once it is a day old it gets its own reminder.
    let old = Scratch::new();
    old.append(&[user_line(hours_ago(26)).replace(SESSION, "third")]);
    reminder
        .on_turn_end(CHIEF_PANE, "third", &old.transcript())
        .await;
    assert_eq!(typed_lines(&fake).len(), 2);
}

#[tokio::test]
async fn reading_resumes_at_the_kept_offset_and_counts_new_lines_once() {
    let scratch = Scratch::new();
    let mut lines = vec![user_line(hours_ago(2))];
    lines.extend(compaction_lines(hours_ago(2)));
    scratch.append(&lines);
    let fake = chief_fake();
    let reminder = reminder(&scratch, &fake);
    reminder
        .on_turn_end(CHIEF_PANE, SESSION, &scratch.transcript())
        .await;
    let kept = session_of(&scratch, SESSION);
    assert_eq!(kept.compactions, 1);
    assert_eq!(
        kept.offset,
        std::fs::metadata(scratch.transcript()).unwrap().len()
    );

    // A turn end with nothing added counts nothing twice.
    reminder
        .on_turn_end(CHIEF_PANE, SESSION, &scratch.transcript())
        .await;
    assert_eq!(session_of(&scratch, SESSION).compactions, 1);

    // A second compaction is read from the offset; a half-written line is left for later.
    scratch.append(&compaction_lines(hours_ago(1)));
    std::fs::OpenOptions::new()
        .append(true)
        .open(scratch.transcript())
        .unwrap()
        .write_all(br#"{"isCompactSummary":true,"timest"#)
        .unwrap();
    reminder
        .on_turn_end(CHIEF_PANE, SESSION, &scratch.transcript())
        .await;
    assert_eq!(session_of(&scratch, SESSION).compactions, 2);
    assert_eq!(typed_lines(&fake).len(), 1);
}

#[tokio::test]
async fn a_line_that_is_not_json_is_skipped() {
    let scratch = Scratch::new();
    let mut lines = vec!["not json at all".to_string(), user_line(hours_ago(30))];
    lines.push("{broken".into());
    scratch.append(&lines);
    let fake = chief_fake();

    reminder(&scratch, &fake)
        .on_turn_end(CHIEF_PANE, SESSION, &scratch.transcript())
        .await;

    assert_eq!(typed_lines(&fake).len(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn message_text_never_reaches_the_log_or_the_state_file() {
    let (logs, _guard) = capture_logs();
    let scratch = Scratch::new();
    let mut lines = vec![user_line(hours_ago(30))];
    lines.extend(compaction_lines(hours_ago(1)));
    scratch.append(&lines);
    let fake = chief_fake();

    reminder(&scratch, &fake)
        .on_turn_end(CHIEF_PANE, SESSION, &scratch.transcript())
        .await;

    assert_eq!(logs.count("chief restart reminder typed"), 1);
    assert!(!logs.text().contains(SECRET));
    let state = std::fs::read_to_string(scratch.state_file()).unwrap();
    assert!(!state.contains(SECRET));
    assert!(!typed_lines(&fake)[0].contains(SECRET));
}

#[tokio::test(flavor = "current_thread")]
async fn the_typed_log_line_names_the_session_age_and_compactions() {
    let (logs, _guard) = capture_logs();
    let scratch = Scratch::new();
    scratch.append(&[user_line(hours_ago(30))]);

    reminder(&scratch, &chief_fake())
        .on_turn_end(CHIEF_PANE, SESSION, &scratch.transcript())
        .await;

    let line = logs.text();
    for field in [
        format!("session.id={SESSION}"),
        "age_hours=30".to_string(),
        "compactions=0".to_string(),
    ] {
        assert!(line.contains(&field), "{field} not in {line}");
    }
}

#[tokio::test]
async fn the_state_file_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let scratch = Scratch::new();
    scratch.append(&[user_line(hours_ago(2))]);

    reminder(&scratch, &chief_fake())
        .on_turn_end(CHIEF_PANE, SESSION, &scratch.transcript())
        .await;

    let mode = std::fs::metadata(scratch.state_file())
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[tokio::test]
async fn a_turn_end_from_another_pane_does_nothing() {
    let scratch = Scratch::new();
    scratch.append(&[user_line(hours_ago(30))]);
    let fake = chief_fake();

    reminder(&scratch, &fake)
        .on_turn_end("%9", SESSION, &scratch.transcript())
        .await;

    assert!(fake.keys().is_empty());
    assert!(!scratch.state_file().exists());
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_pane_that_reads_idle_a_moment_after_the_turn_end_is_typed_into() {
    let scratch = Scratch::new();
    scratch.append(&[user_line(hours_ago(30))]);
    let fake = chief_fake();
    fake.set(|state| state.view.panes[0].state = EffectiveAgentState::Busy);
    let idle = fake.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(700)).await;
        idle.set(|state| state.view.panes[0].state = EffectiveAgentState::Idle);
    });

    reminder(&scratch, &fake)
        .on_turn_end(CHIEF_PANE, SESSION, &scratch.transcript())
        .await;

    assert_eq!(typed_lines(&fake).len(), 1);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_busy_pane_logs_once_and_types_at_the_next_idle_turn_end() {
    let (logs, _guard) = capture_logs();
    let scratch = Scratch::new();
    scratch.append(&[user_line(hours_ago(30))]);
    let fake = chief_fake();
    fake.set(|state| state.view.panes[0].state = EffectiveAgentState::Busy);
    let reminder = reminder(&scratch, &fake);

    for _ in 0..3 {
        reminder
            .on_turn_end(CHIEF_PANE, SESSION, &scratch.transcript())
            .await;
    }
    assert!(fake.keys().is_empty());
    assert_eq!(logs.count("chief restart reminder not typed"), 1);
    assert!(
        logs.text().contains(r#"reason="pane_busy""#),
        "{}",
        logs.text()
    );

    fake.set(|state| state.view.panes[0].state = EffectiveAgentState::Idle);
    reminder
        .on_turn_end(CHIEF_PANE, SESSION, &scratch.transcript())
        .await;
    assert_eq!(typed_lines(&fake).len(), 1);
    assert_eq!(logs.count("chief restart reminder typed"), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn a_missing_transcript_path_or_file_is_logged_once_with_its_reason() {
    let (logs, _guard) = capture_logs();
    let scratch = Scratch::new();
    let fake = chief_fake();
    let reminder = reminder(&scratch, &fake);

    for _ in 0..2 {
        reminder.on_turn_end(CHIEF_PANE, SESSION, "").await;
        reminder
            .on_turn_end(CHIEF_PANE, SESSION, &scratch.transcript())
            .await;
    }

    assert_eq!(logs.count(r#"reason="no_transcript_path""#), 1);
    assert_eq!(logs.count(r#"reason="transcript_unreadable""#), 1);
    assert!(fake.keys().is_empty());
}

#[tokio::test]
async fn the_line_names_the_start_in_local_time_and_the_compactions() {
    let started = OffsetDateTime::parse("2026-10-18T10:04:31Z", &Rfc3339).unwrap();
    let local = started.to_offset(crate::telemetry::local_offset());

    assert_eq!(
        reminder_line(started, 4),
        format!(
            "🔁 Harold: this chief session started {} {} {:02}:{:02} and has compacted 4 times. Hand off and restart at your next stopping point.",
            local.day(),
            MONTHS[usize::from(u8::from(local.month())) - 1],
            local.hour(),
            local.minute()
        )
    );
    // With the offset at UTC, as before `main` sets it.
    assert_eq!(
        reminder_line(started, 4),
        "🔁 Harold: this chief session started 18 Oct 10:04 and has compacted 4 times. Hand off and restart at your next stopping point."
    );
}

#[tokio::test]
async fn sessions_not_seen_for_30_days_are_dropped() {
    let scratch = Scratch::new();
    let stale = Session {
        last_seen: OffsetDateTime::now_utc().unix_timestamp() - 31 * 24 * 60 * 60,
        ..Session::default()
    };
    let mut state = State::default();
    state.sessions.insert("stale".into(), stale);
    write_state(&scratch.state_file(), &state).unwrap();
    scratch.append(&[user_line(hours_ago(2))]);

    reminder(&scratch, &chief_fake())
        .on_turn_end(CHIEF_PANE, SESSION, &scratch.transcript())
        .await;

    let kept: State =
        serde_json::from_slice(&std::fs::read(scratch.state_file()).unwrap()).unwrap();
    assert_eq!(kept.sessions.keys().collect::<Vec<_>>(), [SESSION]);
}

#[tokio::test]
async fn a_disabled_reminder_does_nothing() {
    let scratch = Scratch::new();
    scratch.append(&[user_line(hours_ago(30))]);
    let fake = chief_fake();
    let off = RestartReminder::new(
        Arc::new(fake.clone()),
        RestartReminderSettings {
            enabled: false,
            ..RestartReminderSettings::default()
        },
        &scratch.0,
    );

    off.on_turn_end(CHIEF_PANE, SESSION, &scratch.transcript())
        .await;

    assert!(fake.keys().is_empty());
}
