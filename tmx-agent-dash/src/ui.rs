use std::borrow::Cow;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap};

use crate::app::{AgentState, App, RuntimeStatus};
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
/// Narrowest terminal that fits the long-form footer with every hint visible.
const WIDE_FOOTER_WIDTH: u16 = 100;
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
    render_footer(frame, footer, compact || area.width < WIDE_FOOTER_WIDTH);
    render_palette(frame, area, app);
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

fn voice_style(voice: crate::app::VoiceState) -> Style {
    let color = match voice {
        crate::app::VoiceState::On => GREEN,
        crate::app::VoiceState::Muted => AMBER,
        crate::app::VoiceState::Unknown => MUTED,
    };
    Style::default().fg(color).add_modifier(Modifier::BOLD)
}

fn render_masthead(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let voice = app.voice();
    let tail = vec![
        Span::styled(
            format!(
                "TRANSPORT {}  ·  REV #{:05}",
                app.connection.label(),
                app.snapshot.through_event_version
            ),
            transport_style(app.connection).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  ·  "),
        Span::styled(voice.label(), voice_style(voice)),
    ];
    let mut spans = if area.width < COMPACT_WIDTH {
        vec![
            Span::styled(" TMX DASH ", Style::default().fg(AMBER)),
            Span::raw("· "),
        ]
    } else {
        vec![
            Span::styled(" HAROLD / TMUX  ", Style::default().fg(AMBER)),
            Span::styled(
                "AGENT SIGNAL BOARD",
                Style::default().fg(INK).add_modifier(Modifier::BOLD),
            ),
            Span::raw("  ·  "),
        ]
    };
    spans.extend(tail.clone());
    let mut title = Line::from(spans);
    if title.width() > usize::from(area.width.saturating_sub(2)) {
        // Transport, revision and voice outrank the title when the masthead is tight.
        title = Line::from(
            std::iter::once(Span::raw(" "))
                .chain(tail)
                .collect::<Vec<_>>(),
        );
    }
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
        let stale_age = app
            .last_snapshot_received_at_ms()
            .map(|received_at_ms| format!("{} ago", age(now_ms, received_at_ms)))
            .unwrap_or_else(|| "age unavailable".to_owned());
        text.push_span(Span::styled(
            format!("  ·  Last committed snapshot {stale_age}"),
            Style::default().fg(CORAL).add_modifier(Modifier::BOLD),
        ));
    }
    if let Some(status) = app.runtime_status() {
        let label = match status {
            RuntimeStatus::Retrying {
                endpoint,
                detail,
                delay_ms,
            } => {
                format!("RETRY IN {delay_ms}ms: {endpoint}: {detail}")
            }
            RuntimeStatus::NavigationUnavailable => "NAVIGATION UNAVAILABLE".to_owned(),
            RuntimeStatus::NavigationFailed(detail) => {
                format!("NAVIGATION FAILED: {detail}")
            }
            RuntimeStatus::VoiceFailed(detail) => format!("VOICE ERROR: {detail}"),
            RuntimeStatus::SourceError(detail) => format!("SOURCE ERROR: {detail}"),
        };
        text.push_span(Span::styled(
            format!("  ·  {label}"),
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
            format!("f {}", app.search.query),
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
    let detail_lines = app.selected_row().map(|agent| detail_lines(agent, now_ms));
    if !detail_fits(detail, detail_lines.as_deref()) {
        render_inventory(frame, area, app, now_ms);
        return;
    }
    render_inventory(frame, inventory, app, now_ms);
    render_detail(frame, detail, detail_lines);
}

fn detail_fits(area: Rect, lines: Option<&[Line<'_>]>) -> bool {
    let Some(lines) = lines else {
        return area.height >= 3;
    };
    let inner_width = area.width.saturating_sub(2);
    if inner_width == 0 {
        return false;
    }

    let wrapped_count = measured_wrapped_height(lines, inner_width);
    let required_height = wrapped_count + 2;
    required_height <= usize::from(area.height)
}

fn measured_wrapped_height(lines: &[Line<'_>], width: u16) -> usize {
    if width == 0 {
        return 0;
    }
    conservative_grapheme_height(lines, usize::from(width))
}

fn conservative_grapheme_height(lines: &[Line<'_>], width: usize) -> usize {
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
    let wide = area.width >= COMPACT_WIDTH;
    let where_width: u16 = if wide { 26 } else { 20 };
    let mut rows = Vec::new();
    let mut selected_index = None;
    for (group, agents) in app.visible_groups() {
        let mut header = Row::new(vec![
            Cell::from(""),
            Cell::from(Line::styled(
                fit(group, usize::from(where_width)).into_owned(),
                Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
            )),
        ]);
        if wide && !rows.is_empty() {
            header = header.top_margin(1);
        }
        rows.push(header);
        for agent in agents {
            let selected = app.selected.as_ref() == Some(&agent.incarnation);
            if selected {
                selected_index = Some(rows.len());
            }
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
                Cell::from(Line::styled(
                    format!(
                        "{marker}{} {}",
                        state_glyph(agent.state),
                        agent.state.label()
                    ),
                    state_style(agent.state),
                )),
                Cell::from(format!(
                    "  {}",
                    fit(app.label(agent), usize::from(where_width) - 2)
                )),
                Cell::from(Line::styled(
                    if app.is_waiting(&agent.incarnation) {
                        "◆"
                    } else {
                        ""
                    },
                    Style::default().fg(AMBER),
                )),
                Cell::from(Line::styled(
                    agent.provider_tag(),
                    Style::default().fg(MUTED),
                )),
                Cell::from(display_work_summary(agent.work_summary.as_deref())),
            ];
            if wide {
                cells.push(Cell::from(age(now_ms, agent.last_transition_at_ms)));
            }
            rows.push(Row::new(cells).style(style));
        }
    }
    let (header, widths) = if wide {
        (
            Row::new(["STATE", "WHERE", "", "", "WORK SUMMARY", "AGE"]),
            vec![
                Constraint::Length(13),
                Constraint::Length(where_width),
                Constraint::Length(1),
                Constraint::Length(2),
                Constraint::Min(12),
                Constraint::Length(7),
            ],
        )
    } else {
        (
            Row::new(["STATE", "WHERE", "", "", "WORK SUMMARY"]),
            vec![
                Constraint::Length(11),
                Constraint::Length(where_width),
                Constraint::Length(1),
                Constraint::Length(2),
                Constraint::Min(8),
            ],
        )
    };
    const LIST_TITLE: &str = " AGENT BLOCK OCCUPANCY ";
    let title_room = usize::from(area.width)
        .saturating_sub(2)
        .saturating_sub(LIST_TITLE.chars().count() + 1);
    let block = board_block()
        .title(LIST_TITLE)
        .title_top(waiting_title(app, title_room).right_aligned())
        .borders(Borders::ALL);
    let table = Table::new(rows, widths)
        .header(header.style(Style::default().fg(MUTED).add_modifier(Modifier::BOLD)))
        .column_spacing(1)
        .block(block);
    let mut state = TableState::default().with_selected(selected_index);
    frame.render_stateful_widget(table, area, &mut state);
}

fn waiting_entry(app: &App, row: &crate::app::AgentRow) -> String {
    if row.group() == row.name() {
        app.label(row).to_owned()
    } else {
        format!("{}/{}", row.group(), app.label(row))
    }
}

/// The waiting line: count, focus-tracking state, then as many entries as fit.
/// Never exceeds `max_width`; when not even the oldest entry fits whole it is
/// truncated so something useful still renders.
fn waiting_title(app: &App, max_width: usize) -> Line<'static> {
    const OPEN: &str = "◀ h  ";
    const CLOSE: &str = "  l ▶ ";
    let waiting = app.waiting_rows();
    let mut spans = vec![Span::styled(
        format!(" WAITING {} ", waiting.len()),
        Style::default()
            .fg(if waiting.is_empty() { MUTED } else { AMBER })
            .add_modifier(Modifier::BOLD),
    )];
    if !app.focus_tracking() {
        spans.push(Span::styled(
            "(focus tracking off) ",
            Style::default().fg(MUTED),
        ));
    }
    let used = Line::from(spans.clone()).width();
    let budget =
        max_width.saturating_sub(used + Line::raw(OPEN).width() + Line::raw(CLOSE).width());
    let more = |hidden: usize| format!(" +{hidden} more");
    let mut entries: Vec<Span<'static>> = Vec::new();
    let mut entries_width = 0;
    let mut shown = 0;
    for (index, row) in waiting.iter().enumerate() {
        let text = format!(
            "{}{}",
            if index == 0 { "" } else { " · " },
            waiting_entry(app, row)
        );
        let hidden_after = waiting.len() - index - 1;
        let reserve = if hidden_after == 0 {
            0
        } else {
            Line::raw(more(hidden_after)).width()
        };
        let width = Line::raw(text.as_str()).width();
        if entries_width + width + reserve > budget {
            break;
        }
        let selected = app.selected.as_ref() == Some(&row.incarnation);
        entries.push(Span::styled(
            text,
            if selected {
                Style::default()
                    .fg(INK)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
            } else {
                Style::default().fg(INK)
            },
        ));
        entries_width += width;
        shown += 1;
    }
    let mut trailing = None;
    if shown == 0 {
        let Some(oldest) = waiting.first() else {
            return Line::from(spans);
        };
        if budget == 0 {
            return Line::from(spans);
        }
        let text = fit(&waiting_entry(app, oldest), budget).into_owned();
        let width = Line::raw(text.as_str()).width();
        entries.push(Span::styled(text, Style::default().fg(INK)));
        let hidden = more(waiting.len() - 1);
        // The count is dropped rather than overflowing the border.
        if waiting.len() > 1 && width + Line::raw(hidden.as_str()).width() <= budget {
            trailing = Some(hidden);
        }
    } else if shown < waiting.len() {
        trailing = Some(more(waiting.len() - shown));
    }
    spans.push(Span::styled(OPEN, Style::default().fg(MUTED)));
    spans.extend(entries);
    if let Some(hidden) = trailing {
        spans.push(Span::styled(hidden, Style::default().fg(MUTED)));
    }
    spans.push(Span::styled(CLOSE, Style::default().fg(MUTED)));
    Line::from(spans)
}

fn render_detail<'a>(frame: &mut Frame<'_>, area: Rect, lines: Option<Vec<Line<'a>>>) {
    let Some(lines) = lines else {
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

    frame.render_widget(
        detail_paragraph(lines).block(
            board_block()
                .title(" SELECTED SIGNAL ")
                .borders(Borders::ALL),
        ),
        area,
    );
}

fn detail_paragraph<'a>(lines: Vec<Line<'a>>) -> Paragraph<'a> {
    Paragraph::new(lines).wrap(Wrap { trim: true })
}

fn detail_lines<'a>(agent: &'a crate::app::AgentRow, now_ms: i64) -> Vec<Line<'a>> {
    vec![
        Line::styled(
            format!(
                "{} {}  {}",
                state_glyph(agent.state),
                agent.state.label(),
                agent.provider_display_name
            ),
            state_style(agent.state).add_modifier(Modifier::BOLD),
        ),
        Line::raw(""),
        fact("STATE", agent.state.label()),
        fact("TARGET", &agent.tmux_target),
        fact("PANE ID", &agent.incarnation.pane_id),
        fact("DIRECTORY", &agent.working_directory),
        fact("AGE", age(now_ms, agent.last_transition_at_ms)),
        Line::raw(""),
        Line::styled(
            "CURRENT WORK",
            Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
        ),
        Line::styled(
            display_work_summary(agent.work_summary.as_deref()),
            Style::default().fg(INK),
        ),
    ]
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, compact: bool) {
    let spans = if compact {
        vec![
            Span::styled(" j/k", Style::default().fg(INK)),
            Span::raw(" move "),
            Span::styled("h/l", Style::default().fg(INK)),
            Span::raw(" wait "),
            Span::styled("f", Style::default().fg(INK)),
            Span::raw(" search "),
            Span::styled("/", Style::default().fg(INK)),
            Span::raw(" commands "),
            Span::styled("Enter", Style::default().fg(INK)),
            Span::raw(" go "),
            Span::styled("q", Style::default().fg(INK)),
            Span::raw(" quit "),
        ]
    } else {
        vec![
            Span::styled(" DISPATCH  ", Style::default().fg(MUTED)),
            Span::styled("j/k", Style::default().fg(INK)),
            Span::raw(" select  "),
            Span::styled("h/l", Style::default().fg(INK)),
            Span::raw(" waiting  "),
            Span::styled("f", Style::default().fg(INK)),
            Span::raw(" search  "),
            Span::styled("/", Style::default().fg(INK)),
            Span::raw(" commands  "),
            Span::styled("Enter", Style::default().fg(INK)),
            Span::raw(" switch pane  "),
            Span::styled("Esc", Style::default().fg(INK)),
            Span::raw(" clear  "),
            Span::styled("q", Style::default().fg(INK)),
            Span::raw(" quit "),
        ]
    };
    frame.render_widget(
        Paragraph::new(Line::from(spans)).block(board_block().borders(Borders::ALL)),
        area,
    );
}

