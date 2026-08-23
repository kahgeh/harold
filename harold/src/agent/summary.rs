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
    ControlString,
    ControlStringEscape,
}

pub(crate) fn normalize_work_summary(input: &str) -> Option<String> {
    let output = sanitize_terminal_text(
        input,
        WhitespaceMode::Collapse,
        Some(WORK_SUMMARY_MAX_SCALARS),
    );
    (!output.is_empty()).then_some(output)
}

pub(crate) fn normalize_visible_grid(input: &str) -> String {
    sanitize_terminal_text(input, WhitespaceMode::PreserveLines, None)
}

pub(crate) fn sanitize_bounded_metadata(input: &str, max_scalars: usize) -> String {
    sanitize_terminal_text(input, WhitespaceMode::Collapse, Some(max_scalars))
}

#[derive(Clone, Copy)]
enum WhitespaceMode {
    Collapse,
    PreserveLines,
}

fn sanitize_terminal_text(
    input: &str,
    whitespace_mode: WhitespaceMode,
    max_scalars: Option<usize>,
) -> String {
    let capacity = max_scalars.map_or(input.len(), |limit| input.len().min(limit));
    let mut output = String::with_capacity(capacity);
    let mut output_scalars = 0;
    let mut pending_space = false;
    let mut state = EscapeState::Text;
    let mut skip_line_feed = false;

    for character in input.chars() {
        if skip_line_feed {
            skip_line_feed = false;
            if character == '\n' {
                continue;
            }
        }
        match state {
            EscapeState::Escape => {
                state = match character {
                    '[' => EscapeState::Csi,
                    ']' => EscapeState::Osc,
                    'P' | 'X' | '^' | '_' => EscapeState::ControlString,
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
                if character == '\u{7}' || character == '\u{9c}' {
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
            EscapeState::ControlString => {
                if character == '\u{9c}' {
                    state = EscapeState::Text;
                } else if character == '\u{1b}' {
                    state = EscapeState::ControlStringEscape;
                }
                continue;
            }
            EscapeState::ControlStringEscape => {
                state = if character == '\\' {
                    EscapeState::Text
                } else {
                    EscapeState::ControlString
                };
                continue;
            }
            EscapeState::Text => {}
        }

        match character {
            '\u{1b}' => state = EscapeState::Escape,
            '\u{9b}' => state = EscapeState::Csi,
            '\u{9d}' => state = EscapeState::Osc,
            '\u{90}' | '\u{98}' | '\u{9e}' | '\u{9f}' => state = EscapeState::ControlString,
            '\r' => {
                skip_line_feed = true;
                handle_whitespace(
                    &mut output,
                    &mut pending_space,
                    &mut output_scalars,
                    whitespace_mode,
                    true,
                    max_scalars,
                );
            }
            '\n' => handle_whitespace(
                &mut output,
                &mut pending_space,
                &mut output_scalars,
                whitespace_mode,
                true,
                max_scalars,
            ),
            '\t' => handle_whitespace(
                &mut output,
                &mut pending_space,
                &mut output_scalars,
                whitespace_mode,
                false,
                max_scalars,
            ),
            '\u{80}'..='\u{9f}' | '\u{0}'..='\u{8}' | '\u{b}'..='\u{1f}' | '\u{7f}' => {}
            _ if character.is_whitespace() => handle_whitespace(
                &mut output,
                &mut pending_space,
                &mut output_scalars,
                whitespace_mode,
                false,
                max_scalars,
            ),
            _ => {
                if pending_space {
                    if limit_reached(output_scalars, max_scalars) {
                        break;
                    }
                    output.push(' ');
                    output_scalars += 1;
                    pending_space = false;
                }
                if limit_reached(output_scalars, max_scalars) {
                    break;
                }
                output.push(character);
                output_scalars += 1;
            }
        }
    }

    while output.ends_with([' ', '\n']) {
        output.pop();
    }
    output
}

fn handle_whitespace(
    output: &mut String,
    pending_space: &mut bool,
    output_scalars: &mut usize,
    mode: WhitespaceMode,
    line_break: bool,
    max_scalars: Option<usize>,
) {
    if line_break && matches!(mode, WhitespaceMode::PreserveLines) {
        *pending_space = false;
        while output.ends_with(' ') {
            output.pop();
            *output_scalars -= 1;
        }
        if !output.is_empty() && !limit_reached(*output_scalars, max_scalars) {
            output.push('\n');
            *output_scalars += 1;
        }
        return;
    }
    *pending_space = !output.is_empty() && !output.ends_with('\n');
}

fn limit_reached(output_scalars: usize, max_scalars: Option<usize>) -> bool {
    max_scalars.is_some_and(|limit| output_scalars == limit)
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
