use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap};

use crate::app::{AgentState, App};
use crate::text::display_work_summary;

const BOARD: Color = Color::Rgb(20, 20, 16);
const PANEL: Color = Color::Rgb(25, 25, 21);
const INK: Color = Color::Rgb(237, 231, 216);
const MUTED: Color = Color::Rgb(143, 138, 127);
const AMBER: Color = Color::Rgb(224, 174, 85);
const GREEN: Color = Color::Rgb(125, 200, 139);
const CORAL: Color = Color::Rgb(224, 114, 98);
const MIN_WIDTH: u16 = 60;
const MIN_HEIGHT: u16 = 18;
const WIDE_WIDTH: u16 = 120;
const COMPACT_WIDTH: u16 = 84;

pub fn render(frame: &mut Frame<'_>, app: &App, now_ms: i64) {
    let area = frame.area();
    frame.render_widget(Block::default().style(Style::default().bg(BOARD)), area);

    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        render_resize_instruction(frame, area);
        return;
    }

    let compact = area.width < COMPACT_WIDTH || area.height < 24;
    let chrome_height = if compact { 2 } else { 3 };
    let [masthead, monitor, summary, search, workspace, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(chrome_height),
        Constraint::Length(chrome_height),
        Constraint::Length(chrome_height),
        Constraint::Min(6),
        Constraint::Length(3),
    ])
    .areas(area);

    render_masthead(frame, masthead, app);
    render_monitor(frame, monitor, app, now_ms);
    render_summary(frame, summary, app);
    render_search(frame, search, app);
    render_workspace(frame, workspace, app, now_ms);
    render_footer(frame, footer, compact);
}

fn render_resize_instruction(frame: &mut Frame<'_>, area: Rect) {
    let message = Paragraph::new(vec![
        Line::styled(
            "Terminal too small",
            Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
        ),
        Line::styled(
            format!("Resize to at least {MIN_WIDTH}x{MIN_HEIGHT}"),
            Style::default().fg(INK),
        ),
    ])
    .alignment(Alignment::Center)
    .block(board_block().borders(Borders::ALL));
    frame.render_widget(message, area);
}

fn render_masthead(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let status = Span::styled(
        format!(
            "TRANSPORT {}  ·  REV #{:05}",
            app.connection.label(),
            app.snapshot.through_event_version
        ),
        transport_style(app.connection).add_modifier(Modifier::BOLD),
    );
    let title = if area.width < COMPACT_WIDTH {
        Line::from(vec![
            Span::styled(" TMX DASH ", Style::default().fg(AMBER)),
            Span::raw("· "),
            status,
        ])
    } else {
        Line::from(vec![
            Span::styled(" HAROLD / TMUX  ", Style::default().fg(AMBER)),
            Span::styled(
                "AGENT SIGNAL BOARD",
                Style::default().fg(INK).add_modifier(Modifier::BOLD),
            ),
            Span::raw("  ·  "),
            status,
        ])
    };
    frame.render_widget(
        Paragraph::new(title).block(board_block().borders(Borders::ALL)),
        area,
    );
}

fn render_monitor(frame: &mut Frame<'_>, area: Rect, app: &App, now_ms: i64) {
    let degraded = app
        .degraded_health()
        .map(|health| format!("{}:{}", health.component, health.reason_code))
        .collect::<Vec<_>>();
    let unknown = app
        .snapshot
        .monitor_health
        .iter()
        .filter(|health| health.state == crate::app::MonitorHealthState::Unknown)
        .map(|health| format!("{}:{}", health.component, health.reason_code))
        .collect::<Vec<_>>();
    let mut text = if !degraded.is_empty() {
        Line::from(vec![
            Span::styled(
                " ▲ MONITOR DEGRADED ",
                Style::default().fg(CORAL).add_modifier(Modifier::BOLD),
            ),
            Span::styled(degraded.join("  "), Style::default().fg(INK)),
            Span::styled(
                "  · LAST COMMITTED ROWS RETAINED",
                Style::default().fg(MUTED),
            ),
        ])
    } else if unknown.is_empty() && !app.snapshot.monitor_health.is_empty() {
        Line::from(Span::styled(
            " MONITOR HEALTHY",
            Style::default().fg(GREEN).add_modifier(Modifier::BOLD),
        ))
    } else {
        let detail = if unknown.is_empty() {
            "no health observations".to_owned()
        } else {
            unknown.join("  ")
        };
        Line::from(vec![
            Span::styled(
                " ? MONITOR UNKNOWN ",
                Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
            ),
            Span::styled(detail, Style::default().fg(INK)),
        ])
    };
    if app.connection == crate::app::ConnectionState::Stale {
        text.push_span(Span::styled(
            format!(
                "  ·  Last committed snapshot {} ago",
                age(now_ms, app.snapshot.server_time_ms)
            ),
            Style::default().fg(CORAL).add_modifier(Modifier::BOLD),
        ));
    }
    frame.render_widget(
        Paragraph::new(text).block(board_block().borders(Borders::LEFT | Borders::RIGHT)),
        area,
    );
}