fn render_palette(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(palette) = &app.palette else {
        return;
    };
    let entries = app.palette_entries();
    let list_rows = u16::try_from(entries.len().clamp(1, 6)).unwrap_or(6);
    let width = area.width.saturating_sub(4).min(60);
    let height = (list_rows + 4).min(area.height);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 3,
        width,
        height,
    };
    let mut lines = vec![
        Line::from(vec![
            Span::styled("> ", Style::default().fg(AMBER)),
            Span::styled(
                palette.query.clone(),
                Style::default().fg(INK).add_modifier(Modifier::BOLD),
            ),
            Span::styled("█", Style::default().fg(AMBER)),
        ]),
        Line::from(""),
    ];
    if entries.is_empty() {
        let message = if app.connection == crate::app::ConnectionState::Live {
            "No matching command"
        } else {
            "Commands unavailable until connected"
        };
        lines.push(Line::from(Span::styled(
            message,
            Style::default().fg(MUTED),
        )));
    } else {
        for (index, entry) in entries.iter().enumerate() {
            let selected = index == palette.selected;
            lines.push(Line::from(Span::styled(
                format!("{} {}", if selected { "▸" } else { " " }, entry.label),
                if selected {
                    Style::default().fg(INK).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(MUTED)
                },
            )));
        }
    }
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(
            board_block()
                .borders(Borders::ALL)
                .title(" Command ")
                .title_bottom(" Enter run · Esc close "),
        ),
        popup,
    );
}

