use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use crate::channels::{split_body, truncate_body};
use crate::store::HaroldStore;
use crate::util::sanitise_for_applescript;

use super::{
    Direction, FetchedRow, NotificationPlan, RecentRows, TWIN_WINDOW, is_marked_as_harold,
    notification_plan, record_row, send_script,
};

#[test]
fn split_body_no_question() {
    let (main, q) = split_body("Work is done. All good.");
    assert_eq!(main, "Work is done. All good.");
    assert_eq!(q, None);
}

#[test]
fn split_body_trailing_question() {
    let (main, q) = split_body("Build succeeded. Should I deploy?");
    assert_eq!(main, "Build succeeded.");
    assert_eq!(q, Some("Should I deploy?"));
}

#[test]
fn split_body_only_question() {
    let (main, q) = split_body("Should I deploy?");
    assert_eq!(main, "Should I deploy?");
    assert_eq!(q, None);
}

#[test]
fn split_body_multiple_sentences_with_question() {
    let (main, q) = split_body("Done. Tests pass. Ready to merge. Shall I open a PR?");
    assert_eq!(main, "Done. Tests pass. Ready to merge.");
    assert_eq!(q, Some("Shall I open a PR?"));
}

#[test]
fn truncate_body_caps_at_280_chars_and_flattens_newlines() {
    let short = "Hello world.\nDone.";
    assert_eq!(truncate_body(short), "Hello world. Done.");

    let long: String = "x".repeat(300);
    let result = truncate_body(&long);
    assert_eq!(result.len(), 280);

    // Multi-byte: caps at 280 *characters*, not bytes.
    let emoji_long: String = "\u{1F600}".repeat(300);
    let result = truncate_body(&emoji_long);
    assert_eq!(result.chars().count(), 280);
    assert!(result.len() > 280);
}

#[test]
fn sanitise_strips_newlines_and_continuation() {
    let result = sanitise_for_applescript("line1\nline2\r¬end");
    assert!(!result.contains('\n'));
    assert!(!result.contains('\r'));
    assert!(!result.contains('¬'));
    assert!(result.contains("line1"));
    assert!(result.contains("line2"));
}

#[test]
fn retry_after_main_only_resumes_with_the_missing_question() {
    let plan = notification_plan(
        &["🤖 [harold:0.1] Work is done. (harold)"],
        "[harold:0.1] Work is done. (harold)",
        Some("Should I deploy?"),
    );

    assert_eq!(plan, NotificationPlan::SendQuestionOnly("Should I deploy?"));
}

#[test]
fn retry_after_both_parts_skips_the_completed_notification() {
    let plan = notification_plan(
        &[
            "🤖 Should I deploy?",
            "🤖 [harold:0.1] Work is done. (harold)",
        ],
        "[harold:0.1] Work is done. (harold)",
        Some("Should I deploy?"),
    );

    assert_eq!(plan, NotificationPlan::Skip);
}

#[test]
fn matching_question_from_another_turn_sends_the_full_notification() {
    let plan = notification_plan(
        &[
            "🤖 Should I deploy?",
            "🤖 [other:0.1] Different work. (other)",
        ],
        "[harold:0.1] Work is done. (harold)",
        Some("Should I deploy?"),
    );

    assert_eq!(plan, NotificationPlan::SendAll);
}

/// The text Messages is told to send, as the listener later reads it from chat.db.
fn sent_text(text: &str) -> String {
    let script = send_script(text, "+61400000000");
    let body = script
        .strip_prefix("tell application \"Messages\" to send \"")
        .expect("send script prefix");
    let end = body
        .rfind("\" to buddy \"")
        .expect("send script recipient clause");
    body[..end].to_string()
}

#[test]
fn listener_skips_every_message_harold_sends() {
    // Messages to the user's own number are stored as both sent and received rows,
    // and the listener reads both, so anything unmarked is routed back in as a reply.
    for text in [
        "✓ Delivered to [harold  main:0.3]",
        "No active pane found. Available: harold  main:0.3",
        "No active agent sessions found.",
        "[harold:0.1] Work is done. (harold)",
        "Should I deploy?",
    ] {
        let sent = sent_text(text);
        assert!(
            is_marked_as_harold(sent.trim()),
            "{text:?} is sent as {sent:?} and would be read back as an inbound message"
        );
        assert!(sent.ends_with(text), "{sent:?} lost its body");
    }
}

#[test]
fn send_script_escapes_quotes_and_backslashes_after_marking() {
    assert_eq!(
        send_script("say \"hi\" \\ bye", "bud\"dy"),
        "tell application \"Messages\" to send \"🤖 say \\\"hi\\\" \\\\ bye\" to buddy \"bud\\\"dy\""
    );
}

#[test]
fn listener_keeps_messages_the_user_wrote() {
    for text in ["Yes", "[harold  main:0.3] carry on", "✓ sounds good"] {
        assert!(!is_marked_as_harold(text), "{text:?}");
    }
}

