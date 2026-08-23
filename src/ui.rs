use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, Wrap};

use crate::app::{AgentState, App};
use crate::text::display_work_summary;

const BOARD: Color = Color::Rgb(20, 20, 16);
const PANEL: Color = Color::Rgb(25, 25, 21);
const INK: Color = Color::Rgb(237, 231, 216);
const MUTED: Color = Color::Rgb(143, 138, 127);
const AMBER: Color = Color::Rgb(224, 174, 85);
const GREEN: Color = Color::Rgb(125, 200, 139);
const CORAL: Color = Color::Rgb(224, 114, 98);

pub fn render(frame: &mut Frame<'_>, app: &App, now_ms: i64) {
    frame.render_widget(
        Block::default().style(Style::default().bg(BOARD)),
        frame.area(),
    );

    let [masthead, monitor, summary, search, workspace, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Min(12),
        Constraint::Length(3),
    ])
    .areas(frame.area());

    render_masthead(frame, masthead, app);
    render_monitor(frame, monitor, app);
    render_summary(frame, summary, app);
    render_search(frame, search, app);
    render_workspace(frame, workspace, app, now_ms);
    render_footer(frame, footer);
}

fn render_masthead(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let title = Line::from(vec![
        Span::styled(" HAROLD / TMUX CONTROL  ", Style::default().fg(AMBER)),
        Span::styled(
            "AGENT SIGNAL BOARD",
            Style::default().fg(INK).add_modifier(Modifier::BOLD),
        ),
        Span::raw("                                  "),
        Span::styled(
            format!(
                "{}  REV #{:05}",
                app.connection.label(),
                app.snapshot.through_event_version
            ),
            Style::default().fg(GREEN).add_modifier(Modifier::BOLD),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(title).block(board_block().borders(Borders::ALL)),
        area,
    );
}

fn render_monitor(frame: &mut Frame<'_>, area: Rect, app: &App) {
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
    let text = if !degraded.is_empty() {
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
    frame.render_widget(
        Paragraph::new(summary).block(board_block().borders(Borders::ALL)),
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
    let line = Line::from(vec![
        Span::styled(" FILTER  ", Style::default().fg(MUTED)),
        Span::styled(
            format!("/ {}", app.search.query),
            Style::default().fg(INK).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("  [{mode}]"), Style::default().fg(AMBER)),
        Span::styled(
            format!(
                "                                  {visible_count} OF {}  LOCAL",
                app.snapshot.rows.len()
            ),
            Style::default().fg(MUTED),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(line).block(board_block().borders(Borders::LEFT | Borders::RIGHT)),
        area,
    );
}

fn render_workspace(frame: &mut Frame<'_>, area: Rect, app: &App, now_ms: i64) {
    let [inventory, detail] =
        Layout::horizontal([Constraint::Percentage(68), Constraint::Percentage(32)]).areas(area);
    render_inventory(frame, inventory, app, now_ms);
    render_detail(frame, detail, app, now_ms);
}

fn render_inventory(frame: &mut Frame<'_>, area: Rect, app: &App, now_ms: i64) {
    let rows = app.visible_rows().into_iter().map(|agent| {
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
        Row::new(vec![
            Cell::from(format!("{marker}{}", agent.state.label())),
            Cell::from(agent.provider_display_name.as_str()),
            Cell::from(agent.tmux_target.as_str()),
            Cell::from(display_work_summary(agent.work_summary.as_deref())),
            Cell::from(age(now_ms, agent.last_transition_at_ms)),
        ])
        .style(style)
        .height(2)
    });
    let header = Row::new(["STATE", "AGENT", "TARGET", "WORK SUMMARY", "AGE"])
        .style(Style::default().fg(MUTED).add_modifier(Modifier::BOLD));
    let table = Table::new(
        rows,
        [
            Constraint::Length(13),
            Constraint::Length(12),
            Constraint::Length(22),
            Constraint::Min(28),
            Constraint::Length(7),
        ],
    )
    .header(header)
    .column_spacing(1)
    .block(
        board_block()
            .title(" AGENT BLOCK OCCUPANCY ")
            .borders(Borders::ALL),
    );
    frame.render_widget(table, area);
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

    let transition_age = age(now_ms, agent.last_transition_at_ms);
    let lines = vec![
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
        fact("AGE", &transition_age),
        Line::raw(""),
        Line::styled(
            "CURRENT WORK",
            Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
        ),
        Line::styled(
            display_work_summary(agent.work_summary.as_deref()),
            Style::default().fg(INK),
        ),
    ];
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: true }).block(
            board_block()
                .title(" SELECTED SIGNAL ")
                .borders(Borders::ALL),
        ),
        area,
    );
}

fn render_footer(frame: &mut Frame<'_>, area: Rect) {
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" TMX DISPATCH CONSOLE  ", Style::default().fg(MUTED)),
            Span::styled("j/k", Style::default().fg(INK)),
            Span::raw(" select   "),
            Span::styled("/", Style::default().fg(INK)),
            Span::raw(" search   "),
            Span::styled("Enter", Style::default().fg(INK)),
            Span::raw(" switch pane   "),
            Span::styled("q / Esc", Style::default().fg(INK)),
            Span::raw(" quit "),
        ]))
        .block(board_block().borders(Borders::ALL)),
        area,
    );
}

fn board_block<'a>() -> Block<'a> {
    Block::default()
        .style(Style::default().bg(PANEL).fg(INK))
        .border_style(Style::default().fg(Color::Rgb(90, 86, 72)))
}

fn fact<'a>(label: &'a str, value: &'a str) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("{label:<10}"), Style::default().fg(MUTED)),
        Span::styled(value, Style::default().fg(INK)),
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

fn age(now_ms: i64, transition_ms: i64) -> String {
    let elapsed_ms = now_ms.saturating_sub(transition_ms).max(0);
    format!("{}s", elapsed_ms / 1_000)
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

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
