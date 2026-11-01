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
