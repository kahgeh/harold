#[derive(Clone, Copy)]
enum ParserState {
    Ground,
    Escape,
    EscapeIntermediate,
    Csi,
    Osc,
    OscEscape,
    StringControl,
    StringEscape,
}

pub fn sanitize_display(input: &str, max_scalars: usize) -> String {
    let mut output = String::with_capacity(input.len().min(max_scalars));
    let mut output_scalars = 0;
    let mut state = ParserState::Ground;

    for character in input.chars() {
        state = match state {
            ParserState::Ground => match character {
                '\u{1b}' => ParserState::Escape,
                '\u{90}' | '\u{98}' | '\u{9e}' | '\u{9f}' => ParserState::StringControl,
                '\u{9b}' => ParserState::Csi,
                '\u{9d}' => ParserState::Osc,
                character if character.is_control() => ParserState::Ground,
                character => {
                    if output_scalars < max_scalars {
                        output.push(character);
                        output_scalars += 1;
                    }
                    ParserState::Ground
                }
            },
            ParserState::Escape => match character {
                '[' => ParserState::Csi,
                ']' => ParserState::Osc,
                'P' | 'X' | '^' | '_' => ParserState::StringControl,
                '\u{1b}' => ParserState::Escape,
                '\u{20}'..='\u{2f}' => ParserState::EscapeIntermediate,
                '\u{30}'..='\u{7e}' => ParserState::Ground,
                _ => ParserState::Ground,
            },
            ParserState::EscapeIntermediate => match character {
                '\u{1b}' => ParserState::Escape,
                '\u{30}'..='\u{7e}' => ParserState::Ground,
                _ => ParserState::EscapeIntermediate,
            },
            ParserState::Csi => match character {
                '\u{1b}' => ParserState::Escape,
                '\u{40}'..='\u{7e}' => ParserState::Ground,
                _ => ParserState::Csi,
            },
            ParserState::Osc => match character {
                '\u{07}' | '\u{9c}' => ParserState::Ground,
                '\u{1b}' => ParserState::OscEscape,
                _ => ParserState::Osc,
            },
            ParserState::OscEscape => match character {
                '\\' | '\u{07}' | '\u{9c}' => ParserState::Ground,
                '\u{1b}' => ParserState::OscEscape,
                _ => ParserState::Osc,
            },
            ParserState::StringControl => match character {
                '\u{9c}' => ParserState::Ground,
                '\u{1b}' => ParserState::StringEscape,
                _ => ParserState::StringControl,
            },
            ParserState::StringEscape => match character {
                '\\' | '\u{9c}' => ParserState::Ground,
                '\u{1b}' => ParserState::StringEscape,
                _ => ParserState::StringControl,
            },
        };
    }

    output
}

pub fn normalize_search(input: &str) -> String {
    input.to_lowercase()
}

pub fn display_work_summary(summary: Option<&str>) -> &str {
    match summary {
        Some(summary) if !summary.is_empty() => summary,
        _ => "No work summary reported",
    }
}

#[cfg(test)]
mod tests {
    use super::{display_work_summary, normalize_search, sanitize_display};

    #[test]
    fn strips_c0_del_and_c1_controls_without_losing_printable_unicode() {
        assert_eq!(sanitize_display("A\0\n\u{7f}\u{80}界", 32), "A界");
    }

    #[test]
    fn strips_seven_and_eight_bit_csi_sequences() {
        assert_eq!(sanitize_display("safe\x1b[31mred\x1b[0m!", 32), "safered!");
        assert_eq!(sanitize_display("safe\u{9b}31mred", 32), "safered");
    }

    #[test]
    fn strips_osc_and_string_control_sequences() {
        assert_eq!(
            sanitize_display("a\x1b]0;title\x07b\x1b]name\x1b\\c", 32),
            "abc"
        );
        assert_eq!(sanitize_display("a\x1bPprivate payload\x1b\\b", 32), "ab");
    }

    #[test]
    fn consumes_truncated_control_sequences_through_end_of_input() {
        assert_eq!(sanitize_display("safe\x1b[31", 32), "safe");
        assert_eq!(sanitize_display("safe\x1b]title", 32), "safe");
        assert_eq!(sanitize_display("safe\x1bPpayload", 32), "safe");
    }

    #[test]
    fn truncates_after_stripping_without_splitting_unicode_scalars() {
        assert_eq!(sanitize_display("a\x1b[31m界🙂z", 3), "a界🙂");
        assert_eq!(sanitize_display("visible", 0), "");

        let over_limit = format!("{}z", "x".repeat(160));
        let bounded = sanitize_display(&over_limit, 160);
        assert_eq!(bounded.chars().count(), 160);
        assert!(bounded.chars().all(|character| character == 'x'));
    }

    #[test]
    fn normalizes_search_case_without_discarding_unicode() {
        assert_eq!(normalize_search("Claude ÄBC 界"), "claude äbc 界");
    }

    #[test]
    fn missing_or_empty_summary_has_one_operator_copy() {
        assert_eq!(display_work_summary(None), "No work summary reported");
        assert_eq!(display_work_summary(Some("")), "No work summary reported");
        assert_eq!(
            display_work_summary(Some("Project agent snapshots")),
            "Project agent snapshots"
        );
    }
}
