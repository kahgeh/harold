use super::super::domain::ObservedAgentState;
use super::capture::StyledPaneCapture;
use super::{GenericAdapter, PromptScan, ProviderScreenAdapter};
use crate::settings::AgentProviderSettings;

const SPINNER_GLYPHS: [char; 7] = ['·', '✢', '✳', '✶', '✻', '✽', '*'];

pub(super) struct ClaudeAdapter<'a>(pub(super) &'a AgentProviderSettings);

impl ProviderScreenAdapter for ClaudeAdapter<'_> {
    fn classify_visible(&self, capture: &StyledPaneCapture) -> Option<ObservedAgentState> {
        if spinner_is_running(&capture.text) {
            return Some(ObservedAgentState::Busy);
        }
        GenericAdapter(self.0).classify_visible(capture)
    }

    fn scan_prompts(&self, capture: &StyledPaneCapture) -> PromptScan {
        GenericAdapter(self.0).scan_prompts(capture)
    }
}

/// Reads the newest top-level transcript row above the input box. Claude indents
/// the rows it nests under a status (tool results, todos, tips), so they are skipped.
fn spinner_is_running(text: &str) -> bool {
    let lines: Vec<&str> = text
        .lines()
        .map(|line| line.trim_end_matches('\r'))
        .collect();
    let Some(input_row) = (1..lines.len())
        .rev()
        .find(|&row| lines[row].starts_with('❯') && lines[row - 1].starts_with('─'))
    else {
        return false;
    };
    lines[..input_row - 1]
        .iter()
        .rev()
        .find(|line| !line.trim().is_empty() && !line.starts_with(char::is_whitespace))
        .is_some_and(|status| is_running_status(status))
}

/// A finished turn keeps its glyph but drops the ellipsis: "✻ Worked for 20s".
fn is_running_status(line: &str) -> bool {
    let mut chars = line.chars();
    let (Some(glyph), Some(' ')) = (chars.next(), chars.next()) else {
        return false;
    };
    let rest = chars.as_str();
    SPINNER_GLYPHS.contains(&glyph) && (rest.contains('…') || rest.contains("retrying"))
}
