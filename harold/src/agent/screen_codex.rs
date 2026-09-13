use super::super::domain::ObservedAgentState;
use super::capture::StyledPaneCapture;
use super::{PromptScan, ProviderScreenAdapter, classify_state, prompt_block};
use crate::settings::AgentProviderSettings;

pub(super) struct CodexAdapter<'a>(pub(super) &'a AgentProviderSettings);

impl ProviderScreenAdapter for CodexAdapter<'_> {
    fn classify_visible(&self, capture: &StyledPaneCapture) -> Option<ObservedAgentState> {
        classify_state(&capture.text, self.0)
    }

    fn scan_prompts(&self, capture: &StyledPaneCapture) -> PromptScan {
        if !capture.request.preserve_styles {
            return PromptScan::default();
        }
        let mut parser = StyledRows::default();
        let mut blocks = Vec::new();
        let mut pending: Option<String> = None;
        for text in capture.text.split('\n') {
            let row = parser.parse(text);
            if !row.valid {
                pending = None;
                continue;
            }
            if let Some(start) = submitted_text(&row) {
                if let Some(previous) = pending.take() {
                    blocks.push(prompt_block(&previous, &self.0.idle_all));
                }
                pending = Some(start);
                continue;
            }
            if let Some(continuation) = continuation_text(&row) {
                if let Some(prompt) = &mut pending {
                    prompt.push(' ');
                    prompt.push_str(&continuation);
                }
                continue;
            }
            if let Some(previous) = pending.take() {
                blocks.push(prompt_block(&previous, &self.0.idle_all));
            }
        }
        if let Some(previous) = pending {
            blocks.push(prompt_block(&previous, &self.0.idle_all));
        }
        PromptScan { blocks }
    }
}

#[derive(Clone, Copy, Default)]
struct Style {
    bold: bool,
    dim: bool,
}
impl Style {
    fn plain(self) -> bool {
        !self.bold && !self.dim
    }
}
struct Cell {
    character: char,
    style: Style,
}
struct Row {
    cells: Vec<Cell>,
    valid: bool,
    end_style: Style,
}

fn submitted_text(row: &Row) -> Option<String> {
    if !row.valid {
        return None;
    }
    let [marker, separator, rest @ ..] = row.cells.as_slice() else {
        return None;
    };
    // Live 0.153.4 submissions style both marker AND separator, then reset.
    // The nonempty composer has normal text too, but resets BEFORE its separator.
    if !matches!(marker.character, '›' | '>')
        || !marker.style.bold
        || separator.character != ' '
        || !separator.style.bold
        || !row.end_style.plain()
        || rest
            .iter()
            .any(|cell| !cell.character.is_whitespace() && !cell.style.plain())
    {
        return None;
    }
    Some(rest.iter().map(|cell| cell.character).collect())
}

fn continuation_text(row: &Row) -> Option<String> {
    if !row.valid {
        return None;
    }
    let [first, second, rest @ ..] = row.cells.as_slice() else {
        return None;
    };
    if first.character != ' '
        || second.character != ' '
        || rest.is_empty()
        || row.cells.iter().any(|cell| !cell.style.plain())
    {
        return None;
    }
    let text: String = rest.iter().map(|cell| cell.character).collect();
    let text = text.trim();
    if text.is_empty() || text.starts_with(['•', '›', '>', '└', '─', '╭', '╰']) {
        return None;
    }
    Some(text.to_string())
}

#[derive(Default)]
enum Control {
    #[default]
    Text,
    Escape,
    Csi(String),
    String {
        osc: bool,
        escaped: bool,
    },
}

#[derive(Default)]
struct StyledRows {
    style: Style,
    control: Control,
    distrust_style: bool,
}

impl StyledRows {
    fn parse(&mut self, text: &str) -> Row {
        let mut malformed = false;
        let mut row = Row {
            cells: Vec::new(),
            valid: matches!(self.control, Control::Text) && !self.distrust_style,
            end_style: self.style,
        };
        for character in text.chars() {
            match &mut self.control {
                Control::Escape => {
                    self.control = match character {
                        '[' => Control::Csi(String::new()),
                        ']' => Control::String {
                            osc: true,
                            escaped: false,
                        },
                        'P' | 'X' | '^' | '_' => Control::String {
                            osc: false,
                            escaped: false,
                        },
                        _ => {
                            row.valid = false;
                            malformed = true;
                            Control::Text
                        }
                    };
                }
                Control::Csi(parameters) => {
                    if ('\u{40}'..='\u{7e}').contains(&character) {
                        let mut next_style = self.style;
                        if character != 'm' || !apply_sgr(&mut next_style, parameters) {
                            row.valid = false;
                            malformed = true;
                        } else {
                            self.style = next_style;
                            if parameters == "0" || parameters.is_empty() {
                                self.distrust_style = false;
                            }
                        }
                        self.control = Control::Text;
                    } else if parameters.len() < 128
                        && (character.is_ascii_digit() || character == ';')
                    {
                        parameters.push(character);
                    } else {
                        row.valid = false;
                        malformed = true;
                    }
                }
                Control::String { osc, escaped } => {
                    if character == '\u{9c}'
                        || (*osc && character == '\u{7}')
                        || (*escaped && character == '\\')
                    {
                        self.control = Control::Text;
                    } else {
                        *escaped = character == '\u{1b}';
                    }
                }
                Control::Text => match character {
                    '\u{1b}' => self.control = Control::Escape,
                    '\u{9b}' => self.control = Control::Csi(String::new()),
                    '\u{9d}' => {
                        self.control = Control::String {
                            osc: true,
                            escaped: false,
                        }
                    }
                    '\u{90}' | '\u{98}' | '\u{9e}' | '\u{9f}' => {
                        self.control = Control::String {
                            osc: false,
                            escaped: false,
                        }
                    }
                    '\r' => {}
                    character if character.is_control() => {
                        row.valid = false;
                        malformed = true;
                    }
                    _ => row.cells.push(Cell {
                        character,
                        style: self.style,
                    }),
                },
            }
        }
        malformed |= !matches!(self.control, Control::Text);
        if malformed {
            row.valid = false;
            self.style = Style::default();
            self.distrust_style = true;
        }
        row.end_style = self.style;
        row
    }
}

fn apply_sgr(style: &mut Style, parameters: &str) -> bool {
    let Ok(parameters) = parameters
        .split(';')
        .map(|value| {
            if value.is_empty() {
                Ok(0_u16)
            } else {
                value.parse::<u16>()
            }
        })
        .collect::<Result<Vec<_>, _>>()
    else {
        return false;
    };
    let mut parameters = parameters.into_iter();
    while let Some(code) = parameters.next() {
        match code {
            0 => *style = Style::default(),
            1 => style.bold = true,
            2 => style.dim = true,
            22 => *style = Style::default(),
            // Extended color components can contain 0, 1, or 2. They are not attributes.
            38 | 48 | 58 => {
                let count = match parameters.next() {
                    Some(5) => 1,
                    Some(2) => 3,
                    _ => return false,
                };
                for _ in 0..count {
                    if !parameters.next().is_some_and(|value| value <= 255) {
                        return false;
                    }
                }
            }
            3..=9
            | 21
            | 23..=29
            | 30..=37
            | 39
            | 40..=47
            | 49
            | 53..=55
            | 59
            | 90..=97
            | 100..=107 => {}
            _ => return false,
        }
    }
    true
}