fn render_summary(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let (busy, idle, unknown) = app.state_counts();
    let summary = Line::from(vec![
        Span::styled(
            format!(" BUSY {busy:02} "),
            Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("    IDLE {idle:02} "),
            Style::default().fg(GREEN).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("    UNKNOWN {unknown:02} "),
            Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("    SERVER TIME {}", app.snapshot.server_time_ms),
            Style::default().fg(MUTED),
        ),
    ]);
    let borders = if area.height <= 2 {
        Borders::LEFT | Borders::RIGHT
    } else {
        Borders::ALL
    };
    frame.render_widget(
        Paragraph::new(summary).block(board_block().borders(borders)),
        area,
    );
}

fn render_search(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let visible_count = app.visible_rows().len();
    let mode = if app.search.editing {
        "EDITING"
    } else {
        "ACTIVE"
    };
    let query = Line::from(vec![
        Span::styled(" FILTER  ", Style::default().fg(MUTED)),
        Span::styled(
            format!("/ {}", app.search.query),
            Style::default().fg(INK).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("  [{mode}]"), Style::default().fg(AMBER)),
    ]);
    let count = format!("{visible_count} OF {} LOCAL ", app.snapshot.rows.len());
    let block = board_block().borders(Borders::LEFT | Borders::RIGHT);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let count_width = u16::try_from(count.chars().count())
        .unwrap_or(u16::MAX)
        .min(inner.width);
    let [query_area, count_area] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(count_width)]).areas(inner);
    frame.render_widget(Paragraph::new(query), query_area);
    frame.render_widget(
        Paragraph::new(count)
            .alignment(Alignment::Right)
            .style(Style::default().fg(MUTED)),
        count_area,
    );
}

fn render_workspace(frame: &mut Frame<'_>, area: Rect, app: &App, now_ms: i64) {
    match app.connection {
        crate::app::ConnectionState::Connecting => {
            render_state_message(
                frame,
                area,
                "Waiting for first Harold snapshot",
                "Connecting…",
            );
        }
        crate::app::ConnectionState::Unavailable => {
            render_state_message(frame, area, "Harold is unavailable", "Press r to retry now")
        }
        crate::app::ConnectionState::Live | crate::app::ConnectionState::Stale => {
            render_live_workspace(frame, area, app, now_ms);
        }
    }
}

fn render_live_workspace(frame: &mut Frame<'_>, area: Rect, app: &App, now_ms: i64) {
    let visible = app.visible_rows();
    if visible.is_empty() {
        let (title, guidance) = if app.snapshot.rows.is_empty() {
            (
                "No configured agent panes found",
                "Waiting for Harold inventory",
            )
        } else {
            ("No agents match this search", "Esc clears the local filter")
        };
        render_state_message(frame, area, title, guidance);
        return;
    }

    if area.width < WIDE_WIDTH {
        render_inventory(frame, area, app, now_ms);
        return;
    }

    let [inventory, detail] =
        Layout::horizontal([Constraint::Percentage(68), Constraint::Percentage(32)]).areas(area);
    if !detail_fits(detail, app, now_ms) {
        render_inventory(frame, area, app, now_ms);
        return;
    }
    render_inventory(frame, inventory, app, now_ms);
    render_detail(frame, detail, app, now_ms);
}

fn detail_fits(area: Rect, app: &App, now_ms: i64) -> bool {
    let Some(agent) = app.selected_row() else {
        return area.height >= 3;
    };
    let inner_width = area.width.saturating_sub(2);
    if inner_width == 0 {
        return false;
    }

    let lines = detail_lines(agent, now_ms);
    let wrapped_count = measured_wrapped_height(&lines, inner_width);
    let required_height = wrapped_count + 2;
    required_height <= usize::from(area.height)
}

