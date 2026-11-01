use super::recap::*;
use crate::settings::RecapAdapter;
use crate::transcript::Tail;

fn line(subtype: &str, session: &str, content: &str) -> Vec<u8> {
    let mut v = serde_json::to_vec(&serde_json::json!({
        "type": "system",
        "subtype": subtype,
        "sessionId": session,
        "content": content,
        "timestamp": "2026-11-02T01:02:03.500Z",
    }))
    .unwrap();
    v.push(b'\n');
    v
}

fn read(bytes: &[u8], session: &str) -> Option<RecapLine> {
    read_recap_line(RecapAdapter::ClaudeTranscriptV1, bytes, session)
}

#[test]
fn adapter_takes_an_away_summary_of_the_kept_session() {
    let got = read(&line("away_summary", "s1", "Fixing the build."), "s1").unwrap();
    assert_eq!(got.text, "Fixing the build.");
    assert_eq!(got.at_ms, 1_793_581_323_500);
}

#[test]
fn adapter_skips_another_session() {
    assert_eq!(read(&line("away_summary", "s2", "Hello."), "s1"), None);
}

#[test]
fn adapter_removes_the_note() {
    let got = read(
        &line(
            "away_summary",
            "s1",
            "Fixing it. (disable recaps in /config)",
        ),
        "s1",
    )
    .unwrap();
    assert_eq!(got.text, "Fixing it.");
}

#[test]
fn adapter_skips_a_user_line_that_quotes_away_summary() {
    let bytes = serde_json::to_vec(&serde_json::json!({
        "type": "user", "sessionId": "s1", "message": {"content": "what is \"away_summary\"?"},
        "timestamp": "2026-11-02T01:02:03Z",
    }))
    .unwrap();
    assert_eq!(read(&bytes, "s1"), None);
}

#[test]
fn adapter_skips_a_line_that_is_not_json() {
    assert_eq!(read(b"\"away_summary\" {not json\n", "s1"), None);
}

#[test]
fn adapter_gives_none_for_a_recap_that_is_only_the_note() {
    assert_eq!(
        read(
            &line("away_summary", "s1", " (disable recaps in /config)"),
            "s1"
        ),
        None
    );
}

#[test]
fn adapter_none_reads_nothing() {
    let bytes = line("away_summary", "s1", "Hello.");
    assert_eq!(read_recap_line(RecapAdapter::None, &bytes, "s1"), None);
}

#[test]
fn a_recap_of_345_chars_stays_whole() {
    let text = "a".repeat(345);
    assert_eq!(
        read(&line("away_summary", "s1", &text), "s1").unwrap().text,
        text
    );
}

#[test]
fn a_recap_of_400_chars_stays_whole() {
    let text = "a".repeat(400);
    let got = read(&line("away_summary", "s1", &text), "s1").unwrap();
    assert_eq!(got.text, text);
}

#[test]
fn a_recap_of_401_chars_is_cut_to_400() {
    let text = "a".repeat(401);
    let got = read(&line("away_summary", "s1", &text), "s1").unwrap();
    assert_eq!(got.text.chars().count(), 400);
}

fn recap(text: &str) -> RecapLine {
    RecapLine {
        text: text.into(),
        at_ms: 1,
    }
}

#[test]
fn store_get_needs_the_same_agent_pid() {
    let store = Recaps::default();
    store.note_turn_end("p1", 10, "claude", "s1", "/t/a.jsonl");
    store.update_read(
        "p1",
        10,
        "s1",
        "/t/a.jsonl",
        Tail { offset: 5 },
        Some(recap("x")),
    );
    assert_eq!(store.get("p1", 10), Some(recap("x")));
    assert_eq!(store.get("p1", 11), None);
    store.update_read(
        "p1",
        11,
        "s1",
        "/t/a.jsonl",
        Tail { offset: 9 },
        Some(recap("y")),
    );
    assert_eq!(
        store.get("p1", 10),
        Some(recap("x")),
        "a stale pid writes nothing"
    );
}

