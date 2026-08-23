#![allow(
    dead_code,
    reason = "normalization is consumed by later monitor ingress slices"
)]

use super::domain::{CompletionSummaryUpdate, WorkSummaryUpdate};

pub(crate) const WORK_SUMMARY_MAX_SCALARS: usize = 160;

#[derive(Clone, Copy)]
enum EscapeState {
    Text,
    Escape,
    EscapeIntermediate,
    Csi,
    Osc,
    OscEscape,
}

pub(crate) fn normalize_work_summary(input: &str) -> Option<String> {
    let mut output = String::with_capacity(input.len().min(WORK_SUMMARY_MAX_SCALARS));
    let mut output_scalars = 0;
    let mut pending_space = false;
    let mut state = EscapeState::Text;

    for character in input.chars() {
        match state {
            EscapeState::Escape => {
                state = match character {
                    '[' => EscapeState::Csi,
                    ']' => EscapeState::Osc,
                    '\u{20}'..='\u{2f}' => EscapeState::EscapeIntermediate,
                    _ => EscapeState::Text,
                };
                continue;
            }
            EscapeState::EscapeIntermediate => {
                if ('\u{30}'..='\u{7e}').contains(&character) {
                    state = EscapeState::Text;
                }
                continue;
            }
            EscapeState::Csi => {
                if ('\u{40}'..='\u{7e}').contains(&character) {
                    state = EscapeState::Text;
                }
                continue;
            }
            EscapeState::Osc => {
                if character == '\u{7}' {
                    state = EscapeState::Text;
                } else if character == '\u{1b}' {
                    state = EscapeState::OscEscape;
                }
                continue;
            }
            EscapeState::OscEscape => {
                state = if character == '\\' {
                    EscapeState::Text
                } else {
                    EscapeState::Osc
                };
                continue;
            }
            EscapeState::Text => {}
        }

        match character {
            '\u{1b}' => state = EscapeState::Escape,
            '\u{9b}' => state = EscapeState::Csi,
            '\u{9d}' => state = EscapeState::Osc,
            '\u{80}'..='\u{9f}' | '\u{0}'..='\u{8}' | '\u{b}'..='\u{1f}' | '\u{7f}' => {}
            _ if character.is_whitespace() => pending_space = !output.is_empty(),
            _ => {
                if pending_space {
                    if output_scalars == WORK_SUMMARY_MAX_SCALARS {
                        break;
                    }
                    output.push(' ');
                    output_scalars += 1;
                    pending_space = false;
                }
                if output_scalars == WORK_SUMMARY_MAX_SCALARS {
                    break;
                }
                output.push(character);
                output_scalars += 1;
            }
        }
    }

    while output.ends_with(' ') {
        output.pop();
    }

    (!output.is_empty()).then_some(output)
}

pub(crate) fn explicit_summary_update(input: Option<&str>) -> WorkSummaryUpdate {
    let Some(input) = input else {
        return WorkSummaryUpdate::Unchanged;
    };
    normalize_work_summary(input).map_or(WorkSummaryUpdate::Clear, WorkSummaryUpdate::Set)
}

pub(crate) fn completion_summary_update(input: &str) -> CompletionSummaryUpdate {
    normalize_work_summary(input).map_or(
        CompletionSummaryUpdate::Unchanged,
        CompletionSummaryUpdate::Set,
    )
}