fn measured_wrapped_height(lines: &[Line<'static>], width: u16) -> usize {
    if width == 0 {
        return 0;
    }
    conservative_grapheme_height(lines, usize::from(width))
}

fn conservative_grapheme_height(lines: &[Line<'static>], width: usize) -> usize {
    lines
        .iter()
        .map(|line| conservative_grapheme_line_height(line, width))
        .sum()
}

fn conservative_grapheme_line_height(line: &Line<'_>, width: usize) -> usize {
    let mut words = Vec::<(usize, Vec<usize>)>::new();
    let mut whitespace_width = 0;
    let mut word_whitespace = 0;
    let mut word = Vec::new();

    for grapheme in line.styled_graphemes(Style::default()) {
        let grapheme_width = Line::raw(grapheme.symbol).width();
        if grapheme.is_whitespace() {
            if !word.is_empty() {
                words.push((word_whitespace, std::mem::take(&mut word)));
            }
            whitespace_width += grapheme_width;
            continue;
        }
        if word.is_empty() {
            word_whitespace = whitespace_width;
            whitespace_width = 0;
        }
        word.push(grapheme_width);
    }
    if !word.is_empty() {
        words.push((word_whitespace, word));
    }

    let mut rows = 1;
    let mut used = 0;
    for (separating_width, graphemes) in words {
        let word_width = graphemes.iter().sum::<usize>();
        if word_width <= width {
            if used == 0 {
                used = word_width;
            } else if used + separating_width + word_width <= width {
                used += separating_width + word_width;
            } else {
                rows += 1;
                used = word_width;
            }
            continue;
        }

        if used > 0 {
            rows += 1;
            used = 0;
        }
        for grapheme_width in graphemes {
            if grapheme_width > width {
                continue;
            }
            if used + grapheme_width > width {
                rows += 1;
                used = 0;
            }
            used += grapheme_width;
        }
    }
    rows
}

fn render_state_message(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &'static str,
    guidance: &'static str,
) {
    frame.render_widget(
        Paragraph::new(vec![
            Line::styled(title, Style::default().fg(INK).add_modifier(Modifier::BOLD)),
            Line::styled(guidance, Style::default().fg(MUTED)),
        ])
        .alignment(Alignment::Center)
        .block(
            board_block()
                .title(" AGENT BLOCK OCCUPANCY ")
                .borders(Borders::ALL),
        ),
        area,
    );
}

fn render_inventory(frame: &mut Frame<'_>, area: Rect, app: &App, now_ms: i64) {
    let visible = app.visible_rows();
    let selected_index = app
        .selected
        .as_ref()
        .and_then(|selected| visible.iter().position(|row| &row.incarnation == selected));
    let rows = visible.into_iter().map(|agent| {
        let selected = app.selected.as_ref() == Some(&agent.incarnation);
        let marker = if selected { "▶ " } else { "  " };
        let style = if selected {
            Style::default()
                .fg(INK)
                .bg(Color::Rgb(41, 40, 30))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(INK)
        };
        let mut cells = vec![
            Cell::from(format!("{marker}{}", agent.state.label())),
            Cell::from(agent.provider_display_name.as_str()),
            Cell::from(agent.tmux_target.as_str()),
            Cell::from(display_work_summary(agent.work_summary.as_deref())),
        ];
        if area.width >= COMPACT_WIDTH {
            cells.push(Cell::from(age(now_ms, agent.last_transition_at_ms)));
        }
        Row::new(cells)
            .style(style)
            .height(u16::from(area.width >= COMPACT_WIDTH) + 1)
    });
    let (header, widths) = if area.width < COMPACT_WIDTH {
        (
            Row::new(["STATE", "AGENT", "TARGET", "WORK SUMMARY"]),
            vec![
                Constraint::Length(10),
                Constraint::Length(9),
                Constraint::Length(15),
                Constraint::Min(8),
            ],
        )
    } else {
        (
            Row::new(["STATE", "AGENT", "TARGET", "WORK SUMMARY", "AGE"]),
            vec![
                Constraint::Length(13),
                Constraint::Length(12),
                Constraint::Length(22),
                Constraint::Min(12),
                Constraint::Length(7),
            ],
        )
    };
    let table = Table::new(rows, widths)
        .header(header.style(Style::default().fg(MUTED).add_modifier(Modifier::BOLD)))
        .column_spacing(1)
        .block(
            board_block()
                .title(" AGENT BLOCK OCCUPANCY ")
                .borders(Borders::ALL),
        );
    let mut state = TableState::default().with_selected(selected_index);
    frame.render_stateful_widget(table, area, &mut state);
}

fn render_detail(frame: &mut Frame<'_>, area: Rect, app: &App, now_ms: i64) {
    let Some(agent) = app.selected_row() else {
        frame.render_widget(
            Paragraph::new("No visible agent selected")
                .style(Style::default().fg(MUTED))
                .block(
                    board_block()
                        .title(" SELECTED SIGNAL ")
                        .borders(Borders::ALL),
                ),
            area,
        );
        return;
    };

    let lines = detail_lines(agent, now_ms);
    frame.render_widget(
        detail_paragraph(lines).block(
            board_block()
                .title(" SELECTED SIGNAL ")
                .borders(Borders::ALL),
        ),
        area,
    );
}

fn detail_paragraph(lines: Vec<Line<'static>>) -> Paragraph<'static> {
    Paragraph::new(lines).wrap(Wrap { trim: true })
}

fn detail_lines(agent: &crate::app::AgentRow, now_ms: i64) -> Vec<Line<'static>> {
    vec![
        Line::styled(
            format!(
                "{}  {}",
                state_glyph(agent.state),
                agent.provider_display_name
            ),
            state_style(agent.state).add_modifier(Modifier::BOLD),
        ),
        Line::raw(""),
        fact("STATE", agent.state.label()),
        fact("TARGET", &agent.tmux_target),
        fact("PANE ID", &agent.incarnation.pane_id),
        fact("DIRECTORY", &agent.working_directory),
        fact("AGE", &age(now_ms, agent.last_transition_at_ms)),
        Line::raw(""),
        Line::styled(
            "CURRENT WORK",
            Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
        ),
        Line::styled(
            display_work_summary(agent.work_summary.as_deref()).to_owned(),
            Style::default().fg(INK),
        ),
    ]
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, compact: bool) {
    let spans = if compact {
        vec![
            Span::styled(" j/k", Style::default().fg(INK)),
            Span::raw(" select  "),
            Span::styled("/", Style::default().fg(INK)),
            Span::raw(" search  "),
            Span::styled("Enter", Style::default().fg(INK)),
            Span::raw(" switch  "),
            Span::styled("q/Esc", Style::default().fg(INK)),
            Span::raw(" quit "),
        ]
    } else {
        vec![
            Span::styled(" TMX DISPATCH CONSOLE  ", Style::default().fg(MUTED)),
            Span::styled("j/k", Style::default().fg(INK)),
            Span::raw(" select   "),
            Span::styled("/", Style::default().fg(INK)),
            Span::raw(" search   "),
            Span::styled("Enter", Style::default().fg(INK)),
            Span::raw(" switch pane   "),
            Span::styled("q / Esc", Style::default().fg(INK)),
            Span::raw(" quit "),
        ]
    };
    frame.render_widget(
        Paragraph::new(Line::from(spans)).block(board_block().borders(Borders::ALL)),
        area,
    );
}

fn board_block<'a>() -> Block<'a> {
    Block::default()
        .style(Style::default().bg(PANEL).fg(INK))
        .border_style(Style::default().fg(Color::Rgb(90, 86, 72)))
}

fn fact(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<10}"), Style::default().fg(MUTED)),
        Span::styled(value.to_owned(), Style::default().fg(INK)),
    ])
}