#[test]
fn a_new_session_id_drops_the_old_recap() {
    let store = Recaps::default();
    store.note_turn_end("p1", 10, "claude", "s1", "/t/a.jsonl");
    store.update_read(
        "p1",
        10,
        "s1",
        "/t/a.jsonl",
        Tail { offset: 5 },
        Some(recap("x")),
    );
    store.note_turn_end("p1", 10, "claude", "s2", "/t/b.jsonl");
    assert_eq!(store.get("p1", 10), None);
    let targets = store.targets();
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].session_id, "s2");
    assert_eq!(targets[0].tail, Tail { offset: 0 });
    store.note_turn_end("p1", 12, "claude", "s2", "/t/b.jsonl");
    assert_eq!(store.targets()[0].agent_pid, 12);
}

#[test]
fn a_new_version_only_when_the_recap_changes() {
    let store = Recaps::default();
    let rx = store.subscribe();
    store.note_turn_end("p1", 10, "claude", "s1", "/t/a.jsonl");
    let v0 = *rx.borrow();
    store.update_read("p1", 10, "s1", "/t/a.jsonl", Tail { offset: 5 }, None);
    assert_eq!(*rx.borrow(), v0, "no recap, no change");
    store.update_read(
        "p1",
        10,
        "s1",
        "/t/a.jsonl",
        Tail { offset: 6 },
        Some(recap("x")),
    );
    assert_eq!(*rx.borrow(), v0 + 1);
    store.update_read(
        "p1",
        10,
        "s1",
        "/t/a.jsonl",
        Tail { offset: 7 },
        Some(recap("x")),
    );
    assert_eq!(*rx.borrow(), v0 + 1, "same recap, offset moves only");
    store.update_read("p1", 10, "s1", "/t/a.jsonl", Tail { offset: 8 }, None);
    assert_eq!(*rx.borrow(), v0 + 1, "no new recap keeps the old one");
    assert_eq!(store.get("p1", 10), Some(recap("x")));
    store.forget_missing(&[]);
    assert_eq!(*rx.borrow(), v0 + 2);
    assert_eq!(store.get("p1", 10), None);
}

#[test]
fn a_stale_read_after_a_new_session_changes_nothing() {
    let store = Recaps::default();
    store.note_turn_end("p1", 10, "claude", "s1", "/t/a.jsonl");
    store.note_turn_end("p1", 10, "claude", "s2", "/t/b.jsonl");
    let rx = store.subscribe();
    let v0 = *rx.borrow();
    store.update_read(
        "p1",
        10,
        "s1",
        "/t/a.jsonl",
        Tail { offset: 50 },
        Some(recap("old")),
    );
    assert_eq!(*rx.borrow(), v0);
    assert_eq!(store.get("p1", 10), None);
    assert_eq!(store.targets()[0].tail, Tail { offset: 0 });
    store.update_read(
        "p1",
        10,
        "s2",
        "/t/a.jsonl",
        Tail { offset: 50 },
        Some(recap("old")),
    );
    assert_eq!(store.get("p1", 10), None, "a stale path writes nothing");
}

#[test]
fn a_changed_path_resets_the_tail_and_keeps_the_recap() {
    let store = Recaps::default();
    store.note_turn_end("p1", 10, "claude", "s1", "/t/a.jsonl");
    store.update_read(
        "p1",
        10,
        "s1",
        "/t/a.jsonl",
        Tail { offset: 5 },
        Some(recap("x")),
    );
    store.note_turn_end("p1", 10, "claude", "s1", "/t/c.jsonl");
    assert_eq!(store.get("p1", 10), Some(recap("x")));
    let target = &store.targets()[0];
    assert_eq!(target.tail, Tail { offset: 0 });
    assert_eq!(target.transcript_path, "/t/c.jsonl");
}

#[test]
fn a_recap_line_debug_omits_the_text() {
    assert!(!format!("{:?}", recap("secret words")).contains("secret"));
}

mod reader {
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use super::line;
    use crate::agent::domain::{AgentSnapshot, EffectiveAgentState};
    use crate::agent::recap::*;
    use crate::settings::{AgentProviderSettings, RecapAdapter};
    use crate::store::EventVersion;
    use crate::telemetry::test_support;
    use crate::timed_prompts::tests::projection;

    const PID: u32 = 20;
    const SECRET: &str = "the launch codes are 0000";

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("harold-recap-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn file(&self) -> String {
            self.0.join("session.jsonl").to_string_lossy().into_owned()
        }

        fn append(&self, bytes: &[u8]) {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.file())
                .unwrap();
            file.write_all(bytes).unwrap();
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
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

