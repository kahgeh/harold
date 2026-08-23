use super::domain::{CompletionSummaryUpdate, WorkSummaryUpdate};
use super::summary::{completion_summary_update, explicit_summary_update, normalize_work_summary};

#[test]
fn normalizer_removes_terminal_controls_and_collapses_whitespace() {
    let cases = [
        (
            "  implement\tprojector\nnow  ",
            Some("implement projector now"),
        ),
        ("Review 🦀 café", Some("Review 🦀 café")),
        ("\u{1b}[31mred\u{1b}[0m text", Some("red text")),
        ("before\u{1b}]0;secret\u{7}after", Some("beforeafter")),
        ("left\u{1b}(Bright", Some("leftright")),
        ("a\u{1}b\u{85}c", Some("abc")),
        ("\u{2003}\u{202f}", None),
        ("\u{1b}[31;", None),
    ];

    for (input, expected) in cases {
        assert_eq!(normalize_work_summary(input).as_deref(), expected);
    }
}

#[test]
fn normalizer_caps_output_at_160_unicode_scalars() {
    let exactly_160 = "🦀".repeat(160);
    let over_160 = format!("{}z", exactly_160);

    assert_eq!(
        normalize_work_summary(&exactly_160),
        Some(exactly_160.clone())
    );
    assert_eq!(normalize_work_summary(&over_160), Some(exactly_160));
}

#[test]
fn explicit_summary_presence_distinguishes_preserve_clear_and_set() {
    assert_eq!(explicit_summary_update(None), WorkSummaryUpdate::Unchanged);
    assert_eq!(
        explicit_summary_update(Some(" \u{1b}[31m \t")),
        WorkSummaryUpdate::Clear
    );
    assert_eq!(
        explicit_summary_update(Some("  implement\nprojector  ")),
        WorkSummaryUpdate::Set("implement projector".to_string())
    );
}

#[test]
fn legacy_empty_summary_is_non_destructive() {
    assert_eq!(
        completion_summary_update(" \u{1b}[31m \t"),
        CompletionSummaryUpdate::Unchanged
    );
    assert_eq!(
        completion_summary_update("  review\nprojector  "),
        CompletionSummaryUpdate::Set("review projector".to_string())
    );
}

#[test]
fn summary_update_defaults_preserve_existing_values() {
    assert_eq!(WorkSummaryUpdate::default(), WorkSummaryUpdate::Unchanged);
    assert_eq!(
        CompletionSummaryUpdate::default(),
        CompletionSummaryUpdate::Unchanged
    );
}
