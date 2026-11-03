//! The remote control. Tasks 6 to 8 fill this module.
//!
//! This part is the model (the list of entries and the keys) and the drawing.
//! The model has no I/O, so tests can check every rule.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::api::AgentStateSource;
use crate::app::AgentRow;
use crate::cli::Options;
use crate::navigation::TmuxNavigator;
use crate::runtime::AppError;
use crate::ui::{AMBER, BOARD, CORAL, INK, MUTED};

/// One line of the remote control.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The pane id of the agent, for example `block:<id>` for a Rex pane.
    pub pane_id: String,
    /// The session of the agent. The jump needs it.
    pub session: String,
    /// The block id of a Rex pane, when `pane_id` is `block:<id>`.
    pub block_id: Option<String>,
    /// The product of the pod, empty for the chief.
    pub product: String,
    /// `Chief`, or `PnEM · <product>`.
    pub label: String,
    /// True when the agent is a waiting agent (marked `◆` in the control-tower).
    pub waiting: bool,
    pub is_chief: bool,
}

/// What the caller must do after a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Jump to the entry with this index, then close.
    Jump(usize),
    /// Save the new order of the PnEM products.
    Move {
        order: Vec<String>,
    },
    /// Close with no jump.
    Close,
    None,
}

/// The state of the remote control.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Model {
    pub entries: Vec<Entry>,
    pub selected: usize,
    /// A message for the last line, for example a failed jump.
    pub notice: Option<String>,
}

/// Build the entries: the chief first, then the PnEMs in the given order.
///
/// Only rows of this Mac (empty `host`) are used. A PnEM whose product is not
/// in `order` comes after the others, in row order. `is_waiting` tells if a row
/// is a waiting agent.
pub fn entries(
    rows: &[AgentRow],
    order: &[String],
    is_waiting: impl Fn(&AgentRow) -> bool,
) -> Vec<Entry> {
    let local = || rows.iter().filter(|row| row.host.is_empty());
    let make = |row: &AgentRow, is_chief: bool| Entry {
        pane_id: row.incarnation.pane_id.clone(),
        session: row.session_name.clone(),
        block_id: row
            .incarnation
            .pane_id
            .strip_prefix("block:")
            .map(str::to_string),
        product: row.pod_product.clone(),
        label: if is_chief {
            "Chief".to_string()
        } else {
            format!("PnEM \u{b7} {}", row.pod_product)
        },
        waiting: is_waiting(row),
        is_chief,
    };

    let mut list: Vec<Entry> = local()
        .find(|row| row.role == "chief")
        .map(|row| make(row, true))
        .into_iter()
        .collect();

    let mut pnems: Vec<&AgentRow> = local().filter(|row| row.role == "pnem").collect();
    // The sort is stable, so equal keys keep the row order.
    pnems.sort_by_key(|row| {
        order
            .iter()
            .position(|name| *name == row.pod_product)
            .unwrap_or(usize::MAX)
    });
    list.extend(pnems.into_iter().map(|row| make(row, false)));
    list
}

impl Model {
    pub fn new(entries: Vec<Entry>) -> Model {
        Model {
            entries,
            selected: 0,
            notice: None,
        }
    }

    /// Use new entries and keep the selection on the same pane when it exists.
    pub fn replace_entries(&mut self, entries: Vec<Entry>) {
        let current = self.entries.get(self.selected).map(|e| e.pane_id.clone());
        self.entries = entries;
        let same = current.and_then(|id| self.entries.iter().position(|e| e.pane_id == id));
        self.selected = same.unwrap_or(self.selected);
        self.selected = self.selected.min(self.entries.len().saturating_sub(1));
    }

    fn order(&self) -> Vec<String> {
        self.entries
            .iter()
            .filter(|e| !e.is_chief)
            .map(|e| e.product.clone())
            .collect()
    }

    /// Swap the selected PnEM with its neighbour. `up` moves it towards the chief.
    fn move_selected(&mut self, up: bool) -> Action {
        let from = self.selected;
        match self.entries.get(from) {
            Some(entry) if !entry.is_chief => {}
            _ => return Action::None,
        }
        let to = if up {
            match from.checked_sub(1) {
                Some(to) => to,
                None => return Action::None,
            }
        } else {
            from + 1
        };
        match self.entries.get(to) {
            Some(neighbour) if !neighbour.is_chief => {}
            _ => return Action::None,
        }
        self.entries.swap(from, to);
        self.selected = to;
        Action::Move {
            order: self.order(),
        }
    }

    fn next_waiting(&self) -> Action {
        let len = self.entries.len();
        (1..=len)
            .map(|step| (self.selected + step) % len)
            .find(|&index| self.entries[index].waiting)
            .map_or(Action::None, Action::Jump)
    }