    fn provider(id: &str, adapter: RecapAdapter) -> AgentProviderSettings {
        AgentProviderSettings {
            id: id.into(),
            display_name: id.into(),
            command_contains: vec![id.into()],
            busy_all: Vec::new(),
            idle_all: Vec::new(),
            summary_line_prefixes: Vec::new(),
            screen_adapter: Default::default(),
            recap_adapter: adapter,
            screen_history_lines: 10,
        }
    }

    fn snapshot(pane_id: &str, provider_id: &str) -> AgentSnapshot {
        AgentSnapshot {
            through_event_version: EventVersion::start(),
            server_time_ms: 1,
            monitor_health: Vec::new(),
            panes: vec![projection(
                pane_id,
                provider_id,
                "/tmp",
                EffectiveAgentState::Idle,
            )],
            tts_muted: false,
            messaging_paused: false,
        }
    }

    fn reader(recaps: &Recaps, provider_id: &str, adapter: RecapAdapter) -> RecapReader {
        RecapReader::new(
            recaps.clone(),
            vec![provider(provider_id, adapter)],
            Duration::from_millis(200),
        )
    }

    fn count(provider_id: &str, result: &str) -> u64 {
        test_support::exact_counter_value(
            "harold.agent.recap_read",
            &[("result", result), ("provider_id", provider_id)],
        )
        .unwrap_or(0)
    }

    fn id() -> String {
        format!("p{}", uuid::Uuid::new_v4().simple())
    }

    #[tokio::test]
    async fn no_transcript_is_read_for_a_provider_without_an_adapter() {
        test_support::install();
        let (logs, _guard) = capture_logs();
        let provider_id = id();
        let recaps = Recaps::default();
        recaps.note_turn_end("%1", PID, &provider_id, "s1", "/nonexistent/none.jsonl");
        let mut reader = reader(&recaps, &provider_id, RecapAdapter::None);
        reader.check_once(&snapshot("%1", &provider_id)).await;
        assert_eq!(count(&provider_id, "failed"), 0);
        assert_eq!(count(&provider_id, "found"), 0);
        assert_eq!(count(&provider_id, "none"), 0);
        assert_eq!(logs.count("agent recap read failed"), 0);
    }

    #[tokio::test]
    async fn the_last_away_summary_of_the_kept_session_is_kept() {
        test_support::install();
        let provider_id = id();
        let scratch = Scratch::new();
        let mut bytes = line("away_summary", "s1", "First.");
        bytes.extend(line("away_summary", "other", "Other session."));
        bytes.extend(line("away_summary", "s1", "Second."));
        scratch.append(&bytes);
        let recaps = Recaps::default();
        recaps.note_turn_end("%1", PID, &provider_id, "s1", &scratch.file());
        let mut reader = reader(&recaps, &provider_id, RecapAdapter::ClaudeTranscriptV1);
        reader.check_once(&snapshot("%1", &provider_id)).await;
        assert_eq!(recaps.get("%1", PID).unwrap().text, "Second.");
        assert_eq!(count(&provider_id, "found"), 1);
    }

    #[tokio::test]
    async fn a_check_with_no_growth_reads_nothing() {
        test_support::install();
        let provider_id = id();
        let scratch = Scratch::new();
        scratch.append(&line("user", "s1", "hello"));
        let recaps = Recaps::default();
        recaps.note_turn_end("%1", PID, &provider_id, "s1", &scratch.file());
        let mut reader = reader(&recaps, &provider_id, RecapAdapter::ClaudeTranscriptV1);
        let snap = snapshot("%1", &provider_id);
        reader.check_once(&snap).await;
        reader.check_once(&snap).await;
        assert_eq!(count(&provider_id, "none"), 1);
    }

    #[tokio::test]
    async fn a_first_window_with_no_newline_is_not_read_again_while_the_file_is_unchanged() {
        test_support::install();
        let provider_id = id();
        let scratch = Scratch::new();
        scratch.append(&vec![
            b'x';
            crate::transcript::FIRST_READ_LIMIT as usize + 10
        ]);
        let recaps = Recaps::default();
        recaps.note_turn_end("%1", PID, &provider_id, "s1", &scratch.file());
        let mut reader = reader(&recaps, &provider_id, RecapAdapter::ClaudeTranscriptV1);
        let snap = snapshot("%1", &provider_id);
        reader.check_once(&snap).await;
        reader.check_once(&snap).await;
        assert_eq!(count(&provider_id, "none"), 1);
        scratch.append(b"y");
        reader.check_once(&snap).await;
        assert_eq!(count(&provider_id, "none"), 2);
    }

