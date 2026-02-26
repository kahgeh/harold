use crate::notify::{sanitise_for_applescript, split_body};

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
    // No preceding sentence — falls back to returning the full body
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
fn sanitise_strips_newlines_and_continuation() {
    let result = sanitise_for_applescript("line1\nline2\r¬end");
    assert!(!result.contains('\n'));
    assert!(!result.contains('\r'));
    assert!(!result.contains('¬'));
    assert!(result.contains("line1"));
    assert!(result.contains("line2"));
}