fn state_glyph(state: AgentState) -> &'static str {
    match state {
        AgentState::Busy => "●",
        AgentState::Idle => "○",
        AgentState::Unknown => "·",
    }
}

fn state_style(state: AgentState) -> Style {
    let color = match state {
        AgentState::Busy => AMBER,
        AgentState::Idle => GREEN,
        AgentState::Unknown => MUTED,
    };
    Style::default().fg(color)
}

fn transport_style(connection: crate::app::ConnectionState) -> Style {
    let color = match connection {
        crate::app::ConnectionState::Live => GREEN,
        crate::app::ConnectionState::Connecting => AMBER,
        crate::app::ConnectionState::Unavailable | crate::app::ConnectionState::Stale => CORAL,
    };
    Style::default().fg(color)
}

fn age(now_ms: i64, transition_ms: i64) -> String {
    let elapsed_ms = now_ms.saturating_sub(transition_ms).max(0);
    format!("{}s", elapsed_ms / 1_000)
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyCode;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::text::Line;

    use super::render;
    use crate::app::{
        AgentIncarnation, AgentRow, AgentState, App, ConnectionState, MonitorHealth,
        MonitorHealthState, SearchState, Snapshot,
    };

    #[test]
    fn wide_dashboard_shows_operator_signals_without_evidence_provenance() {
        let selected = incarnation("%17", 91700, 91844, "codex");
        let app = App::new(
            ConnectionState::Live,
            Snapshot {
                through_event_version: 1842,
                server_time_ms: 1_777_000_000_000,
                monitor_health: vec![
                    MonitorHealth {
                        component: "inventory".into(),
                        state: MonitorHealthState::Degraded,
                        reason_code: "tmux_unavailable".into(),
                        observed_at_ms: 1_776_999_999_000,
                    },
                    MonitorHealth {
                        component: "screen".into(),
                        state: MonitorHealthState::Healthy,
                        reason_code: "ok".into(),
                        observed_at_ms: 1_776_999_999_500,
                    },
                ],
                rows: vec![
                    row(
                        selected.clone(),
                        AgentState::Busy,
                        "Codex",
                        "tmx-agent-dash:0.1",
                        "Build event snapshot dashboard",
                        1_776_999_992_000,
                    ),
                    row(
                        incarnation("%22", 92000, 92100, "claude"),
                        AgentState::Idle,
                        "Claude",
                        "harold:2.1",
                        "Review event projection contract",
                        1_776_999_970_000,
                    ),
                    row(
                        incarnation("%31", 93000, 93100, "opencode"),
                        AgentState::Unknown,
                        "OpenCode",
                        "lab:1.0",
                        "Awaiting assignment",
                        1_776_999_900_000,
                    ),
                ],
            },
            SearchState {
                query: "event".into(),
                editing: true,
            },
            Some(selected),
        );
        let backend = TestBackend::new(140, 38);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| render(frame, &app, 1_777_000_000_000))
            .unwrap();

        let content = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>()
            .join("");

        for expected in [
            "LIVE",
            "MONITOR DEGRADED",
            "BUSY",
            "IDLE",
            "UNKNOWN",
            "WORK SUMMARY",
            "/ event",
            "2 OF 3",
            "CURRENT WORK",
            "Build event snapshot dashboard",
            "Claude",
            "inventory:tmux_unavailable",
        ] {
            assert!(content.contains(expected), "missing {expected:?}");
        }
        for forbidden in ["EVIDENCE", "HOOK", "SCREEN"] {
            assert!(!content.contains(forbidden), "found {forbidden:?}");
        }
    }

    #[test]
    fn wide_dashboard_does_not_present_unknown_monitor_health_as_healthy() {
        let app = App::new(
            ConnectionState::Live,
            Snapshot {
                through_event_version: 1843,
                server_time_ms: 1_777_000_000_000,
                monitor_health: vec![MonitorHealth {
                    component: "inventory".into(),
                    state: MonitorHealthState::Unknown,
                    reason_code: "not_observed".into(),
                    observed_at_ms: 1_776_999_999_000,
                }],
                rows: Vec::new(),
            },
            SearchState {
                query: String::new(),
                editing: false,
            },
            None,
        );
        let backend = TestBackend::new(140, 38);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| render(frame, &app, 1_777_000_000_000))
            .unwrap();

        let content = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>()
            .join("");
        assert!(content.contains("MONITOR UNKNOWN"));
        assert!(!content.contains("MONITOR HEALTHY"));
    }

    #[test]
    fn wide_dashboard_presents_empty_monitor_health_as_unknown() {
        let app = App::new(
            ConnectionState::Live,
            Snapshot {
                through_event_version: 1843,
                server_time_ms: 1_777_000_000_000,
                monitor_health: Vec::new(),
                rows: Vec::new(),
            },
            SearchState {
                query: String::new(),
                editing: false,
            },
            None,
        );
        let backend = TestBackend::new(140, 38);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| render(frame, &app, 1_777_000_000_000))
            .unwrap();

        let content = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>()
            .join("");
        assert!(content.contains("MONITOR UNKNOWN"));
        assert!(content.contains("no health observations"));
        assert!(!content.contains("MONITOR HEALTHY"));
    }

    #[test]
    fn medium_dashboard_hides_detail_but_keeps_table_signals() {
        let selected = incarnation("%17", 91700, 91844, "codex");
        let app = live_app(
            vec![row(
                selected.clone(),
                AgentState::Busy,
                "Codex",
                "agents:2.7",
                "Build responsive dashboard",
                90_000,
            )],
            Some(selected),
        );

        let content = rendered(&app, 104, 28, 100_000);

        for expected in [
            "STATE",
            "AGENT",
            "TARGET",
            "WORK SUMMARY",
            "Codex",
            "agents:2.7",
            "Build responsive dashboard",
        ] {
            assert!(content.contains(expected), "missing {expected:?}");
        }
        assert!(!content.contains("CURRENT WORK"));
        assert!(!content.contains("SELECTED SIGNAL"));
    }

    #[test]
    fn compact_dashboard_preserves_state_provider_target_and_truncated_summary() {
        let selected = incarnation("%17", 91700, 91844, "codex");
        let app = live_app(
            vec![row(
                selected.clone(),
                AgentState::Busy,
                "Codex",
                "agents:2.7",
                "Responsive summary remains visible at compact widths",
                90_000,
            )],
            Some(selected),
        );

        let content = rendered(&app, 72, 22, 100_000);

        for expected in [
            "STATE",
            "AGENT",
            "TARGET",
            "WORK SUMMARY",
            "BUSY",
            "BUSY 01",
            "IDLE 00",
            "Codex",
            "agents:2.7",
            "Responsive",
        ] {
            assert!(content.contains(expected), "missing {expected:?}");
        }
        assert!(!content.contains("CURRENT WORK"));
    }

    #[test]
    fn undersized_dashboard_replaces_layout_with_resize_instruction() {
        let app = live_app(Vec::new(), None);

        let content = rendered(&app, 59, 17, 100_000);

        assert!(content.contains("Terminal too small"));
        assert!(content.contains("Resize to at least 60x18"));
        assert!(!content.contains("AGENT BLOCK OCCUPANCY"));
    }

    #[test]
    fn connecting_dashboard_explains_that_first_snapshot_is_loading() {
        let app = App::new(
            ConnectionState::Connecting,
            snapshot(Vec::new(), Vec::new()),
            empty_search(),
            None,
        );

        let content = rendered(&app, 104, 28, 100_000);

        assert!(content.contains("TRANSPORT CONNECTING"));
        assert!(content.contains("Waiting for first Harold snapshot"));
    }

    #[test]
    fn unavailable_dashboard_shows_retry_guidance_without_empty_live_copy() {
        let app = App::new(
            ConnectionState::Unavailable,
            snapshot(Vec::new(), Vec::new()),
            empty_search(),
            None,
        );

        let content = rendered(&app, 104, 28, 100_000);

        assert!(content.contains("TRANSPORT UNAVAILABLE"));
        assert!(content.contains("Harold is unavailable"));
        assert!(content.contains("Press r to retry now"));
        assert!(!content.contains("No configured agent panes found"));
    }

    #[test]
    fn stale_dashboard_marks_age_and_retains_last_committed_rows() {
        let selected = incarnation("%17", 91700, 91844, "codex");
        let app = App::new(
            ConnectionState::Stale,
            Snapshot {
                through_event_version: 42,
                server_time_ms: 90_000,
                monitor_health: vec![health(MonitorHealthState::Healthy, "ok")],
                rows: vec![row(
                    selected.clone(),
                    AgentState::Busy,
                    "Codex",
                    "agents:2.7",
                    "Retained work remains readable",
                    80_000,
                )],
            },
            empty_search(),
            Some(selected),
        );

        let content = rendered(&app, 104, 28, 100_000);

        assert!(content.contains("TRANSPORT STALE"));
        assert!(content.contains("Last committed snapshot 10s ago"));
        assert!(content.contains("Retained work remains readable"));
    }

    #[test]
    fn healthy_live_dashboard_labels_transport_and_monitor_separately() {
        let app = live_app(Vec::new(), None);

        let content = rendered(&app, 104, 28, 100_000);

        assert!(content.contains("TRANSPORT LIVE"));
        assert!(content.contains("MONITOR HEALTHY"));
    }

    #[test]
    fn accepted_healthy_snapshot_clears_unknown_monitor_warning() {
        let mut app = App::new(
            ConnectionState::Live,
            snapshot(
                vec![health(MonitorHealthState::Unknown, "not_observed")],
                Vec::new(),
            ),
            empty_search(),
            None,
        );
        assert!(
            rendered(&app, 104, 28, 100_000).contains("MONITOR UNKNOWN"),
            "precondition: unknown health must be visible"
        );

        app.apply_later_snapshot(Snapshot {
            through_event_version: 43,
            server_time_ms: 100_000,
            monitor_health: vec![health(MonitorHealthState::Healthy, "ok")],
            rows: Vec::new(),
        })
        .unwrap();
        let recovered = rendered(&app, 104, 28, 100_000);

        assert!(recovered.contains("MONITOR HEALTHY"));
        assert!(!recovered.contains("MONITOR UNKNOWN"));
    }

    #[test]
    fn degraded_live_dashboard_retains_rows_beneath_distinct_warning() {
        let selected = incarnation("%17", 91700, 91844, "codex");
        let app = App::new(
            ConnectionState::Live,
            Snapshot {
                through_event_version: 42,
                server_time_ms: 100_000,
                monitor_health: vec![health(MonitorHealthState::Degraded, "capture_failed")],
                rows: vec![row(
                    selected.clone(),
                    AgentState::Busy,
                    "Codex",
                    "agents:2.7",
                    "Last committed work",
                    90_000,
                )],
            },
            empty_search(),
            Some(selected),
        );

        let content = rendered(&app, 104, 28, 100_000);

        assert!(content.contains("TRANSPORT LIVE"));
        assert!(content.contains("MONITOR DEGRADED"));
        assert!(content.contains("inventory:capture_failed"));
        assert!(content.contains("Last committed work"));
        assert!(!content.contains("MONITOR HEALTHY"));
    }

    #[test]
    fn empty_live_dashboard_has_specific_configuration_copy() {
        let app = live_app(Vec::new(), None);

        let content = rendered(&app, 104, 28, 100_000);

        assert!(content.contains("No configured agent panes found"));
        assert!(content.contains("TRANSPORT LIVE"));
        assert!(content.contains("REV #00042"));
    }

    #[test]
    fn live_search_with_no_matches_has_distinct_copy() {
        let row = row(
            incarnation("%17", 91700, 91844, "codex"),
            AgentState::Busy,
            "Codex",
            "agents:2.7",
            "Build dashboard",
            90_000,
        );
        let app = App::new(
            ConnectionState::Live,
            snapshot(vec![health(MonitorHealthState::Healthy, "ok")], vec![row]),
            SearchState {
                query: "no-such-agent".into(),
                editing: true,
            },
            None,
        );

        let content = rendered(&app, 104, 28, 100_000);

        assert!(content.contains("No agents match this search"));
        assert!(content.contains("0 OF 1"));
    }

    #[test]
    fn compact_search_keeps_visible_total_count_when_query_is_long() {
        let row = row(
            incarnation("%17", 91700, 91844, "codex"),
            AgentState::Busy,
            "Codex",
            "agents:2.7",
            "Build dashboard",
            90_000,
        );
        let app = App::new(
            ConnectionState::Live,
            snapshot(vec![health(MonitorHealthState::Healthy, "ok")], vec![row]),
            SearchState {
                query: "a-very-long-local-filter-that-does-not-match-any-agent".into(),
                editing: true,
            },
            None,
        );

        let content = rendered(&app, 72, 22, 100_000);

        assert!(content.contains("0 OF 1 LOCAL"));
    }

    #[test]
    fn missing_summary_uses_exact_copy_in_table_and_detail() {
        let selected = incarnation("%17", 91700, 91844, "codex");
        let mut missing = row(
            selected.clone(),
            AgentState::Idle,
            "Codex",
            "agents:2.7",
            "ignored",
            90_000,
        );
        missing.work_summary = None;
        let app = live_app(vec![missing], Some(selected));

        let content = rendered(&app, 140, 38, 100_000);

        assert_eq!(content.matches("No work summary reported").count(), 2);
    }

    #[test]
    fn selection_marker_moves_to_new_row_after_selection_change() {
        let first = incarnation("%17", 91700, 91844, "codex");
        let second = incarnation("%18", 92700, 92844, "claude");
        let mut app = live_app(
            vec![
                row(
                    first.clone(),
                    AgentState::Busy,
                    "Codex",
                    "agents:2.7",
                    "First task",
                    90_000,
                ),
                row(
                    second,
                    AgentState::Idle,
                    "Claude",
                    "agents:2.8",
                    "Second task",
                    90_000,
                ),
            ],
            Some(first),
        );

        let before = rendered(&app, 104, 28, 100_000);
        assert!(before.contains("▶ BUSY"));
        assert!(!before.contains("▶ IDLE"));

        app.handle_key(KeyCode::Char('j'));
        let after = rendered(&app, 104, 28, 100_000);
        assert!(!after.contains("▶ BUSY"));
        assert!(after.contains("▶ IDLE"));
    }

    #[test]
    fn selected_row_below_initial_table_viewport_scrolls_into_view() {
        let rows = (0_u32..10)
            .map(|index| {
                row(
                    incarnation(
                        &format!("%{}", index + 10),
                        91_700 + index,
                        91_844 + index,
                        "codex",
                    ),
                    AgentState::Idle,
                    "Codex",
                    &format!("agents:2.{index}"),
                    &format!("Work item {index}"),
                    90_000,
                )
            })
            .collect::<Vec<_>>();
        let selected = rows.last().unwrap().incarnation.clone();
        let app = live_app(rows, Some(selected));

        let content = rendered(&app, 104, 28, 100_000);

        assert!(content.contains("▶ IDLE"));
        assert!(content.contains("agents:2.9"));
        assert!(content.contains("Work item 9"));
    }

    #[test]
    fn wide_but_short_dashboard_uses_full_width_inventory_without_clipped_detail() {
        let selected = incarnation("%17", 91700, 91844, "codex");
        let app = live_app(
            vec![row(
                selected.clone(),
                AgentState::Busy,
                "Codex",
                "agents:2.7",
                "Build responsive dashboard",
                90_000,
            )],
            Some(selected),
        );

        let content = rendered(&app, 140, 18, 100_000);

        assert!(content.contains("AGENT BLOCK OCCUPANCY"));
        assert!(content.contains("Build responsive dashboard"));
        assert!(!content.contains("SELECTED SIGNAL"));
        assert!(!content.contains("CURRENT WORK"));
        assert!(!content.contains("/Users/kahgeh/Dev/p/Codex"));
    }

    #[test]
    fn exact_minimum_dashboard_prioritizes_transport_and_revision_in_masthead() {
        let app = live_app(Vec::new(), None);

        let content = rendered(&app, 60, 18, 100_000);

        assert!(content.contains("TMX DASH"));
        assert!(content.contains("TRANSPORT LIVE"));
        assert!(content.contains("REV #00042"));
        assert!(!content.contains("Terminal too small"));
    }

    #[test]
    fn wide_detail_is_omitted_when_wrapped_content_would_clip() {
        let (app, directory, summary) = long_detail_app();

        let content = rendered(&app, 140, 27, 100_000);

        assert!(content.contains(&summary));
        assert!(!content.contains("SELECTED SIGNAL"));
        assert!(!content.contains("CURRENT WORK"));
        assert!(!content.contains(&directory));
    }

    #[test]
    fn sufficiently_tall_wide_detail_renders_complete_wrapped_content() {
        let (app, _directory, _summary) = long_detail_app();

        let content = rendered(&app, 140, 44, 100_000);

        assert!(content.contains("SELECTED SIGNAL"));
        assert!(content.contains("CURRENT WORK"));
        assert!(content.contains("/Users/kahgeh/Dev/p/tmx-agent-dash"));
        assert!(content.contains("current-feature"));
        assert!(content.contains("Complete the responsive renderer"));
        assert!(content.contains("operator-facing signal"));
    }

    #[test]
    fn odd_width_cjk_detail_overflow_uses_full_width_inventory() {
        let selected = incarnation("%17", 91700, 91844, "codex");
        let summary = "界".repeat(43);
        let mut agent = row(
            selected.clone(),
            AgentState::Busy,
            "Codex",
            "tmx-agent-dash:2.17",
            &summary,
            90_000,
        );
        agent.working_directory = "/Users/kahgeh/Dev/p/tmx-agent-dash".into();
        let app = live_app(vec![agent], Some(selected));

        let content = rendered(&app, 140, 29, 100_000);

        assert!(!content.contains("SELECTED SIGNAL"));
        assert!(!content.contains("CURRENT WORK"));
        assert!(content.contains("AGENT BLOCK OCCUPANCY"));
        assert!(content.contains("▶ BUSY"));
        assert!(content.contains("tmx-agent-dash:2.17"));
        assert_eq!(content.matches('界').count(), 40);
    }

    #[test]
    fn wrapped_measurement_treats_zwj_family_sequences_as_grapheme_clusters() {
        let line = Line::raw("👨‍👩‍👧‍👦👨‍👩‍👧‍👦👨‍👩‍👧‍👦");

        assert_eq!(line.width(), 6);
        assert_eq!(super::measured_wrapped_height(&[line], 3), 3);
    }

    #[test]
    fn wrapped_measurement_respects_combining_keycap_cluster_width() {
        let line = Line::raw("1️⃣1️⃣1️⃣");

        assert_eq!(line.width(), 6);
        assert_eq!(super::measured_wrapped_height(&[line], 3), 3);
    }

    #[test]
    fn wrapped_measurement_matches_cjk_odd_width_boundary() {
        let line = Line::raw("界".repeat(43));

        assert_eq!(line.width(), 86);
        assert_eq!(super::measured_wrapped_height(&[line], 43), 3);
    }

    #[test]
    fn wrapped_measurement_handles_zero_width_without_allocating_a_screen() {
        let line = Line::raw("no drawable columns");

        assert_eq!(super::measured_wrapped_height(&[line], 0), 0);
    }

    #[test]
    fn wrapped_measurement_counts_long_zwj_input_by_grapheme_width() {
        let line = Line::raw("👨‍👩‍👧‍👦".repeat(1_024));

        assert_eq!(line.width(), 2_048);
        assert_eq!(super::measured_wrapped_height(&[line], 43), 49);
    }

    #[test]
    fn wrapped_measurement_matches_trimmed_word_and_whitespace_boundaries() {
        let line = Line::raw(
            "abcd efghij    klmnopabcd efgh     ijklmnopabcdefg hijkl mnopab c d e f g h i j k l m n o",
        );

        assert_eq!(super::measured_wrapped_height(&[line], 20), 5);
    }

    fn rendered(app: &App, width: u16, height: u16, now_ms: i64) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, app, now_ms)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>()
            .join("")
    }

    fn live_app(rows: Vec<AgentRow>, selected: Option<AgentIncarnation>) -> App {
        App::new(
            ConnectionState::Live,
            snapshot(vec![health(MonitorHealthState::Healthy, "ok")], rows),
            empty_search(),
            selected,
        )
    }

    fn long_detail_app() -> (App, String, String) {
        let selected = incarnation("%17", 91700, 91844, "codex");
        let directory =
            "/Users/kahgeh/Dev/p/tmx-agent-dash/worktrees/responsive-dashboard/current-feature"
                .to_owned();
        let summary =
            "Complete the responsive renderer while retaining every operator-facing signal"
                .to_owned();
        let mut agent = row(
            selected.clone(),
            AgentState::Busy,
            "Codex",
            "tmx-agent-dash:2.17",
            &summary,
            90_000,
        );
        agent.working_directory = directory.clone();
        (live_app(vec![agent], Some(selected)), directory, summary)
    }

    fn snapshot(monitor_health: Vec<MonitorHealth>, rows: Vec<AgentRow>) -> Snapshot {
        Snapshot {
            through_event_version: 42,
            server_time_ms: 100_000,
            monitor_health,
            rows,
        }
    }

    fn health(state: MonitorHealthState, reason_code: &str) -> MonitorHealth {
        MonitorHealth {
            component: "inventory".into(),
            state,
            reason_code: reason_code.into(),
            observed_at_ms: 99_000,
        }
    }

    fn empty_search() -> SearchState {
        SearchState {
            query: String::new(),
            editing: false,
        }
    }

    fn incarnation(
        pane_id: &str,
        pane_pid: u32,
        agent_pid: u32,
        provider_id: &str,
    ) -> AgentIncarnation {
        AgentIncarnation {
            pane_id: pane_id.into(),
            pane_pid,
            agent_pid,
            agent_started_at_ms: 1_776_999_000_000 + i64::from(agent_pid),
            provider_id: provider_id.into(),
        }
    }

    fn row(
        incarnation: AgentIncarnation,
        state: AgentState,
        provider_display_name: &str,
        tmux_target: &str,
        work_summary: &str,
        last_transition_at_ms: i64,
    ) -> AgentRow {
        AgentRow {
            incarnation,
            provider_display_name: provider_display_name.into(),
            tmux_target: tmux_target.into(),
            session_name: tmux_target.split(':').next().unwrap().into(),
            window_index: 0,
            pane_index: 0,
            working_directory: format!("/Users/kahgeh/Dev/p/{provider_display_name}"),
            work_summary: Some(work_summary.into()),
            state,
            last_transition_at_ms,
        }
    }
}