    #[tokio::test]
    async fn a_file_that_grew_with_no_new_recap_counts_none() {
        test_support::install();
        let provider_id = id();
        let scratch = Scratch::new();
        scratch.append(&line("away_summary", "s1", "Kept."));
        let recaps = Recaps::default();
        recaps.note_turn_end("%1", PID, &provider_id, "s1", &scratch.file());
        let mut reader = reader(&recaps, &provider_id, RecapAdapter::ClaudeTranscriptV1);
        let snap = snapshot("%1", &provider_id);
        reader.check_once(&snap).await;
        scratch.append(&line("user", "s1", "more"));
        reader.check_once(&snap).await;
        assert_eq!(count(&provider_id, "found"), 1);
        assert_eq!(count(&provider_id, "none"), 1);
        assert_eq!(recaps.get("%1", PID).unwrap().text, "Kept.");
    }

    #[tokio::test]
    async fn an_unreadable_file_logs_once_in_three_checks_and_again_after_a_good_read() {
        test_support::install();
        let (logs, _guard) = capture_logs();
        let provider_id = id();
        let scratch = Scratch::new();
        let recaps = Recaps::default();
        recaps.note_turn_end("%1", PID, &provider_id, "s1", &scratch.file());
        let mut reader = reader(&recaps, &provider_id, RecapAdapter::ClaudeTranscriptV1);
        let snap = snapshot("%1", &provider_id);
        for _ in 0..3 {
            reader.check_once(&snap).await;
        }
        assert_eq!(logs.count("agent recap read failed"), 1);
        assert!(logs.text().contains("reason=not_found"), "{}", logs.text());
        assert_eq!(count(&provider_id, "failed"), 3);
        scratch.append(&line("user", "s1", "hello"));
        reader.check_once(&snap).await;
        std::fs::remove_file(scratch.file()).unwrap();
        reader.check_once(&snap).await;
        reader.check_once(&snap).await;
        assert_eq!(logs.count("agent recap read failed"), 2);
    }

    #[tokio::test]
    async fn a_read_over_200ms_logs_slow() {
        test_support::install();
        let (logs, _guard) = capture_logs();
        let provider_id = id();
        let scratch = Scratch::new();
        scratch.append(&line("user", "s1", "hello"));
        let recaps = Recaps::default();
        recaps.note_turn_end("%1", PID, &provider_id, "s1", &scratch.file());
        let providers = vec![provider(&provider_id, RecapAdapter::ClaudeTranscriptV1)];
        let mut slow = RecapReader::new(recaps.clone(), providers.clone(), Duration::ZERO);
        slow.check_once(&snapshot("%1", &provider_id)).await;
        assert_eq!(logs.count("agent recap read slow"), 1);
        let text = logs.text();
        assert!(text.contains("elapsed_ms="), "{text}");
        assert!(text.contains("bytes="), "{text}");
        assert!(text.contains("pane_id=%1"), "{text}");
        // The default threshold stays quiet for a small read.
        scratch.append(&line("user", "s1", "more"));
        let mut quick = reader(&recaps, &provider_id, RecapAdapter::ClaudeTranscriptV1);
        quick.check_once(&snapshot("%1", &provider_id)).await;
        assert_eq!(logs.count("agent recap read slow"), 1);
    }

    #[tokio::test]
    async fn a_slow_read_that_then_fails_logs_slow_with_zero_bytes() {
        test_support::install();
        let (logs, _guard) = capture_logs();
        let provider_id = id();
        let scratch = Scratch::new();
        // A folder opens and has a length, but a read of it fails.
        let recaps = Recaps::default();
        recaps.note_turn_end("%1", PID, &provider_id, "s1", &scratch.0.to_string_lossy());
        let providers = vec![provider(&provider_id, RecapAdapter::ClaudeTranscriptV1)];
        let mut reader = RecapReader::new(recaps, providers, Duration::ZERO);
        reader.check_once(&snapshot("%1", &provider_id)).await;
        assert_eq!(count(&provider_id, "failed"), 1);
        assert_eq!(logs.count("agent recap read failed"), 1);
        assert_eq!(logs.count("agent recap read slow"), 1);
        assert!(logs.text().contains("bytes=0"), "{}", logs.text());
    }