struct TestDirectory(std::path::PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("harold-imessage-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("create test directory");
        Self(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn row(rowid: i64, text: &str, direction: Direction) -> FetchedRow {
    FetchedRow {
        rowid,
        text: text.into(),
        direction,
    }
}

#[tokio::test]
async fn paused_listener_advances_the_cursor_and_appends_nothing() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(&directory.0).await.unwrap();
    let cursor = AtomicI64::new(10);
    let recent = Mutex::new(RecentRows::new());
    let now = Instant::now();

    let confirmation = row(11, "✓ Delivered to [harold:0.1]", Direction::Inbound);
    record_row(&store, confirmation, &cursor, &recent, true, now).await;
    let reply = row(12, "carry on", Direction::Inbound);
    record_row(&store, reply, &cursor, &recent, true, now).await;

    assert_eq!(cursor.load(Ordering::Relaxed), 12);
    assert_eq!(store.project_unhandled_events(10).await.unwrap().applied, 0);
    assert!(store.next_pending_delivery().await.unwrap().is_none());
}

#[tokio::test]
async fn running_listener_appends_the_row_and_advances_the_cursor() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(&directory.0).await.unwrap();
    let cursor = AtomicI64::new(10);
    let recent = Mutex::new(RecentRows::new());

    let reply = row(11, "carry on", Direction::Inbound);
    record_row(&store, reply, &cursor, &recent, false, Instant::now()).await;

    assert_eq!(cursor.load(Ordering::Relaxed), 11);
    assert_eq!(store.project_unhandled_events(10).await.unwrap().applied, 1);
    let delivery = store.next_pending_delivery().await.unwrap().unwrap();
    assert_eq!(delivery.event_type, "InboundMessageReceived");
}

#[test]
fn same_text_from_the_other_direction_within_the_window_is_a_duplicate() {
    let start = Instant::now();
    let soon = start + Duration::from_millis(5);

    let mut recent = RecentRows::new();
    recent.remember(Direction::Inbound, "Yes".into(), start);
    assert!(recent.take_twin(Direction::SelfSent, "Yes", soon));

    let mut recent = RecentRows::new();
    recent.remember(Direction::SelfSent, "Yes".into(), start);
    assert!(recent.take_twin(Direction::Inbound, "Yes", start + TWIN_WINDOW));
}

#[test]
fn recorded_row_absorbs_only_one_twin() {
    let start = Instant::now();
    let mut recent = RecentRows::new();
    recent.remember(Direction::SelfSent, "Yes".into(), start);

    // The wrong direction neither matches nor uses the entry up.
    assert!(!recent.take_twin(Direction::SelfSent, "Yes", start));
    assert!(recent.take_twin(Direction::Inbound, "Yes", start));
    assert!(recent.0.is_empty());
    assert!(!recent.take_twin(Direction::Inbound, "Yes", start));
}

#[test]
fn twin_takes_the_oldest_matching_entry() {
    let start = Instant::now();
    let later = start + Duration::from_secs(6);
    let mut recent = RecentRows::new();
    recent.remember(Direction::Inbound, "Yes".into(), start);
    recent.remember(Direction::SelfSent, "Yes".into(), start);
    recent.remember(Direction::Inbound, "Yes".into(), later);

    assert!(recent.take_twin(Direction::SelfSent, "Yes", later));
    assert_eq!(
        recent.0,
        [
            (Direction::SelfSent, "Yes".to_string(), start),
            (Direction::Inbound, "Yes".to_string(), later),
        ]
    );
}

#[test]
fn same_text_from_the_same_direction_is_kept() {
    let start = Instant::now();
    let mut recent = RecentRows::new();
    recent.remember(Direction::Inbound, "Yes".into(), start);

    assert!(!recent.take_twin(Direction::Inbound, "Yes", start + Duration::from_millis(5)));
}

#[test]
fn different_text_from_the_other_direction_is_kept() {
    let start = Instant::now();
    let mut recent = RecentRows::new();
    recent.remember(Direction::Inbound, "Yes".into(), start);

    assert!(!recent.take_twin(Direction::SelfSent, "Yes please", start));
    assert!(!recent.take_twin(Direction::SelfSent, "yes", start));
}

#[test]
fn same_text_from_the_other_direction_outside_the_window_is_kept() {
    let start = Instant::now();
    let mut recent = RecentRows::new();
    recent.remember(Direction::Inbound, "Yes".into(), start);

    let late = start + TWIN_WINDOW + Duration::from_millis(1);
    assert!(!recent.take_twin(Direction::SelfSent, "Yes", late));
}

#[test]
fn recent_rows_forget_entries_older_than_the_window() {
    let start = Instant::now();
    let mut recent = RecentRows::new();
    for second in 0..5 {
        recent.remember(
            Direction::Inbound,
            "Yes".into(),
            start + Duration::from_secs(second),
        );
    }
    assert_eq!(recent.0.len(), 5);

    // Both a lookup and a new entry drop what has aged out.
    recent.take_twin(
        Direction::SelfSent,
        "No",
        start + TWIN_WINDOW + Duration::from_millis(2_500),
    );
    assert_eq!(recent.0.len(), 2);
    recent.remember(Direction::SelfSent, "No".into(), start + TWIN_WINDOW * 2);
    assert_eq!(recent.0.len(), 1);
}

#[tokio::test]
async fn reply_stored_as_an_inbound_and_a_self_row_is_recorded_once() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(&directory.0).await.unwrap();
    let inbound_cursor = AtomicI64::new(10);
    let self_cursor = AtomicI64::new(10);
    let recent = Mutex::new(RecentRows::new());
    let now = Instant::now();

    // One poll: inbound rows first, then self rows.
    let inbound = row(12, "carry on", Direction::Inbound);
    record_row(&store, inbound, &inbound_cursor, &recent, false, now).await;
    let twin = row(11, "carry on", Direction::SelfSent);
    record_row(&store, twin, &self_cursor, &recent, false, now).await;

    assert_eq!(inbound_cursor.load(Ordering::Relaxed), 12);
    assert_eq!(self_cursor.load(Ordering::Relaxed), 11);
    assert_eq!(store.project_unhandled_events(10).await.unwrap().applied, 1);

    // The copies can also land in different polls, in either order.
    let later = now + Duration::from_secs(5);
    let own = row(13, "and then stop", Direction::SelfSent);
    record_row(&store, own, &self_cursor, &recent, false, now).await;
    let twin = row(14, "and then stop", Direction::Inbound);
    record_row(&store, twin, &inbound_cursor, &recent, false, later).await;

    assert_eq!(self_cursor.load(Ordering::Relaxed), 13);
    assert_eq!(inbound_cursor.load(Ordering::Relaxed), 14);
    assert_eq!(store.project_unhandled_events(10).await.unwrap().applied, 1);
}

