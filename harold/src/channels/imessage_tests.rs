use crate::channels::{split_body, truncate_body};
use crate::util::sanitise_for_applescript;

use super::{
    NotificationPlan, confirmation_text, is_marked_as_harold, marked_as_harold, notification_plan,
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

#[test]
fn listener_skips_every_message_harold_sends() {
    // Messages to the user's own number are stored as both sent and received rows,
    // and the listener reads both, so anything unmarked is routed back in as a reply.
    for reply in [
        "✓ Delivered to [harold  main:0.3]",
        "No active pane found. Available: harold  main:0.3",
        "No active agent sessions found.",
    ] {
        assert!(
            is_marked_as_harold(&confirmation_text(reply)),
            "{reply:?} would be read back as an inbound message"
        );
    }
    assert!(is_marked_as_harold(&marked_as_harold(
        "[harold:0.1] Work is done."
    )));
}

#[test]
fn listener_keeps_messages_the_user_wrote() {
    for text in ["Yes", "[harold  main:0.3] carry on", "✓ sounds good"] {
        assert!(!is_marked_as_harold(text), "{text:?}");
    }
}