    #[tokio::test]
    async fn a_shrunk_file_is_read_again_with_the_first_read_limit() {
        test_support::install();
        let provider_id = id();
        let scratch = Scratch::new();
        let limit = crate::transcript::FIRST_READ_LIMIT as usize;
        let filler = line("user", "s1", &"x".repeat(1000));
        let pad = |bytes: usize| filler.repeat(bytes / filler.len());
        let mut first = pad(limit + 4 * 1024 * 1024);
        first.extend(line("away_summary", "s1", "Recap A."));
        scratch.append(&first);
        let recaps = Recaps::default();
        recaps.note_turn_end("%1", PID, &provider_id, "s1", &scratch.file());
        let mut reader = reader(&recaps, &provider_id, RecapAdapter::ClaudeTranscriptV1);
        let snap = snapshot("%1", &provider_id);
        reader.check_once(&snap).await;
        assert_eq!(recaps.get("%1", PID).unwrap().text, "Recap A.");
        // The file is rewritten smaller, but still longer than the limit. Its recap lies
        // before the limit window.
        std::fs::remove_file(scratch.file()).unwrap();
        let mut second = line("away_summary", "s1", "Recap B.");
        second.extend(pad(limit + 1024 * 1024));
        scratch.append(&second);
        reader.check_once(&snap).await;
        assert_eq!(recaps.get("%1", PID).unwrap().text, "Recap A.");
        assert_eq!(count(&provider_id, "found"), 1);
        assert_eq!(count(&provider_id, "none"), 1);
        // A recap inside the window is found.
        scratch.append(&line("away_summary", "s1", "Recap C."));
        reader.check_once(&snap).await;
        assert_eq!(recaps.get("%1", PID).unwrap().text, "Recap C.");
    }

    #[tokio::test]
    async fn a_recap_read_again_counts_none() {
        test_support::install();
        let provider_id = id();
        let scratch = Scratch::new();
        scratch.append(&line("away_summary", "s1", "Same."));
        let other = scratch.0.join("copy.jsonl");
        std::fs::copy(scratch.file(), &other).unwrap();
        let recaps = Recaps::default();
        recaps.note_turn_end("%1", PID, &provider_id, "s1", &scratch.file());
        let mut reader = reader(&recaps, &provider_id, RecapAdapter::ClaudeTranscriptV1);
        let snap = snapshot("%1", &provider_id);
        reader.check_once(&snap).await;
        recaps.note_turn_end("%1", PID, &provider_id, "s1", &other.to_string_lossy());
        reader.check_once(&snap).await;
        assert_eq!(count(&provider_id, "found"), 1);
        assert_eq!(count(&provider_id, "none"), 1);
        assert_eq!(recaps.get("%1", PID).unwrap().text, "Same.");
    }

    #[tokio::test]
    async fn the_recap_text_is_not_in_the_captured_logs() {
        test_support::install();
        let (logs, _guard) = capture_logs();
        let provider_id = id();
        let scratch = Scratch::new();
        scratch.append(&line("away_summary", "s1", SECRET));
        let recaps = Recaps::default();
        recaps.note_turn_end("%1", PID, &provider_id, "s1", &scratch.file());
        let providers = vec![provider(&provider_id, RecapAdapter::ClaudeTranscriptV1)];
        let mut reader = RecapReader::new(recaps.clone(), providers, Duration::ZERO);
        reader.check_once(&snapshot("%1", &provider_id)).await;
        std::fs::remove_file(scratch.file()).unwrap();
        reader.check_once(&snapshot("%1", &provider_id)).await;
        assert_eq!(recaps.get("%1", PID).unwrap().text, SECRET);
        assert!(!logs.text().contains("launch codes"), "{}", logs.text());
    }

    #[tokio::test]
    async fn a_pane_that_left_the_snapshot_is_forgotten() {
        test_support::install();
        let provider_id = id();
        let scratch = Scratch::new();
        scratch.append(&line("away_summary", "s1", "Kept."));
        let recaps = Recaps::default();
        recaps.note_turn_end("%1", PID, &provider_id, "s1", &scratch.file());
        let mut reader = reader(&recaps, &provider_id, RecapAdapter::ClaudeTranscriptV1);
        reader.check_once(&snapshot("%1", &provider_id)).await;
        assert!(recaps.get("%1", PID).is_some());
        reader.check_once(&snapshot("%2", &provider_id)).await;
        assert!(recaps.get("%1", PID).is_none());
    }
}