#[tokio::test]
async fn same_reply_sent_twice_from_the_phone_is_recorded_twice() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(&directory.0).await.unwrap();
    let cursor = AtomicI64::new(10);
    let recent = Mutex::new(RecentRows::new());
    let now = Instant::now();

    record_row(
        &store,
        row(11, "Yes", Direction::Inbound),
        &cursor,
        &recent,
        false,
        now,
    )
    .await;
    record_row(
        &store,
        row(12, "Yes", Direction::Inbound),
        &cursor,
        &recent,
        false,
        now,
    )
    .await;

    assert_eq!(cursor.load(Ordering::Relaxed), 12);
    assert_eq!(store.project_unhandled_events(10).await.unwrap().applied, 2);
}

#[tokio::test]
async fn row_discarded_by_the_pause_is_not_remembered() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(&directory.0).await.unwrap();
    let inbound_cursor = AtomicI64::new(10);
    let self_cursor = AtomicI64::new(10);
    let recent = Mutex::new(RecentRows::new());
    let now = Instant::now();

    let discarded = row(11, "carry on", Direction::Inbound);
    record_row(&store, discarded, &inbound_cursor, &recent, true, now).await;
    assert!(recent.lock().unwrap().0.is_empty());

    // Messaging resumes before the other copy is read: it is the only one recorded.
    let twin = row(12, "carry on", Direction::SelfSent);
    record_row(&store, twin, &self_cursor, &recent, false, now).await;

    assert_eq!(self_cursor.load(Ordering::Relaxed), 12);
    assert_eq!(store.project_unhandled_events(10).await.unwrap().applied, 1);
}

/// Feeds rows through `record_row` in the given order, a second apart, and returns how
/// many were recorded.
async fn recorded_count(directions: &[Direction]) -> usize {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(&directory.0).await.unwrap();
    let inbound_cursor = AtomicI64::new(10);
    let self_cursor = AtomicI64::new(10);
    let recent = Mutex::new(RecentRows::new());
    let start = Instant::now();

    let mut rowid = 10;
    for &direction in directions {
        rowid += 1;
        let cursor = match direction {
            Direction::Inbound => &inbound_cursor,
            Direction::SelfSent => &self_cursor,
        };
        let now = start + Duration::from_secs((rowid - 10) as u64);
        record_row(
            &store,
            row(rowid, "Yes", direction),
            cursor,
            &recent,
            false,
            now,
        )
        .await;
        assert_eq!(cursor.load(Ordering::Relaxed), rowid);
    }
    store.project_unhandled_events(10).await.unwrap().applied
}

#[tokio::test]
async fn phone_reply_after_the_same_text_from_the_mac_is_recorded() {
    use Direction::{Inbound, SelfSent};

    // The Mac pair in either arrival order, then a single row from the phone.
    assert_eq!(recorded_count(&[SelfSent, Inbound, Inbound]).await, 2);
    assert_eq!(recorded_count(&[Inbound, SelfSent, Inbound]).await, 2);
}

#[tokio::test]
async fn same_reply_typed_twice_on_the_mac_is_recorded_twice() {
    use Direction::{Inbound, SelfSent};

    for order in [
        [Inbound, SelfSent, Inbound, SelfSent],
        [Inbound, SelfSent, SelfSent, Inbound],
        [SelfSent, Inbound, Inbound, SelfSent],
        [SelfSent, Inbound, SelfSent, Inbound],
        // Both pairs read in one poll: inbound rows first, then self rows.
        [Inbound, Inbound, SelfSent, SelfSent],
    ] {
        assert_eq!(recorded_count(&order).await, 2, "{order:?}");
    }
}
