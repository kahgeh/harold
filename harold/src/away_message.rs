//! Rules for the text an agent asks Harold to send to the user's away device, and the
//! refusal messages the daemon and the command share.

/// The longest message Harold sends, in Unicode characters of the one-line text.
pub(crate) const MAX_MESSAGE_CHARS: usize = 1_000;

// The status messages of the four refusals. The command tells refusals with the same gRPC
// code apart by these, so the daemon and the command both read them from here.
pub(crate) const INVALID_SOURCE_ID: &str = "invalid source id";
pub(crate) const INVALID_MESSAGE_LENGTH: &str = "invalid message length";
pub(crate) const MESSAGING_PAUSED: &str = "messaging paused";
pub(crate) const AGENT_NOT_FOUND: &str = "agent not found";

/// A character that ends a line or is a tab. The iMessage send deletes control characters
/// (`sanitise_for_applescript`), which would join the words on either side of one, and it
/// leaves U+2028 and U+2029 in place.
fn is_break(c: char) -> bool {
    matches!(
        c,
        '\n' | '\r' | '\t' | '\u{0B}' | '\u{0C}' | '\u{85}' | '\u{2028}' | '\u{2029}'
    )
}

/// `text` as one line: each run of whitespace that holds a line break or a tab becomes one
/// space, every other control character is removed, and both ends are trimmed. A run of
/// plain spaces is kept as written.
pub(crate) fn one_line(text: &str) -> String {
    let mut line = String::with_capacity(text.len());
    let mut run = String::new();
    let mut run_has_break = false;
    for c in text.chars().filter(|c| is_break(*c) || !c.is_control()) {
        if c.is_whitespace() {
            run_has_break |= is_break(c);
            run.push(c);
            continue;
        }
        line.push_str(if run_has_break { " " } else { &run });
        run.clear();
        run_has_break = false;
        line.push(c);
    }
    line.trim().to_string()
}

/// Whether a one-line text has 1 to 1,000 characters.
pub(crate) fn is_sendable_length(line: &str) -> bool {
    (1..=MAX_MESSAGE_CHARS).contains(&line.chars().count())
}

#[cfg(test)]
mod tests {
    use super::{MAX_MESSAGE_CHARS, is_sendable_length, one_line};

    #[test]
    fn one_line_joins_lines_with_one_space() {
        assert_eq!(
            one_line("approval:\nhttps://example.com"),
            "approval: https://example.com"
        );
        assert_eq!(one_line("a \n\t b"), "a b");
        assert_eq!(one_line("a\tb"), "a b");
        assert_eq!(one_line("a\r\n\r\nb"), "a b");
    }

    #[test]
    fn one_line_keeps_spaces_that_hold_no_line_break() {
        assert_eq!(one_line("a  b"), "a  b");
        assert_eq!(
            one_line("quotes \"x\" [tag] https://e.com/a?b=1"),
            "quotes \"x\" [tag] https://e.com/a?b=1"
        );
    }

    #[test]
    fn one_line_removes_other_control_characters() {
        assert_eq!(one_line("a\u{0}b\u{7}"), "ab");
        assert_eq!(one_line("a\u{1b}[31m\nb"), "a[31m b");
        assert_eq!(one_line("a\u{0}\nb"), "a b");
    }

    #[test]
    fn one_line_trims_both_ends() {
        assert_eq!(one_line("  hi  "), "hi");
        assert_eq!(one_line("\nhi\n"), "hi");
        assert_eq!(one_line("\n\t \r"), "");
    }

    #[test]
    fn one_line_replaces_every_kind_of_line_break() {
        assert_eq!(
            one_line("a\u{2028}b\u{2029}c\u{85}d\u{0B}e\u{0C}f\r\ng"),
            "a b c d e f g"
        );
    }

    #[test]
    fn length_is_counted_in_characters_after_the_text_is_one_line() {
        assert!(!is_sendable_length(""));
        assert!(is_sendable_length("x"));
        assert!(is_sendable_length(&"x".repeat(MAX_MESSAGE_CHARS)));
        assert!(!is_sendable_length(&"x".repeat(MAX_MESSAGE_CHARS + 1)));
        // 4,000 bytes, 1,000 characters.
        assert!(is_sendable_length(&"🤖".repeat(MAX_MESSAGE_CHARS)));
        // 1,001 raw characters that become 1,000.
        let padded = format!("{}\n", "x".repeat(MAX_MESSAGE_CHARS));
        assert!(is_sendable_length(&one_line(&padded)));
    }
}