fn board_block<'a>() -> Block<'a> {
    Block::default()
        .style(Style::default().bg(PANEL).fg(INK))
        .border_style(Style::default().fg(Color::Rgb(90, 86, 72)))
}

fn fact<'a>(label: &str, value: impl Into<Cow<'a, str>>) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("{label:<10}"), Style::default().fg(MUTED)),
        Span::styled(value, Style::default().fg(INK)),
    ])
}

/// Truncates `text` to `width` terminal columns, marking the cut with an ellipsis.
fn fit(text: &str, width: usize) -> Cow<'_, str> {
    if Line::raw(text).width() <= width {
        return Cow::Borrowed(text);
    }
    let mut fitted = String::new();
    for character in text.chars() {
        fitted.push(character);
        if Line::raw(fitted.as_str()).width() + 1 > width {
            fitted.pop();
            break;
        }
    }
    fitted.push('…');
    Cow::Owned(fitted)
}

fn state_glyph(state: AgentState) -> &'static str {
    match state {
        AgentState::Busy => "●",
        AgentState::Idle => "●",
        AgentState::Unknown => "○",
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
    use std::borrow::Cow;

    use crossterm::event::KeyCode;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::style::Color;
    use ratatui::text::Line;

    use super::{fit, render};
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
                tts_muted: false,
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
            "f event",
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
    fn table_and_selected_detail_keep_state_words_with_semantic_glyph_colors() {
        for (state, expected, color) in [
            (AgentState::Idle, "● IDLE", Color::Rgb(125, 200, 139)),
            (AgentState::Busy, "● BUSY", Color::Rgb(224, 174, 85)),
            (AgentState::Unknown, "○ UNKNOWN", Color::Rgb(143, 138, 127)),
        ] {
            let selected = incarnation("%17", 91700, 91844, "codex");
            let app = live_app(
                vec![row(
                    selected.clone(),
                    state,
                    "Codex",
                    "tmx-agent-dash:0.1",
                    "Keep status accessible without colour",
                    90_000,
                )],
                Some(selected),
            );

            let buffer = rendered_buffer(&app, 140, 38, 100_000);

            assert_styled_occurrences(&buffer, expected, color, 2);
        }
    }

    #[test]
    fn footer_names_escape_clear_and_q_as_the_only_quit_key() {
        let content = rendered(&live_app(Vec::new(), None), 140, 38, 100_000);

        assert!(content.contains("Esc clear"));
        assert!(content.contains("q quit"));
        assert!(!content.contains("Esc quit"));
    }

    #[test]
    fn wide_dashboard_does_not_present_unknown_monitor_health_as_healthy() {
        let app = App::new(
            ConnectionState::Live,
            Snapshot {
                tts_muted: false,
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
                tts_muted: false,
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
            "WHERE",
            "WORK SUMMARY",
            "Codex",
            "cx",
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
            "WHERE",
            "WORK SUMMARY",
            "BUSY",
            "BUSY 01",
            "IDLE 00",
            "Codex",
            "cx",
            "Responsive",
        ] {
            assert!(content.contains(expected), "missing {expected:?}");
        }
        assert!(!content.contains("CURRENT WORK"));
    }

    #[test]
    fn compact_selected_unknown_keeps_hollow_glyph_and_complete_word() {
        let selected = incarnation("%17", 91700, 91844, "codex");
        let app = live_app(
            vec![row(
                selected.clone(),
                AgentState::Unknown,
                "Codex",
                "agents:2.7",
                "Awaiting classification",
                90_000,
            )],
            Some(selected),
        );

        let buffer = rendered_buffer(&app, 72, 22, 100_000);
        let content = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>()
            .join("");

        assert!(content.contains("▶ ○ UNKNOWN"));
        assert_styled_occurrences(&buffer, "○ UNKNOWN", Color::Rgb(143, 138, 127), 1);
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
        let mut app = App::new(
            ConnectionState::Stale,
            Snapshot {
                tts_muted: false,
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
        app.record_snapshot_received_at(95_000);

        let content = rendered(&app, 104, 28, 100_000);

        assert!(content.contains("TRANSPORT STALE"));
        assert!(content.contains("Last committed snapshot 5s ago"));
        assert!(content.contains("Retained work remains readable"));
    }

    #[test]
    fn runtime_status_exposes_retry_and_navigation_failures_without_hiding_rows() {
        let selected = incarnation("%17", 91700, 91844, "codex");
        let mut app = live_app(
            vec![row(
                selected.clone(),
                AgentState::Busy,
                "Codex",
                "agents:2.7",
                "Retained work",
                90_000,
            )],
            Some(selected),
        );

        app.set_runtime_status(crate::app::RuntimeStatus::Retrying {
            endpoint: "http://127.0.0.1:50060".into(),
            detail: "Harold unavailable".into(),
            delay_ms: 500,
        });
        let retrying = rendered(&app, 140, 38, 100_000);
        assert!(retrying.contains("RETRY IN 500ms"));
        assert!(retrying.contains("Harold unavailable"));
        assert!(retrying.contains("Retained work"));

        app.set_runtime_status(crate::app::RuntimeStatus::NavigationUnavailable);
        assert!(rendered(&app, 140, 38, 100_000).contains("NAVIGATION UNAVAILABLE"));

        app.set_runtime_status(crate::app::RuntimeStatus::NavigationFailed(
            "pane disappeared".into(),
        ));
        assert!(rendered(&app, 140, 38, 100_000).contains("NAVIGATION FAILED: pane disappeared"));
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
            tts_muted: false,
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
                tts_muted: false,
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
        assert!(before.contains("▶ ● BUSY"));
        assert!(!before.contains("▶ ● IDLE"));

        app.handle_key(KeyCode::Char('j'));
        let after = rendered(&app, 104, 28, 100_000);
        assert!(!after.contains("▶ ● BUSY"));
        assert!(after.contains("▶ ● IDLE"));
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

        assert!(content.contains("▶ ● IDLE"));
        assert!(content.contains("cx"));
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
    fn masthead_shows_voice_on_muted_and_unknown_in_wide_and_compact() {
        let mut on = live_app(Vec::new(), None);
        assert!(rendered(&on, 140, 38, 100_000).contains("● VOICE ON"));
        assert!(rendered(&on, 70, 30, 100_000).contains("● VOICE ON"));

        on.snapshot.tts_muted = true;
        assert!(rendered(&on, 140, 38, 100_000).contains("○ VOICE MUTED"));
        assert!(rendered(&on, 70, 30, 100_000).contains("○ VOICE MUTED"));

        let unknown = App::new(
            crate::app::ConnectionState::Connecting,
            crate::app::Snapshot {
                through_event_version: 0,
                server_time_ms: 0,
                monitor_health: Vec::new(),
                rows: Vec::new(),
                tts_muted: false,
            },
            empty_search(),
            None,
        );
        assert!(rendered(&unknown, 140, 38, 100_000).contains("○ VOICE —"));
        assert!(rendered(&unknown, 60, 18, 100_000).contains("○ VOICE —"));
    }

    #[test]
    fn voice_indicator_colour_and_glyph_are_paired_with_the_word() {
        let mut app = live_app(Vec::new(), None);
        app.snapshot.tts_muted = true;
        let buffer = rendered_buffer(&app, 140, 38, 100_000);
        assert_styled_occurrences(&buffer, "○ VOICE MUTED", Color::Rgb(224, 174, 85), 1);
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
        assert!(content.contains("▶ ● BUSY"));
        assert!(content.contains("cx"));
        assert!(!content.contains("tmx-agent-dash:2.17"));
        // The waiting-marker column takes one more cell of width (plus its
        // spacing) from the work column, so the last wide glyph is clipped.
        assert_eq!(content.matches('界').count(), 42);
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

    #[test]
    fn detail_lines_borrow_agent_text_instead_of_cloning_it() {
        let selected = incarnation("%17", 91700, 91844, "codex");
        let agent = row(
            selected,
            AgentState::Busy,
            "Codex",
            "tmx-agent-dash:2.17",
            "Keep detail text borrowed across measurement and rendering",
            90_000,
        );

        let lines = super::detail_lines(&agent, 100_000);

        for (line_index, span_index) in [(3, 1), (4, 1), (5, 1), (9, 0)] {
            assert!(
                matches!(
                    lines[line_index].spans[span_index].content,
                    Cow::Borrowed(_)
                ),
                "detail line {line_index} span {span_index} must borrow agent text"
            );
        }
    }

    fn rendered(app: &App, width: u16, height: u16, now_ms: i64) -> String {
        rendered_buffer(app, width, height, now_ms)
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>()
            .join("")
    }

    /// Like `rendered`, but with a newline after every terminal row.
    fn rendered_lines(app: &App, width: u16, height: u16, now_ms: i64) -> String {
        let buffer = rendered_buffer(app, width, height, now_ms);
        buffer
            .content()
            .chunks(usize::from(buffer.area.width))
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn rendered_buffer(app: &App, width: u16, height: u16, now_ms: i64) -> Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, app, now_ms)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn assert_styled_occurrences(
        buffer: &Buffer,
        expected: &str,
        color: Color,
        expected_count: usize,
    ) {
        let symbols = expected
            .chars()
            .map(|character| character.to_string())
            .collect::<Vec<_>>();
        let matches = buffer
            .content()
            .windows(symbols.len())
            .filter(|cells| {
                cells
                    .iter()
                    .zip(&symbols)
                    .all(|(cell, symbol)| cell.symbol() == symbol)
            })
            .collect::<Vec<_>>();

        assert_eq!(
            matches.len(),
            expected_count,
            "expected {expected_count} rendered {expected:?} labels"
        );
        for cells in matches {
            for (cell, symbol) in cells.iter().zip(&symbols) {
                if symbol != " " {
                    assert_eq!(
                        cell.style().fg,
                        Some(color),
                        "{expected:?} symbol {symbol:?} used the wrong foreground"
                    );
                }
            }
        }
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
            tts_muted: false,
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

    fn located(
        pane_id: &str,
        state: AgentState,
        provider: &str,
        session: &str,
        directory: &str,
        summary: &str,
    ) -> AgentRow {
        let mut located = row(
            incarnation(pane_id, 10, 20, &provider.to_lowercase()),
            state,
            provider,
            &format!("{session}:0.3"),
            summary,
            90_000,
        );
        located.session_name = session.into();
        located.working_directory = directory.into();
        located
    }

    fn user_layout() -> App {
        let mut app = live_app(Vec::new(), None);
        app.apply_later_snapshot(Snapshot {
            through_event_version: 43,
            ..snapshot(
                vec![health(MonitorHealthState::Healthy, "ok")],
                vec![
                    located(
                        "%1",
                        AgentState::Busy,
                        "Claude",
                        "harold  main",
                        "/p/harold/main",
                        "fixing handler",
                    ),
                    located(
                        "%2",
                        AgentState::Idle,
                        "Claude",
                        "harold  voice-mute-palette1",
                        "/p/harold/voice-mute-palette",
                        "writing plan",
                    ),
                    located(
                        "%3",
                        AgentState::Idle,
                        "Codex",
                        "home",
                        "/Users/k/Dev/p/sre",
                        "waiting",
                    ),
                ],
            )
        })
        .unwrap();
        app
    }

    #[test]
    fn list_groups_agents_under_project_headers_with_names_and_tags() {
        for (width, height) in [(140, 38), (100, 30), (70, 30), (60, 20)] {
            let content = rendered(&user_layout(), width, height, 100_000);
            assert!(content.contains("WHERE"), "{width}x{height}");
            assert!(
                !content.contains("TARGET") || width >= 120,
                "{width}x{height}"
            );
            let harold = content.find("harold").expect("harold header");
            let main = content.find("main").expect("main row");
            let palette = content
                .find("voice-mute-palette")
                .expect("full worktree name");
            let home = content.find("home").expect("home header");
            let sre = content.find("sre").expect("sre row");
            assert!(harold < main && main < palette && palette < home && home < sre);
            assert!(content.contains("cc"));
            assert!(content.contains("cx"));
            assert!(
                !content.contains("voice-mute-palette1"),
                "session suffix must not show"
            );
        }
    }

    #[test]
    fn group_header_is_not_selectable_and_selection_marker_tracks_the_agent_row() {
        let mut app = user_layout();
        app.handle_key(crossterm::event::KeyCode::Char('g'));
        assert_eq!(app.selected.as_ref().unwrap().pane_id, "%1");
        let buffer = rendered_buffer(&app, 140, 38, 100_000);
        let marker_line = buffer
            .content()
            .chunks(usize::from(buffer.area.width))
            .map(|cells| cells.iter().map(|cell| cell.symbol()).collect::<String>())
            .find(|line| line.contains('▶'))
            .expect("selection marker");
        assert!(marker_line.contains("main"));
        assert!(!marker_line.contains("harold"));
    }

    #[test]
    fn long_names_are_truncated_at_the_end_with_an_ellipsis() {
        assert_eq!(fit("voice-mute-palette", 18), "voice-mute-palette");
        assert_eq!(fit("voice-mute-palette", 10), "voice-mut…");
        assert_eq!(fit("界界界界", 5), "界界…");
        assert_eq!(fit("abc", 0), "…");
    }

    #[test]
    fn selected_row_in_a_late_group_scrolls_into_view() {
        let rows = (0..30)
            .map(|index| {
                located(
                    &format!("%{index}"),
                    AgentState::Idle,
                    "Claude",
                    &format!("proj{index:02}  w"),
                    &format!("/p/proj{index:02}/w{index:02}"),
                    "work",
                )
            })
            .collect();
        let mut app = live_app(Vec::new(), None);
        app.apply_later_snapshot(Snapshot {
            through_event_version: 43,
            ..snapshot(vec![health(MonitorHealthState::Healthy, "ok")], rows)
        })
        .unwrap();
        app.handle_key(crossterm::event::KeyCode::Char('G'));
        let content = rendered(&app, 100, 24, 100_000);
        assert!(content.contains("w29"));
        assert!(content.contains("proj29"));
    }

    #[test]
    fn palette_overlay_lists_filtered_entries_and_empty_state() {
        let mut app = live_app(Vec::new(), None);
        app.handle_key(crossterm::event::KeyCode::Char('/'));
        for character in "voi".chars() {
            app.handle_key(crossterm::event::KeyCode::Char(character));
        }
        let content = rendered(&app, 140, 38, 100_000);
        assert!(content.contains("Command"));
        assert!(content.contains("> voi"));
        assert!(content.contains("Voice: mute"));
        assert!(!content.contains("Voice: unmute"));

        app.handle_key(crossterm::event::KeyCode::Char('z'));
        assert!(rendered(&app, 140, 38, 100_000).contains("No matching command"));
    }

    #[test]
    fn palette_explains_when_commands_are_unavailable() {
        let mut app = App::new(
            crate::app::ConnectionState::Connecting,
            crate::app::Snapshot {
                through_event_version: 0,
                server_time_ms: 0,
                monitor_health: Vec::new(),
                rows: Vec::new(),
                tts_muted: false,
            },
            empty_search(),
            None,
        );
        app.handle_key(crossterm::event::KeyCode::Char('/'));
        assert!(rendered(&app, 140, 38, 100_000).contains("Commands unavailable until connected"));
    }

    #[test]
    fn footer_advertises_f_search_and_slash_commands() {
        let content = rendered(&live_app(Vec::new(), None), 140, 38, 100_000);
        assert!(content.contains("f search"));
        assert!(content.contains("/ commands"));
        let compact = rendered(&live_app(Vec::new(), None), 70, 30, 100_000);
        assert!(compact.contains("f search"));
        assert!(compact.contains("/ commands"));
    }

    #[test]
    fn footer_keeps_q_quit_visible_at_supported_widths() {
        for (w, h) in [(60, 18), (70, 30), (84, 38), (98, 38), (140, 38)] {
            let content = rendered(&live_app(Vec::new(), None), w, h, 100_000);
            assert!(content.contains("q quit"), "q quit missing at {w}x{h}");
        }
    }

    #[test]
    fn muted_masthead_keeps_voice_and_transport_at_minimum_size() {
        let mut app = live_app(Vec::new(), None);
        app.snapshot.tts_muted = true;
        let content = rendered(&app, 60, 18, 100_000);
        assert!(content.contains("○ VOICE MUTED"));
        assert!(content.contains("TRANSPORT"));
    }

    #[test]
    fn palette_fits_the_minimum_terminal_size() {
        let mut app = live_app(Vec::new(), None);
        app.handle_key(crossterm::event::KeyCode::Char('/'));
        let content = rendered(&app, 60, 18, 100_000);
        assert!(content.contains("Voice: mute"));
    }

    fn layout_with_states(states: [AgentState; 3], revision: u64) -> crate::app::Snapshot {
        crate::app::Snapshot {
            through_event_version: revision,
            ..snapshot(
                vec![health(MonitorHealthState::Healthy, "ok")],
                vec![
                    located(
                        "%1",
                        states[0],
                        "Claude",
                        "harold  main",
                        "/p/harold/main",
                        "a",
                    ),
                    located(
                        "%2",
                        states[1],
                        "Claude",
                        "harold  voice-mute-palette1",
                        "/p/harold/voice-mute-palette",
                        "b",
                    ),
                    located("%3", states[2], "Codex", "home", "/Users/k/Dev/p/sre", "c"),
                ],
            )
        }
    }

    fn waiting_app() -> App {
        use AgentState::{Busy, Idle};
        let mut app = live_app(Vec::new(), None);
        app.observe_focus(crate::app::FocusReading::Active("%99".into()));
        app.apply_later_snapshot(layout_with_states([Busy, Busy, Busy], 43))
            .unwrap();
        app.apply_later_snapshot(layout_with_states([Busy, Busy, Idle], 44))
            .unwrap();
        app.apply_later_snapshot(layout_with_states([Busy, Idle, Idle], 45))
            .unwrap();
        app
    }

    #[test]
    fn waiting_line_shows_count_entries_oldest_first_and_keys() {
        let content = rendered(&waiting_app(), 140, 38, 100_000);
        assert!(content.contains("WAITING 2"));
        assert!(content.contains("◀ h"));
        assert!(content.contains("l ▶"));
        let sre = content.find("home/sre").expect("oldest entry");
        let palette = content
            .find("harold/voice-mute-palette")
            .expect("newest entry");
        assert!(sre < palette);
        assert!(content.contains("AGENT BLOCK OCCUPANCY"));
        assert!(!content.contains("focus tracking off"));
    }

    #[test]
    fn waiting_line_is_quiet_when_empty_and_flags_missing_focus_tracking() {
        let mut app = user_layout();
        let content = rendered(&app, 140, 38, 100_000);
        assert!(content.contains("WAITING 0"));
        assert!(!content.contains("◀ h"));
        assert!(content.contains("(focus tracking off)"));

        app.observe_focus(crate::app::FocusReading::Active("%99".into()));
        assert!(!rendered(&app, 140, 38, 100_000).contains("focus tracking off"));
    }

    #[test]
    fn waiting_rows_carry_a_marker_and_others_do_not() {
        let content = rendered_lines(&waiting_app(), 140, 38, 100_000);
        let line_with = |needle: &str| {
            content
                .lines()
                .find(|line| line.contains(needle) && line.contains("IDLE"))
                .map(str::to_owned)
        };
        assert!(line_with("sre").unwrap().contains('◆'));
        let busy = content
            .lines()
            .find(|line| line.contains("BUSY") && line.contains("main"))
            .unwrap();
        assert!(!busy.contains('◆'));
    }

    #[test]
    fn waiting_line_overflow_reports_how_many_are_hidden_and_never_wraps() {
        let content = rendered_lines(&waiting_app(), 60, 18, 100_000);
        assert!(content.contains("WAITING 2"));
        assert!(content.contains("more") || content.contains("home/sre"));
        assert_eq!(content.lines().count(), 18);
    }

    #[test]
    fn group_equal_to_name_is_not_repeated_in_a_waiting_entry() {
        use AgentState::{Busy, Idle};
        let single = |state, revision| crate::app::Snapshot {
            through_event_version: revision,
            ..snapshot(
                vec![health(MonitorHealthState::Healthy, "ok")],
                vec![located(
                    "%1",
                    state,
                    "Codex",
                    "kahgeh-com",
                    "/p/kahgeh-com",
                    "a",
                )],
            )
        };
        let mut app = live_app(Vec::new(), None);
        app.apply_later_snapshot(single(Busy, 43)).unwrap();
        app.apply_later_snapshot(single(Idle, 44)).unwrap();
        let content = rendered(&app, 140, 38, 100_000);
        assert!(content.contains("WAITING 1"));
        assert!(!content.contains("kahgeh-com/kahgeh-com"));
    }

    #[test]
    fn footer_lists_every_hint_untruncated_at_all_widths() {
        for (width, height) in [(140, 38), (84, 30), (70, 30), (60, 18)] {
            let content = rendered(&user_layout(), width, height, 100_000);
            for hint in ["j/k", "h/l", "f search", "/ commands", "Enter", "q quit"] {
                assert!(content.contains(hint), "{hint} missing at {width}x{height}");
            }
        }
    }
}