    pub fn key(&mut self, key: KeyEvent) -> Action {
        if key.kind == KeyEventKind::Release
            || key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return Action::None;
        }
        self.notice = None;
        let last = self.entries.len().saturating_sub(1);
        match key.code {
            KeyCode::Esc => Action::Close,
            KeyCode::Enter if self.entries.is_empty() => Action::None,
            KeyCode::Enter => Action::Jump(self.selected),
            KeyCode::Char(digit @ '0'..='9') => {
                let index = digit as usize - '0' as usize;
                if index < self.entries.len() {
                    Action::Jump(index)
                } else {
                    Action::None
                }
            }
            KeyCode::Char('j') => {
                self.selected = (self.selected + 1).min(last);
                Action::None
            }
            KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                Action::None
            }
            KeyCode::Char('J') => self.move_selected(false),
            KeyCode::Char('K') => self.move_selected(true),
            KeyCode::Char('n') if self.entries.is_empty() => Action::None,
            KeyCode::Char('n') => self.next_waiting(),
            _ => Action::None,
        }
    }
}

/// Draw the remote control in the whole area of the frame.
///
/// The Rex layer is already the small window, so the box fills the area. Text
/// is cut at the width and never wraps.
pub fn draw(frame: &mut Frame, model: &Model) {
    let area = frame.area();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(MUTED))
        .title(Line::styled(
            "\u{2500} Harold: remote control ",
            Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(BOARD));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let ink = Style::default().fg(INK);
    let marked = Style::default().fg(AMBER).add_modifier(Modifier::BOLD);
    let muted = Style::default().fg(MUTED);

    let mut lines: Vec<Line> = vec![Line::raw("")];
    for (index, entry) in model.entries.iter().enumerate() {
        let selected = index == model.selected;
        let mark = if selected { '\u{25b8}' } else { ' ' };
        let style = if selected { marked } else { ink };
        lines.push(Line::styled(
            format!("{mark} {index}   {}", entry.label),
            style,
        ));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "  0-9 jump   j/k select   J/K move row",
        muted,
    ));
    lines.push(match &model.notice {
        Some(notice) => Line::styled(
            format!("  {notice}"),
            Style::default().fg(CORAL).add_modifier(Modifier::BOLD),
        ),
        None => Line::styled("  Enter jump   n next waiting   Esc close", muted),
    });

    // Draw the hint lines at the bottom of the box; the rows and the blank
    // line above them are cut first when the area is short.
    let hint_start = lines.len().saturating_sub(2);
    let height = usize::from(inner.height);
    let (body, hints) = lines.split_at(hint_start);
    let hint_rows = hints.len().min(height);
    let body_rows = height - hint_rows;
    let body_area = Rect {
        height: body_rows as u16,
        ..inner
    };
    let hint_area = Rect {
        y: inner.y + body_rows as u16,
        height: hint_rows as u16,
        ..inner
    };
    frame.render_widget(Paragraph::new(body.to_vec()), body_area);
    frame.render_widget(Paragraph::new(hints.to_vec()), hint_area);
}

/// Run the remote control. It is a stub until Task 8.
pub fn run(
    _options: Options,
    _source: AgentStateSource,
    _navigator: TmuxNavigator,
) -> Result<(), AppError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{AgentIncarnation, AgentState};

    fn row(pane_id: &str, session: &str, role: &str, product: &str, host: &str) -> AgentRow {
        AgentRow {
            incarnation: AgentIncarnation {
                pane_id: pane_id.into(),
                pane_pid: 1,
                agent_pid: 2,
                agent_started_at_ms: 3,
                provider_id: "agent".into(),
            },
            provider_display_name: "Agent".into(),
            tmux_target: String::new(),
            session_name: session.into(),
            window_index: 0,
            pane_index: 0,
            working_directory: "/w".into(),
            work_summary: None,
            current_step: None,
            recap: None,
            recap_at_ms: 0,
            state: AgentState::Idle,
            last_transition_at_ms: 0,
            host: host.into(),
            host_icon: String::new(),
            window_name: String::new(),
            role: role.into(),
            pod_product: product.into(),
        }
    }

    fn chief() -> AgentRow {
        row("block:chief", "chief", "chief", "", "")
    }

    fn pnem(product: &str) -> AgentRow {
        row(&format!("block:{product}"), product, "pnem", product, "")
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn labels(list: &[Entry]) -> Vec<&str> {
        list.iter().map(|e| e.label.as_str()).collect()
    }

    fn none(_: &AgentRow) -> bool {
        false
    }

    fn model(products: &[&str]) -> Model {
        let rows: Vec<AgentRow> = std::iter::once(chief())
            .chain(products.iter().map(|p| pnem(p)))
            .collect();
        Model::new(entries(&rows, &names(products), none))
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ch(c: char) -> KeyEvent {
        let mods = if c.is_uppercase() {
            KeyModifiers::SHIFT
        } else {
            KeyModifiers::NONE
        };
        KeyEvent::new(KeyCode::Char(c), mods)
    }

    #[test]
    fn the_chief_is_first_then_pnems_in_the_given_order() {
        let rows = [pnem("a"), pnem("b"), chief(), pnem("c")];
        let list = entries(&rows, &names(&["c", "a", "b"]), none);
        assert_eq!(labels(&list), ["Chief", "PnEM · c", "PnEM · a", "PnEM · b"]);
        assert!(list[0].is_chief);
        assert!(!list[1].is_chief);
        assert_eq!(list[1].session, "c");
        assert_eq!(list[1].block_id.as_deref(), Some("c"));
        assert_eq!(list[1].product, "c");
    }

    #[test]
    fn a_pnem_not_in_the_order_comes_last() {
        let rows = [chief(), pnem("x"), pnem("a"), pnem("y")];
        let list = entries(&rows, &names(&["a"]), none);
        assert_eq!(labels(&list), ["Chief", "PnEM · a", "PnEM · x", "PnEM · y"]);
    }

    #[test]
    fn host_rows_and_other_roles_are_not_listed() {
        let rows = [
            row("block:h", "s", "chief", "", "omarchy"),
            chief(),
            row("block:p", "s", "pnem", "far", "omarchy"),
            row("block:k", "s", "coordinator", "harold", ""),
            row("block:o", "s", "", "", ""),
            pnem("a"),
        ];
        let list = entries(&rows, &names(&["a"]), none);
        assert_eq!(labels(&list), ["Chief", "PnEM · a"]);
    }

    #[test]
    fn only_one_chief_is_listed() {
        let rows = [chief(), row("block:c2", "s", "chief", "", "")];
        assert_eq!(entries(&rows, &[], none).len(), 1);
    }

    #[test]
    fn waiting_comes_from_the_given_function() {
        let rows = [chief(), pnem("a"), pnem("b")];
        let list = entries(&rows, &names(&["a", "b"]), |r| r.pod_product == "b");
        let waiting: Vec<bool> = list.iter().map(|e| e.waiting).collect();
        assert_eq!(waiting, [false, false, true]);
    }

    #[test]
    fn no_pnem_running_lists_only_the_chief() {
        let list = entries(&[chief()], &names(&["a"]), none);
        assert_eq!(labels(&list), ["Chief"]);
    }

    #[test]
    fn no_chief_found_lists_the_pnems_numbered_from_0() {
        let mut m = Model::new(entries(&[pnem("a"), pnem("b")], &names(&["a", "b"]), none));
        assert_eq!(labels(&m.entries), ["PnEM · a", "PnEM · b"]);
        assert_eq!(m.key(ch('0')), Action::Jump(0));
        assert_eq!(m.key(ch('1')), Action::Jump(1));
    }

    #[test]
    fn two_pnems_with_the_same_product_keep_row_order() {
        let rows = [chief(), pnem("a"), pnem("a")];
        let mut second = rows.to_vec();
        second[2].incarnation.pane_id = "block:second".into();
        let list = entries(&second, &names(&["a"]), none);
        assert_eq!(list.len(), 3);
        assert_eq!(list[1].pane_id, "block:a");
        assert_eq!(list[2].pane_id, "block:second");
    }

    #[test]
    fn digit_jumps_to_that_number() {
        let mut m = model(&["a", "b"]);
        assert_eq!(m.key(ch('0')), Action::Jump(0));
        assert_eq!(m.key(ch('2')), Action::Jump(2));
    }

    #[test]
    fn a_digit_with_no_entry_does_nothing() {
        let mut m = model(&["a"]);
        assert_eq!(m.key(ch('5')), Action::None);
    }

    #[test]
    fn j_and_k_clamp_at_the_ends() {
        let mut m = model(&["a", "b"]);
        assert_eq!(m.key(ch('k')), Action::None);
        assert_eq!(m.selected, 0);
        m.key(ch('j'));
        m.key(ch('j'));
        m.key(ch('j'));
        assert_eq!(m.selected, 2);
        m.key(ch('k'));
        assert_eq!(m.selected, 1);
    }

    #[test]
    fn shift_j_moves_the_pnem_down_and_returns_the_new_order() {
        let mut m = model(&["a", "b", "c"]);
        m.key(ch('j'));
        assert_eq!(
            m.key(ch('J')),
            Action::Move {
                order: names(&["b", "a", "c"])
            }
        );
        assert_eq!(
            labels(&m.entries),
            ["Chief", "PnEM · b", "PnEM · a", "PnEM · c"]
        );
        assert_eq!(m.selected, 2);
    }

    #[test]
    fn shift_k_moves_the_pnem_up() {
        let mut m = model(&["a", "b", "c"]);
        m.selected = 3;
        assert_eq!(
            m.key(ch('K')),
            Action::Move {
                order: names(&["a", "c", "b"])
            }
        );
        assert_eq!(m.selected, 2);
    }

    #[test]
    fn shift_k_on_the_first_pnem_does_nothing() {
        let mut m = model(&["a", "b"]);
        m.selected = 1;
        assert_eq!(m.key(ch('K')), Action::None);
        assert_eq!(m.selected, 1);
        assert_eq!(labels(&m.entries), ["Chief", "PnEM · a", "PnEM · b"]);
    }

    #[test]
    fn shift_j_on_the_last_pnem_does_nothing() {
        let mut m = model(&["a", "b"]);
        m.selected = 2;
        assert_eq!(m.key(ch('J')), Action::None);
        assert_eq!(m.selected, 2);
    }

    #[test]
    fn the_chief_row_never_moves() {
        let mut m = model(&["a", "b"]);
        assert_eq!(m.key(ch('J')), Action::None);
        assert_eq!(m.key(ch('K')), Action::None);
        assert_eq!(m.selected, 0);
        assert!(m.entries[0].is_chief);
    }

    #[test]
    fn shift_k_on_the_only_chief_does_nothing() {
        let mut m = model(&[]);
        assert_eq!(m.key(ch('K')), Action::None);
        assert_eq!(m.key(ch('J')), Action::None);
    }

    #[test]
    fn enter_jumps_to_the_selected_entry() {
        let mut m = model(&["a", "b"]);
        m.key(ch('j'));
        assert_eq!(m.key(press(KeyCode::Enter)), Action::Jump(1));
    }

    #[test]
    fn enter_on_an_empty_list_does_nothing() {
        let mut m = Model::new(Vec::new());
        assert_eq!(m.key(press(KeyCode::Enter)), Action::None);
        assert_eq!(m.key(ch('0')), Action::None);
        assert_eq!(m.key(ch('j')), Action::None);
        assert_eq!(m.key(ch('n')), Action::None);
        assert_eq!(m.selected, 0);
    }

    fn model_waiting(products: &[&str], waiting: &[&str]) -> Model {
        let rows: Vec<AgentRow> = std::iter::once(chief())
            .chain(products.iter().map(|p| pnem(p)))
            .collect();
        Model::new(entries(&rows, &names(products), |r| {
            waiting.contains(&r.pod_product.as_str())
        }))
    }

    #[test]
    fn n_jumps_to_the_next_waiting_entry_and_wraps() {
        let mut m = model_waiting(&["a", "b", "c"], &["a", "c"]);
        assert_eq!(m.key(ch('n')), Action::Jump(1));
        m.selected = 1;
        assert_eq!(m.key(ch('n')), Action::Jump(3));
        m.selected = 3;
        assert_eq!(m.key(ch('n')), Action::Jump(1));
    }

    #[test]
    fn n_with_no_waiting_entry_does_nothing() {
        let mut m = model_waiting(&["a"], &[]);
        assert_eq!(m.key(ch('n')), Action::None);
    }

    #[test]
    fn esc_closes() {
        let mut m = model(&["a"]);
        assert_eq!(m.key(press(KeyCode::Esc)), Action::Close);
    }

    #[test]
    fn with_more_than_ten_entries_only_digits_0_to_9_jump() {
        let products: Vec<String> = (0..11).map(|i| format!("p{i}")).collect();
        let refs: Vec<&str> = products.iter().map(String::as_str).collect();
        let mut m = model(&refs);
        assert_eq!(m.entries.len(), 12);
        assert_eq!(m.key(ch('9')), Action::Jump(9));
        for _ in 0..11 {
            m.key(ch('j'));
        }
        assert_eq!(m.selected, 11);
        assert_eq!(m.key(press(KeyCode::Enter)), Action::Jump(11));
    }

    #[test]
    fn replace_entries_keeps_the_selection_on_the_same_pane() {
        let mut m = model(&["a", "b", "c"]);
        m.selected = 2;
        let rows = [chief(), pnem("c"), pnem("a"), pnem("b")];
        m.replace_entries(entries(&rows, &names(&["c", "a", "b"]), none));
        assert_eq!(m.entries[m.selected].pane_id, "block:b");
    }

    #[test]
    fn replace_entries_clamps_when_the_pane_is_gone() {
        let mut m = model(&["a", "b", "c"]);
        m.selected = 3;
        m.replace_entries(entries(&[chief(), pnem("a")], &names(&["a"]), none));
        assert_eq!(m.selected, 1);
        m.replace_entries(Vec::new());
        assert_eq!(m.selected, 0);
    }

    #[test]
    fn key_releases_do_nothing() {
        let mut m = model(&["a"]);
        let mut release = ch('0');
        release.kind = KeyEventKind::Release;
        assert_eq!(m.key(release), Action::None);
    }

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn render(model: &Model, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, model)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    fn sample() -> Model {
        let mut m = model(&["harold", "keymap", "buckwal"]);
        m.selected = 1;
        m
    }

    #[test]
    fn draw_matches_the_mock_at_60_columns() {
        let rows = render(&sample(), 60, 10);
        let inner = |text: &str| format!("\u{2502}{text:<58}\u{2502}");
        assert_eq!(rows[0].chars().count(), 60);
        assert!(rows[0].starts_with("\u{250c}\u{2500} Harold: remote control \u{2500}"));
        assert!(rows[0].ends_with('\u{2510}'));
        assert_eq!(rows[1], inner(""));
        assert_eq!(rows[2], inner("  0   Chief"));
        assert_eq!(rows[3], inner("\u{25b8} 1   PnEM \u{b7} harold"));
        assert_eq!(rows[4], inner("  2   PnEM \u{b7} keymap"));
        assert_eq!(rows[5], inner("  3   PnEM \u{b7} buckwal"));
        assert_eq!(rows[6], inner(""));
        assert_eq!(rows[7], inner("  0-9 jump   j/k select   J/K move row"));
        assert_eq!(rows[8], inner("  Enter jump   n next waiting   Esc close"));
    }

    #[test]
    fn the_box_fills_a_taller_area() {
        let rows = render(&sample(), 60, 12);
        assert!(rows[11].starts_with('\u{2514}'));
        assert!(rows[10].contains("Enter jump"));
    }

    #[test]
    fn draw_works_at_40_and_80_columns() {
        for width in [40u16, 80] {
            let rows = render(&sample(), width, 12);
            assert!(rows[0].contains("Harold: remote control"));
            assert!(rows[3].contains("\u{25b8} 1   PnEM \u{b7} harold"));
            assert!(rows.iter().all(|r| r.chars().count() == width as usize));
        }
    }

    #[test]
    fn only_the_selected_row_has_the_marker() {
        let text = render(&sample(), 60, 12).join("\n");
        assert_eq!(text.matches('\u{25b8}').count(), 1);
    }

    #[test]
    fn a_row_shows_no_state_word() {
        let text = render(&sample(), 60, 12).join("\n");
        for word in ["idle", "working", "blocked"] {
            assert!(!text.contains(word), "found {word}");
        }
    }

    #[test]
    fn the_notice_replaces_the_last_line() {
        let mut m = sample();
        m.notice = Some("jump failed: no pane".into());
        let text = render(&m, 60, 12).join("\n");
        assert!(text.contains("jump failed: no pane"));
        assert!(!text.contains("Enter jump"));
        assert!(text.contains("0-9 jump"));
    }

    #[test]
    fn long_labels_are_cut_at_40_columns() {
        let mut m = sample();
        m.entries[1].label = "PnEM \u{b7} a-product-with-a-very-long-name-indeed".into();
        let rows = render(&m, 40, 12);
        assert!(rows.iter().all(|r| r.chars().count() == 40));
        assert!(rows[3].starts_with("\u{2502}\u{25b8} 1   PnEM \u{b7} a-product"));
        assert!(rows[3].ends_with('\u{2502}'));
        assert!(!rows.join("\n").contains("indeed"));
        assert_eq!(rows[8].chars().count(), 40);
    }

    #[test]
    fn an_empty_list_draws_no_rows_and_the_hint() {
        let m = Model::new(Vec::new());
        let rows = render(&m, 60, 9);
        assert_eq!(rows[2], format!("\u{2502}{:<58}\u{2502}", ""));
        let text = rows.join("\n");
        assert!(text.contains("0-9 jump"));
        assert!(text.contains("Esc close"));
    }

    #[test]
    fn a_very_small_area_does_not_panic() {
        for (w, h) in [(20, 5), (1, 1), (3, 2), (40, 3), (0, 0)] {
            let _ = render(&sample(), w, h);
        }
    }
}
