use std::borrow::Cow;

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Cell, Clear, Paragraph, Row, StatefulWidget, Table, TableState, Wrap,
};

use crate::api::HAROLD_VERSION_LIMIT;
use crate::app::{AgentState, App, CallState, CallView, RuntimeStatus, VoiceState};
use crate::questions::{
    ANSWER_ACTIONS, INSTRUCTION_ACTIONS, Mode, NoteKind, QStop, RowsView, TodoKind,
};
use crate::text::{display_work_summary, sanitize_display};

const BOARD: Color = Color::Rgb(20, 20, 16);
const PANEL: Color = Color::Rgb(25, 25, 21);
const INK: Color = Color::Rgb(237, 231, 216);
const MUTED: Color = Color::Rgb(143, 138, 127);
const AMBER: Color = Color::Rgb(224, 174, 85);
const GREEN: Color = Color::Rgb(125, 200, 139);
const CORAL: Color = Color::Rgb(224, 114, 98);
/// The face of a key-cap button: the call button with no call, and talk now.
const KEY_CAP: Color = Color::Rgb(58, 58, 48);
const MIN_WIDTH: u16 = 60;
const MIN_HEIGHT: u16 = 18;
const WIDE_WIDTH: u16 = 120;
/// Narrowest terminal that fits the list's long-form footer with every hint visible.
const WIDE_FOOTER_WIDTH: u16 = 127;
const COMPACT_WIDTH: u16 = 84;
/// The speaking bars' heights, one step apart; the five bars read it two steps apart.
const LEVELS: [char; 12] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '▆', '▅', '▄', '▃', '▂'];
const BAR_STEP_MS: i64 = 150;
const COLUMN_SPACING: u16 = 1;
/// The work summary's place among the list's columns, in both forms.
const WORK_COLUMN: usize = 4;
/// The rows the agent list keeps.
const WORKSPACE_MIN: u16 = 6;

pub fn render(frame: &mut Frame<'_>, app: &App, now_ms: i64) {
    let area = frame.area();
    frame.render_widget(Block::default().style(Style::default().bg(BOARD)), area);

    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        render_resize_instruction(frame, area);
        return;
    }

    let compact = area.width < COMPACT_WIDTH || area.height < 24;
    let chrome_height = if compact { 2 } else { 3 };
    let fixed = 3 + 3 * chrome_height + WORKSPACE_MIN + 3;
    // The Now line takes a row only when one is spare beside the list's six rows, so at
    // the smallest height it hides rather than squeeze the list.
    let now_height = u16::from(app.now_line.is_some() && area.height > fixed);
    let [masthead, now, monitor, summary, search, workspace, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(now_height),
        Constraint::Length(chrome_height),
        Constraint::Length(chrome_height),
        Constraint::Length(chrome_height),
        Constraint::Min(WORKSPACE_MIN),
        Constraint::Length(3),
    ])
    .areas(area);

    render_masthead(frame, masthead, app);
    if let Some(line) = app.now_line.as_deref() {
        render_now(frame, now, line);
    }
    render_monitor(frame, monitor, app, now_ms);
    render_summary(frame, summary, app);
    render_search(frame, search, app);
    render_workspace(frame, workspace, app, now_ms);
    if app.swallowing_keys() {
        render_swallow_footer(frame, footer);
    } else if app.holding_block_keys() {
        render_hold_footer(frame, footer);
    } else {
        render_footer(
            frame,
            footer,
            compact || area.width < WIDE_FOOTER_WIDTH,
            app.question_stop,
            app.questions.is_open(),
            note_kind(&app.questions),
            TodoFacts {
                link: app.questions.has_web_link(),
                instructions: app.questions.on_instructions(),
            },
        );
    }
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
    let transport_style = transport_style(app.connection).add_modifier(Modifier::BOLD);
    let transport = Span::styled(
        format!("TRANSPORT {}", app.connection.label()),
        transport_style,
    );
    let revision = Span::styled(
        format!("  ·  REV #{:05}", app.snapshot.through_event_version),
        transport_style,
    );
    let mut tail = vec![
        Span::raw("  ·  "),
        Span::styled(voice.label(), voice_style(voice)),
    ];
    if app.messaging_paused() {
        tail.push(Span::raw("  ·  "));
        tail.push(Span::styled(
            "■ MSG PAUSED",
            Style::default().fg(CORAL).add_modifier(Modifier::BOLD),
        ));
    }
    let amber = Style::default().fg(AMBER);
    let version_style = Style::default().fg(INK).add_modifier(Modifier::BOLD);
    let dash_version = sanitize_display(&app.dash_version, HAROLD_VERSION_LIMIT);
    let version_span = Span::styled(dash_version, version_style);
    let compact = area.width < COMPACT_WIDTH;
    let mut candidates: Vec<(Vec<Span<'static>>, bool)> = Vec::new();
    if !compact {
        candidates.push((
            vec![
                Span::styled(" HAROLD / TMUX  ", amber),
                version_span.clone(),
                Span::raw("  ·  "),
            ],
            true,
        ));
    }
    candidates.push((
        vec![
            Span::styled(" TMX DASH ", amber),
            version_span.clone(),
            Span::raw(" · "),
        ],
        true,
    ));
    let lead = vec![Span::raw(" "), version_span, Span::raw("  ·  ")];
    candidates.push((lead.clone(), true));
    candidates.push((lead, false));
    candidates.push((vec![Span::raw(" ")], false));
    let line = |lead: Vec<Span<'static>>, with_revision: bool| {
        Line::from(
            lead.into_iter()
                .chain(std::iter::once(transport.clone()))
                .chain(with_revision.then(|| revision.clone()))
                .chain(tail.iter().cloned())
                .collect::<Vec<_>>(),
        )
    };
    let available = usize::from(area.width.saturating_sub(2));
    // The header shapes are tried in this order and the first that fits wins: `HAROLD / TMUX`
    // and the dash's version, `TMX DASH` and the version, the version alone, then without
    // `REV`, then no version at all. Transport, voice and the paused marker always remain.
    let mut lines = candidates
        .into_iter()
        .map(|(lead, with_revision)| line(lead, with_revision))
        .collect::<Vec<_>>();
    let last = lines.len() - 1;
    let chosen = lines
        .iter()
        .position(|candidate| candidate.width() <= available)
        .unwrap_or(last);
    let title = lines.swap_remove(chosen);
    frame.render_widget(
        Paragraph::new(title).block(board_block().borders(Borders::ALL)),
        area,
    );
}

/// The chief's Now item, on one dim row under the masthead, in line with the text
/// inside the boxes. It is cut to the width and never takes the focus.
fn render_now(frame: &mut Frame<'_>, area: Rect, line: &str) {
    let inner = area.inner(Margin::new(1, 0));
    let text = fit(&format!("NOW  {line}"), usize::from(inner.width)).into_owned();
    frame.render_widget(
        Paragraph::new(Line::styled(text, Style::default().fg(MUTED))),
        inner,
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
            .map(|received_at_ms| format!("{} ago", seconds_ago(now_ms, received_at_ms)))
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
            RuntimeStatus::MessagingFailed(detail) => format!("MESSAGING ERROR: {detail}"),
            RuntimeStatus::CallFailed(detail) => format!("CALL ERROR: {detail}"),
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
        crate::app::ConnectionState::Unavailable => render_state_message(
            frame,
            area,
            "Harold is unavailable",
            "Press Ctrl+L to retry now",
        ),
        crate::app::ConnectionState::Live | crate::app::ConnectionState::Stale => {
            render_live_workspace(frame, area, app, now_ms);
        }
    }
}

fn render_live_workspace(frame: &mut Frame<'_>, area: Rect, app: &App, now_ms: i64) {
    let visible = app.visible_rows();
    if visible.is_empty() && app.questions.view().is_none() {
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
    let widths = inventory_widths(wide);
    let work_width = work_cell_width(area, &widths);
    let chief = app.chief_row().map(|row| &row.incarnation);
    let groups = app.visible_groups();
    // The rows of an open question follow the chief's row, or lead the list when the
    // chief's row is not in it.
    let chief_shown = groups
        .iter()
        .any(|(_, agents)| agents.iter().any(|a| chief == Some(&a.incarnation)));
    let text_width = usize::from(area.width.saturating_sub(2));
    let question_lines = app
        .questions
        .view()
        .map(|view| question_row_lines(&view, text_width, !chief_shown, app.question_stop))
        .unwrap_or_default();
    let mut rows = Vec::new();
    // Each row's height with its margin, for finding where a question row landed.
    let mut heights = Vec::new();
    // The table row of the question's first line; its lines follow one to a row.
    let mut question_at = None;
    let mut selected_index = None;
    if !chief_shown {
        push_question_rows(&question_lines, &mut rows, &mut heights, &mut question_at);
    }
    for (group, agents) in groups {
        let mut header = Row::new(vec![
            Cell::from(""),
            Cell::from(Line::styled(
                fit(group, usize::from(where_width)).into_owned(),
                Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
            )),
        ]);
        let margin = wide && !rows.is_empty();
        if margin {
            header = header.top_margin(1);
        }
        rows.push(header);
        heights.push(1 + u16::from(margin));
        for agent in agents {
            // The cursor is on the chief's row only when it is not on a question row.
            let selected = app.selected.as_ref() == Some(&agent.incarnation);
            let on_cursor = selected && app.question_stop.is_none();
            if on_cursor {
                selected_index = Some(rows.len());
            }
            let marker = if on_cursor { "▶ " } else { "  " };
            let style = if on_cursor {
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
                    fit_with_suffix("", app.label_parts(agent), usize::from(where_width) - 2)
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
            ];
            let summary = display_work_summary(agent.work_summary.as_deref());
            cells.push(match app.call.as_ref() {
                Some(view) if chief == Some(&agent.incarnation) => {
                    let work = call_work(view);
                    Cell::from(chief_work_line(
                        work.as_deref().unwrap_or(summary),
                        view,
                        app.voice() == VoiceState::Muted,
                        now_ms,
                        work_width,
                    ))
                }
                _ => Cell::from(summary),
            });
            if wide {
                cells.push(Cell::from(age(now_ms, agent.last_transition_at_ms)));
            }
            rows.push(Row::new(cells).style(style));
            heights.push(1);
            if chief == Some(&agent.incarnation) {
                push_question_rows(&question_lines, &mut rows, &mut heights, &mut question_at);
            }
        }
    }
    if let Some(at) = question_at
        && let Some(cursor) = question_lines
            .iter()
            .position(|l| l.stop.is_some() && l.stop == app.question_stop)
    {
        selected_index = Some(at + cursor);
    }
    let header = if wide {
        Row::new(["STATE", "WHERE", "", "", "WORK SUMMARY", "AGE"])
    } else {
        Row::new(["STATE", "WHERE", "", "", "WORK SUMMARY"])
    };
    let block = board_block()
        .title(" AGENT BLOCK OCCUPANCY ")
        .title_top(focus_tracking_title(app).right_aligned())
        .borders(Borders::ALL);
    let inner = block.inner(area);
    let table = Table::new(rows, widths)
        .header(header.style(Style::default().fg(MUTED).add_modifier(Modifier::BOLD)))
        .column_spacing(COLUMN_SPACING)
        .block(block);
    let mut state = TableState::default().with_selected(selected_index);
    let mut list = Rect {
        y: inner.y.saturating_add(1),
        height: inner.height.saturating_sub(1),
        ..inner
    };
    let header_row = question_lines
        .iter()
        .position(|l| l.stop == Some(QStop::Header))
        .zip(question_at)
        .map(|(k, first)| first + k);
    // A `▼` takes the list's last row. Find out on a scratch screen whether the header
    // is below the view, and if so give the table a blank footer row for the `▼`, so
    // the table scrolls the cursor row clear of it.
    let mut dry_state = state;
    table
        .clone()
        .render(area, &mut Buffer::empty(area), &mut dry_state);
    let below = header_row.is_some_and(|row| {
        row >= dry_state.offset() && row_y(&heights, dry_state.offset(), row, list.height).is_none()
    });
    let table = if below {
        list.height = list.height.saturating_sub(1);
        table.footer(Row::new(Vec::<Cell>::new()))
    } else {
        table
    };
    frame.render_stateful_widget(table, area, &mut state);
    // The table drew the question rows blank; the text goes over them, as wide as
    // the list is.
    let Some(first) = question_at else {
        return;
    };
    for (k, line) in question_lines.iter().enumerate() {
        if let Some(y) = row_y(&heights, state.offset(), first + k, list.height) {
            let row = Rect {
                y: list.y + y,
                height: 1,
                ..list
            };
            frame.render_widget(
                Paragraph::new(Line::styled(line.text.as_str(), line.style)),
                row,
            );
        }
    }
    if let (Some(row), Some(view)) = (header_row, app.questions.view())
        && row_y(&heights, state.offset(), row, list.height).is_none()
    {
        render_question_edge(frame, inner, row < state.offset(), view.waiting);
    }
}

/// Appends a blank table row for each of the question's lines, the cursor row with its
/// background, and notes where the first one is.
fn push_question_rows<'a>(
    lines: &[QLine],
    rows: &mut Vec<Row<'a>>,
    heights: &mut Vec<u16>,
    first: &mut Option<usize>,
) {
    *first = Some(rows.len());
    for line in lines {
        rows.push(
            Row::new(Vec::<Cell>::new()).style(Style::default().bg(line.style.bg.unwrap_or(BOARD))),
        );
        heights.push(1);
    }
}

/// The y, from the top of the table's rows, of row `index` when the table shows
/// from row `offset`, or `None` when it is not within `room` rows.
fn row_y(heights: &[u16], offset: usize, index: usize, room: u16) -> Option<u16> {
    if index < offset {
        return None;
    }
    let y: u16 = heights[offset..index].iter().sum();
    (y < room).then_some(y)
}

/// The yellow `▲` on the list's top edge or `▼` on its bottom edge, with the count of
/// todos waiting, while the todo's header row is scrolled out of view.
fn render_question_edge(frame: &mut Frame<'_>, inner: Rect, above: bool, waiting: u32) {
    let (arrow, y) = if above {
        ('▲', inner.y)
    } else {
        ('▼', inner.bottom().saturating_sub(1))
    };
    let edge = Rect {
        y,
        height: 1,
        ..inner
    };
    frame.render_widget(Clear, edge);
    frame.render_widget(
        Paragraph::new(Line::styled(
            format!("{arrow} {}", waiting_label(waiting)),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(BOARD)),
        edge,
    );
}

/// One drawn row of an open or closed question.
#[derive(Debug, Clone, PartialEq, Eq)]
struct QLine {
    text: String,
    style: Style,
    /// The stop the row is, on its first row only; set for cursor stops, never for text.
    stop: Option<QStop>,
}

/// `1 todo waiting` or `N todos waiting`.
fn waiting_label(waiting: u32) -> String {
    match waiting {
        1 => "1 todo waiting".into(),
        n => format!("{n} todos waiting"),
    }
}

/// `1 todo` or `N todos`.
fn todos_count(n: u32) -> String {
    match n {
        1 => "1 todo".into(),
        n => format!("{n} todos"),
    }
}

/// The kind as the headers and rows name it.
fn kind_word(kind: TodoKind) -> &'static str {
    match kind {
        TodoKind::Question => "question",
        TodoKind::Instructions => "instructions",
    }
}

/// The columns before a question's text: the cursor marker, then the indent.
const QUESTION_HEADER_INDENT: usize = 4;
const QUESTION_TEXT_INDENT: usize = 8;
const QUESTION_OPTION_INDENT: usize = 10;

/// The rows the todos draw in the list, wrapped to `width`, each with its style. The
/// header row leads: closed it reads `▸ N todos waiting`, open `▾ Q1 · question · 1 of 2
/// todos · harold · spec` (instructions add `· 0 of 4 ticked` and show their title), with
/// `· from the chief` when `fallback` puts the rows at the top of the list. The open
/// question follows it: the intro, then every decision with its options, a blank row
/// between each, then the answer actions. Open instructions draw the intro, the steps
/// with their detail, then `done` and `ignore`. One row per other todo comes last. `cursor` is the stop
/// the cursor is on: its row has the `▶` marker and the yellow, bold style. A note being
/// typed shows under its own row, wherever the cursor is. A notice with no question is one coral row.
fn question_row_lines(
    view: &RowsView,
    width: usize,
    fallback: bool,
    cursor: Option<QStop>,
) -> Vec<QLine> {
    let mut out = Vec::new();
    let muted = Style::default().fg(MUTED);
    let ink = Style::default().fg(INK);
    let notice = view.notice.as_ref().map(|text| {
        (
            text,
            Style::default().fg(CORAL).add_modifier(Modifier::BOLD),
        )
    });
    if view.id.is_empty() {
        if let Some((text, style)) = notice {
            wrap_into(&mut out, QUESTION_HEADER_INDENT, text, width, style, None);
        }
        return out;
    }
    let yellow = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let on_cursor = Style::default()
        .fg(Color::Yellow)
        .bg(Color::Rgb(41, 40, 30))
        .add_modifier(Modifier::BOLD);
    // The text of a row, wrapped after `prefix` on its first row and under it on the
    // others; a cursor stop has the marker and the cursor style.
    let put_hanging = |out: &mut Vec<QLine>,
                       indent: usize,
                       prefix: &str,
                       text: &str,
                       style: Style,
                       stop: Option<QStop>| {
        let here = stop.is_some() && stop == cursor;
        let first = out.len();
        wrap_hanging(
            out,
            indent,
            prefix,
            text,
            width,
            if here { on_cursor } else { style },
            stop,
        );
        if here {
            out[first].text.replace_range(..2, "▶ ");
        }
    };
    let put =
        |out: &mut Vec<QLine>, indent: usize, text: &str, style: Style, stop: Option<QStop>| {
            put_hanging(out, indent, "", text, style, stop);
        };
    let from = if fallback { " · from the chief" } else { "" };
    if !view.open {
        let text = format!("▸ {}{from}", waiting_label(view.waiting));
        put(
            &mut out,
            QUESTION_HEADER_INDENT,
            &text,
            yellow,
            Some(QStop::Header),
        );
    } else {
        let ticked = match view.kind {
            TodoKind::Instructions => {
                format!(" · {} of {} ticked", view.ticked_count, view.steps.len())
            }
            TodoKind::Question => String::new(),
        };
        let label = match view.kind {
            TodoKind::Instructions => &view.title,
            TodoKind::Question => &view.spec,
        };
        let text = format!(
            "▾ {} · {} · {} of {}{ticked} · {} · {label}{from}",
            view.id,
            kind_word(view.kind),
            view.position,
            todos_count(view.waiting),
            view.product,
        );
        put(
            &mut out,
            QUESTION_HEADER_INDENT,
            &text,
            yellow,
            Some(QStop::Header),
        );
        let blank = |out: &mut Vec<QLine>| {
            out.push(QLine {
                text: String::new(),
                style: ink,
                stop: None,
            });
        };
        if !view.intro.is_empty() {
            put(&mut out, QUESTION_TEXT_INDENT, &view.intro, ink, None);
            blank(&mut out);
        }
        for (d, decision) in view.decisions.iter().enumerate() {
            put(
                &mut out,
                QUESTION_TEXT_INDENT,
                &decision.ask,
                ink.add_modifier(Modifier::BOLD),
                None,
            );
            for (o, option) in decision.options.iter().enumerate() {
                let stop = QStop::Option {
                    decision: d,
                    option: o,
                };
                let mark = if option.picked { '●' } else { '○' };
                let suggested = if option.suggested {
                    "  (suggested)"
                } else {
                    ""
                };
                let mut text = format!("{mark} {}  {}", option.key, option.label);
                if let Some(typed) = option.free_text.as_deref().filter(|t| !t.is_empty()) {
                    let room =
                        width.saturating_sub(QUESTION_OPTION_INDENT + Line::raw(&text).width() + 2);
                    text = format!("{text}: {}", fit(typed, room));
                }
                text.push_str(suggested);
                put(&mut out, QUESTION_OPTION_INDENT, &text, ink, Some(stop));
                if !option.impact.is_empty() {
                    let under = QUESTION_OPTION_INDENT + 4 + Line::raw(&option.key).width();
                    put(&mut out, under, &option.impact, muted, None);
                }
                if view.note_stop == Some(stop)
                    && let Some(note) = note_text(&view.mode, &[NoteKind::FreeText])
                {
                    let under = QUESTION_OPTION_INDENT + 4 + Line::raw(&option.key).width();
                    put(&mut out, under, &note, yellow, None);
                }
            }
            blank(&mut out);
        }
        for (i, step) in view.steps.iter().enumerate() {
            let mark = if step.ticked { '✓' } else { ' ' };
            let prefix = format!("[{mark}] {}  ", i + 1);
            let under = QUESTION_OPTION_INDENT + Line::raw(&prefix).width();
            put_hanging(
                &mut out,
                QUESTION_OPTION_INDENT,
                &prefix,
                &step.text,
                ink,
                Some(QStop::Step(i)),
            );
            if !step.detail.is_empty() {
                put(
                    &mut out,
                    under,
                    &step.detail,
                    ink.add_modifier(Modifier::DIM),
                    None,
                );
            }
        }
        if !view.steps.is_empty() {
            blank(&mut out);
            if view.all_ticked {
                let notice = format!(
                    "✓ All {} ticked. Press done to tell the chief.",
                    view.steps.len()
                );
                put(&mut out, QUESTION_TEXT_INDENT, &notice, ink, None);
                blank(&mut out);
            }
        }
        let actions: &[&str] = match view.kind {
            TodoKind::Instructions => &INSTRUCTION_ACTIONS,
            TodoKind::Question => &ANSWER_ACTIONS,
        };
        for (a, action) in actions.iter().enumerate() {
            let stop = QStop::Action(a);
            // The rule sits between `send` and the other actions.
            if a == 1 {
                put(
                    &mut out,
                    QUESTION_OPTION_INDENT,
                    "─────────────",
                    muted,
                    None,
                );
            }
            let style = if a == 0 {
                ink
            } else {
                ink.add_modifier(Modifier::DIM)
            };
            put(&mut out, QUESTION_OPTION_INDENT, action, style, Some(stop));
            if view.note_stop == Some(stop)
                && let Some(note) = note_text(&view.mode, &[NoteKind::Revise, NoteKind::Reject])
            {
                put(&mut out, QUESTION_OPTION_INDENT, &note, yellow, None);
            }
        }
        for other in &view.others {
            let ticked = match other.kind {
                TodoKind::Instructions => {
                    format!(" · {} of {} ticked", other.ticked_count, other.steps_total)
                }
                TodoKind::Question => String::new(),
            };
            let text = format!(
                "▸ {} · {}{ticked} · {} · {}",
                other.id,
                kind_word(other.kind),
                other.product,
                other.label
            );
            put(
                &mut out,
                QUESTION_HEADER_INDENT,
                &text,
                yellow,
                Some(QStop::Other(other.index)),
            );
        }
    }
    if let Some((text, style)) = notice {
        put(&mut out, QUESTION_HEADER_INDENT, text, style, None);
    }
    out
}

/// Appends `text` as rows within `width`, after `indent` spaces on each; only the first
/// row has the `stop`.
fn wrap_into(
    out: &mut Vec<QLine>,
    indent: usize,
    text: &str,
    width: usize,
    style: Style,
    stop: Option<QStop>,
) {
    wrap_hanging(out, indent, "", text, width, style, stop);
}

/// Like [`wrap_into`], with `prefix` before the text on the first row; the other rows
/// keep the text under the first row's.
fn wrap_hanging(
    out: &mut Vec<QLine>,
    indent: usize,
    prefix: &str,
    text: &str,
    width: usize,
    style: Style,
    stop: Option<QStop>,
) {
    let hang = Line::raw(prefix).width();
    let room = width.saturating_sub(indent + hang).max(1);
    let pad = " ".repeat(indent);
    for (n, piece) in wrap_words(text, room).into_iter().enumerate() {
        let lead = if n == 0 {
            prefix.to_string()
        } else {
            " ".repeat(hang)
        };
        out.push(QLine {
            text: format!("{pad}{lead}{piece}"),
            style,
            stop: stop.filter(|_| n == 0),
        });
    }
}

/// `text` broken at spaces into pieces no wider than `room`; a word wider than `room` is
/// broken inside.
fn wrap_words(text: &str, room: usize) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut current = String::new();
    for word in text.split(' ') {
        let mut word = word.to_string();
        while Line::raw(word.as_str()).width() > room {
            if !current.is_empty() {
                pieces.push(std::mem::take(&mut current));
            }
            let mut head = String::new();
            for c in word.chars() {
                head.push(c);
                if Line::raw(head.as_str()).width() > room && head.chars().count() > 1 {
                    head.pop();
                    break;
                }
            }
            word = word[head.len()..].to_string();
            pieces.push(head);
        }
        let joined = if current.is_empty() {
            word.clone()
        } else {
            format!("{current} {word}")
        };
        if Line::raw(joined.as_str()).width() <= room {
            current = joined;
        } else {
            pieces.push(std::mem::replace(&mut current, word));
        }
    }
    pieces.push(current);
    pieces
}

/// The kind of note open in the questions, if any.
fn note_kind(questions: &crate::questions::QuestionBlock) -> Option<NoteKind> {
    if !questions.note_open() {
        return None;
    }
    match questions.view()?.mode {
        Mode::Note { kind, .. } => Some(kind),
        Mode::Choosing => None,
    }
}

/// The note being typed, with its label and cursor block, when its kind is one of `kinds`.
fn note_text(mode: &Mode, kinds: &[NoteKind]) -> Option<String> {
    let Mode::Note { kind, text } = mode else {
        return None;
    };
    let label = match kind {
        _ if !kinds.contains(kind) => return None,
        NoteKind::Revise => "revise:",
        NoteKind::Reject => "reject:",
        NoteKind::FreeText => "answer:",
    };
    Some(format!("{label} {text}█"))
}

/// The column widths. A call never changes them: the chief's buttons share its
/// work cell.
fn inventory_widths(wide: bool) -> Vec<Constraint> {
    if wide {
        vec![
            Constraint::Length(13),
            Constraint::Length(26),
            Constraint::Length(1),
            Constraint::Length(2),
            Constraint::Min(12),
            Constraint::Length(7),
        ]
    } else {
        vec![
            Constraint::Length(11),
            Constraint::Length(20),
            Constraint::Length(1),
            Constraint::Length(2),
            Constraint::Min(8),
        ]
    }
}

/// The work column's width inside `area`, laid out as the table lays it out.
fn work_cell_width(area: Rect, widths: &[Constraint]) -> usize {
    let inner = Rect::new(0, 0, area.width.saturating_sub(2), 1);
    let columns = Layout::horizontal(widths.iter().copied())
        .flex(Flex::Start)
        .spacing(COLUMN_SPACING)
        .split(inner);
    columns
        .get(WORK_COLUMN)
        .map_or(0, |column| usize::from(column.width))
}

/// The chief's work cell during a call: what the chief was sent, or else what
/// became of the last turn.
fn call_work(view: &CallView) -> Option<String> {
    if !view.state.in_call() {
        return None;
    }
    if !view.last_transcript.is_empty() {
        return Some(format!("🎙 {}", view.last_transcript));
    }
    view.last_result.map(str::to_owned)
}

/// The chief's work cell: its summary, truncated first, then the buttons at the
/// right edge.
fn chief_work_line(
    summary: &str,
    view: &CallView,
    voice_muted: bool,
    now_ms: i64,
    width: usize,
) -> Line<'static> {
    let buttons = call_buttons(view, voice_muted, now_ms, width);
    let buttons_width: usize = buttons.iter().map(Span::width).sum();
    let room = width.saturating_sub(buttons_width + 1);
    let summary = if room == 0 {
        String::new()
    } else {
        fit(summary, room).into_owned()
    };
    let gap = width
        .saturating_sub(buttons_width)
        .saturating_sub(Line::raw(summary.as_str()).width());
    let mut spans = vec![Span::raw(summary), Span::raw(" ".repeat(gap))];
    spans.extend(buttons);
    Line::from(spans)
}

/// The five bars for `now_ms`: they step once every 150 ms.
fn speaking_bars(now_ms: i64) -> String {
    let step = now_ms.div_euclid(BAR_STEP_MS);
    (0..5)
        .map(|bar| {
            let level = (step + 2 * bar).rem_euclid(12);
            LEVELS[usize::try_from(level).unwrap_or(0)]
        })
        .collect()
}

/// The call button's texts for `view`, longest first, and its colours.
fn call_button_texts(view: &CallView, voice_muted: bool, now_ms: i64) -> (Vec<String>, Style) {
    let key_cap = Style::default().fg(INK).bg(KEY_CAP);
    let muted = Style::default().fg(MUTED);
    let lit = |colour| {
        Style::default()
            .fg(BOARD)
            .bg(colour)
            .add_modifier(Modifier::BOLD)
    };
    let texts = |texts: &[&str]| texts.iter().map(|text| (*text).to_owned()).collect();
    match view.state {
        CallState::Off if voice_muted => (
            texts(&["[c] 📞 call: voice muted", "[c] 📞 voice muted"]),
            muted,
        ),
        CallState::Off => (texts(&["[c] 📞 call"]), key_cap),
        CallState::Listening => (
            texts(&["[c] ● listening · end call", "[c] ● end call"]),
            lit(GREEN),
        ),
        CallState::Thinking => (
            texts(&["[c] … chief thinking · end call", "[c] … end call"]),
            lit(AMBER),
        ),
        CallState::Speaking => {
            let bars = speaking_bars(now_ms);
            (
                vec![
                    format!("[c] 📞 {bars} chief speaking · end call"),
                    format!("[c] 📞 {bars} end call"),
                    "[c] 📞 end call".to_owned(),
                ],
                lit(AMBER),
            )
        }
        CallState::NotSetUp => (
            texts(&["[c] 📞 call: not set up", "[c] 📞 not set up"]),
            muted,
        ),
    }
}

/// While the chief speaks, the call button's text (by its place in
/// `call_button_texts`) and talk now's, in the order they are tried. Talk now is
/// the only way to cut the chief short, so it outlasts the state words.
const SPEAKING_PAIRS: [(usize, Option<&str>); 6] = [
    (0, Some("[t] talk now")),
    (1, Some("[t] talk now")),
    (2, Some("[t] talk now")),
    (1, Some("[t] talk")),
    (2, Some("[t] talk")),
    (2, None),
];

/// The button (and talk-now button) spans for `view` in `width` columns: the
/// first of the call button's texts that fits or, while the chief speaks, the
/// first of `SPEAKING_PAIRS` that fits. The shortest is used when none does.
fn call_buttons(
    view: &CallView,
    voice_muted: bool,
    now_ms: i64,
    width: usize,
) -> Vec<Span<'static>> {
    let (texts, style) = call_button_texts(view, voice_muted, now_ms);
    let text_width = |text: &str| Line::raw(text).width();
    let (call, talk) = if view.state == CallState::Speaking {
        let pair_width = |(call, talk): &(usize, Option<&str>)| {
            text_width(&texts[*call]) + talk.map_or(0, |talk| 1 + text_width(talk))
        };
        SPEAKING_PAIRS
            .into_iter()
            .find(|pair| pair_width(pair) <= width)
            .unwrap_or(SPEAKING_PAIRS[SPEAKING_PAIRS.len() - 1])
    } else {
        let call = texts
            .iter()
            .position(|text| text_width(text) <= width)
            .unwrap_or(texts.len() - 1);
        (call, None)
    };
    let mut spans = vec![Span::styled(texts[call].clone(), style)];
    if let Some(talk) = talk {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(talk, Style::default().fg(INK).bg(KEY_CAP)));
    }
    spans
}

/// `prefix` + name truncated with an ellipsis so the pane-address suffix that
/// tells colliding agents apart always stays visible.
fn fit_with_suffix(prefix: &str, (name, suffix): (&str, &str), width: usize) -> String {
    let suffix_width = Line::raw(suffix).width();
    let head = format!("{prefix}{name}");
    if suffix_width + 1 >= width {
        return fit(&format!("{head}{suffix}"), width).into_owned();
    }
    format!("{}{suffix}", fit(&head, width - suffix_width))
}

/// The list's right-hand title: `(focus tracking off)` while the focus probe sees no
/// usable pane, and nothing otherwise. Waiting agents show by their ◆ marks instead.
fn focus_tracking_title(app: &App) -> Line<'static> {
    if app.focus_tracking() {
        return Line::default();
    }
    Line::styled(" (focus tracking off) ", Style::default().fg(MUTED))
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

/// The key where a narrow footer goes on to a second line: the list's `c`, or a
/// question row's `^L`.
fn footer_break_key(stop: Option<QStop>) -> &'static str {
    if stop.is_some() { "^L" } else { "c" }
}

/// What the footer needs to know of the current todo.
#[derive(Debug, Clone, Copy, Default)]
struct TodoFacts {
    /// It has a web link `o` opens.
    link: bool,
    /// It is instructions, not a question.
    instructions: bool,
}

/// The footer's hints for the row under the cursor, short or long: the list's on an
/// agent row (`stop` is `None`), the question's on a question row. Each form names a
/// break key, see [`footer_break_key`].
fn footer_hints(
    compact: bool,
    stop: Option<QStop>,
    open: bool,
    note: Option<NoteKind>,
    link: bool,
    instructions: bool,
) -> Vec<Span<'static>> {
    let key = |text| Span::styled(text, Style::default().fg(INK));
    let label = |text| Span::styled(text, Style::default().fg(MUTED));
    if let Some(note) = note {
        // Every key types into the note, so only Enter, Esc and the global keys are named.
        let (enter, enter_words) = match note {
            NoteKind::FreeText => ("Enter", " keep"),
            NoteKind::Revise | NoteKind::Reject => ("Enter", " send"),
        };
        if compact {
            return vec![
                label(" TODO  "),
                key("Enter"),
                Span::raw(format!("{}  ", enter_words)),
                key("Esc"),
                Span::raw(" discard  "),
                key("^L"),
                Span::raw(" retry  "),
                key("^Q"),
                Span::raw(" quit "),
            ];
        }
        return vec![
            label(" TODO  "),
            key(enter),
            Span::raw(format!("{enter_words}  ")),
            key("Esc"),
            Span::raw(" discard  "),
            key("^L"),
            Span::raw(" retry  "),
            key("^Q"),
            Span::raw(" quit "),
        ];
    }
    if let Some(stop) = stop {
        // The same words in both layouts: the label, then each key with its words.
        let mut pairs: Vec<(&str, &str)> = Vec::new();
        match stop {
            QStop::Header if open => pairs.push(("h", " close")),
            QStop::Header | QStop::Other(_) => pairs.push(("l", " open")),
            QStop::Option { .. } => pairs.extend([("Space", " pick"), ("h", " close")]),
            QStop::Step(_) => pairs.extend([("Space", " tick"), ("h", " close")]),
            QStop::Action(0) if instructions => pairs.extend([("Enter", " done"), ("h", " close")]),
            QStop::Action(1) if instructions => {
                pairs.extend([("Enter", " ignore"), ("h", " close")]);
            }
            QStop::Action(_) => pairs.extend([("Enter", " choose"), ("h", " close")]),
        }
        pairs.extend([("j/k", " move"), ("n/N", " todo")]);
        // The row of a todo that is not open names neither its link nor Esc.
        let inside = open && !matches!(stop, QStop::Other(_));
        if inside && link {
            pairs.push(("o", " link"));
        }
        if inside {
            pairs.push(("Esc", " close"));
        }
        pairs.extend([("^L", " retry"), ("^Q", " quit")]);
        let mut hints = vec![label(" TODO  ")];
        for (k, (name, words)) in pairs.iter().enumerate() {
            let gap = if k + 1 == pairs.len() { " " } else { "  " };
            hints.extend([key(name), Span::raw(format!("{words}{gap}"))]);
        }
        return hints;
    }
    if compact {
        return vec![
            key(" j/k"),
            Span::raw(" move "),
            key("n/N"),
            Span::raw(" wait "),
            key("f"),
            Span::raw(" search "),
            key("/"),
            Span::raw(" commands "),
            key("Enter"),
            Span::raw(" go "),
            key("c"),
            Span::raw(" call "),
            key("t"),
            Span::raw(" talk now "),
            key("^L"),
            Span::raw(" retry "),
            key("q"),
            Span::raw(" quit "),
        ];
    }
    vec![
        label(" DISPATCH  "),
        key("j/k"),
        Span::raw(" select  "),
        key("n/N"),
        Span::raw(" waiting  "),
        key("f"),
        Span::raw(" search  "),
        key("/"),
        Span::raw(" commands  "),
        key("Enter"),
        Span::raw(" switch pane  "),
        key("c"),
        Span::raw(" call  "),
        key("t"),
        Span::raw(" talk now  "),
        key("Esc"),
        Span::raw(" clear  "),
        key("^L"),
        Span::raw(" retry  "),
        key("q"),
        Span::raw(" quit "),
    ]
}

/// While keys are swallowed after the questions hid under the cursor, the footer says
/// the way out.
fn render_swallow_footer(frame: &mut Frame<'_>, area: Rect) {
    let key = Style::default().fg(INK);
    let words = Style::default().fg(Color::Yellow);
    let line = Line::from(vec![
        Span::styled(" Esc", key),
        Span::styled(" back to the list (the questions are away)", words),
    ]);
    frame.render_widget(
        Paragraph::new(line).block(board_block().borders(Borders::ALL)),
        area,
    );
}

/// While question keys are held after the question changed under them, the footer says
/// how to go on.
fn render_hold_footer(frame: &mut Frame<'_>, area: Rect) {
    let line = Line::from(Span::styled(
        " esc to continue",
        Style::default().fg(Color::Yellow),
    ));
    frame.render_widget(
        Paragraph::new(line).block(board_block().borders(Borders::ALL)),
        area,
    );
}

/// The footer for the row under the cursor, on one line when it fits. Otherwise it goes
/// on to a second line at its break key, or at the last key before that when the first
/// line still would not fit.
fn render_footer(
    frame: &mut Frame<'_>,
    area: Rect,
    compact: bool,
    stop: Option<QStop>,
    open: bool,
    note: Option<NoteKind>,
    todo: TodoFacts,
) {
    let mut hints = footer_hints(compact, stop, open, note, todo.link, todo.instructions);
    let room = usize::from(area.width).saturating_sub(2);
    if Line::from(hints.clone()).width() <= room {
        frame.render_widget(
            Paragraph::new(Line::from(hints)).block(board_block().borders(Borders::ALL)),
            area,
        );
        return;
    }
    // Too narrow for one line: the rest goes on a second line, in the row the top
    // border would take.
    let break_key = footer_break_key(stop);
    let named = hints
        .iter()
        .position(|span| span.content == break_key)
        .unwrap_or(hints.len());
    let fits = |at: usize| Line::from(hints[..at].to_vec()).width() <= room;
    let at = if fits(named) {
        named
    } else {
        (1..named)
            .rev()
            .find(|&at| hints[at].style.fg == Some(INK) && fits(at))
            .unwrap_or(named)
    };
    let mut second = hints.split_off(at);
    second.insert(0, Span::raw(" "));
    frame.render_widget(
        Paragraph::new(vec![Line::from(hints), Line::from(second)])
            .block(board_block().borders(Borders::LEFT | Borders::RIGHT | Borders::BOTTOM)),
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

/// One unit, rounded down, so a row's age changes at most once a minute.
fn age(now_ms: i64, transition_ms: i64) -> String {
    let minutes = now_ms.saturating_sub(transition_ms).max(0) / 60_000;
    match minutes {
        0 => "<1m".to_owned(),
        1..=59 => format!("{minutes}m"),
        60..=1_439 => format!("{}h", minutes / 60),
        _ => format!("{}d", minutes / 1_440),
    }
}

fn seconds_ago(now_ms: i64, then_ms: i64) -> String {
    format!("{}s", now_ms.saturating_sub(then_ms).max(0) / 1_000)
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use crate::api::HAROLD_VERSION_LIMIT;
    use crossterm::event::KeyCode;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::style::Color;
    use ratatui::text::Line;

    use super::{MUTED, QStop, fit, render};
    use crate::app::{
        AgentIncarnation, AgentRow, AgentState, App, CallState, CallView, ConnectionState,
        MonitorHealth, MonitorHealthState, SearchState, Snapshot,
    };

    #[test]
    fn wide_dashboard_shows_operator_signals_without_evidence_provenance() {
        let selected = incarnation("%17", 91700, 91844, "codex");
        let app = App::new(
            ConnectionState::Live,
            Snapshot {
                harold_version: None,
                tts_muted: false,
                messaging_paused: false,
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
                harold_version: None,
                tts_muted: false,
                messaging_paused: false,
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
                harold_version: None,
                tts_muted: false,
                messaging_paused: false,
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
        assert!(content.contains("Press Ctrl+L to retry now"));
        assert!(!content.contains("No configured agent panes found"));
    }

    #[test]
    fn stale_dashboard_marks_age_and_retains_last_committed_rows() {
        let selected = incarnation("%17", 91700, 91844, "codex");
        let mut app = App::new(
            ConnectionState::Stale,
            Snapshot {
                harold_version: None,
                tts_muted: false,
                messaging_paused: false,
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
            harold_version: None,
            tts_muted: false,
            messaging_paused: false,
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
                harold_version: None,
                tts_muted: false,
                messaging_paused: false,
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
                harold_version: None,
                through_event_version: 0,
                server_time_ms: 0,
                monitor_health: Vec::new(),
                rows: Vec::new(),
                tts_muted: false,
                messaging_paused: false,
            },
            empty_search(),
            None,
        );
        assert!(rendered(&unknown, 140, 38, 100_000).contains("○ VOICE —"));
        assert!(rendered(&unknown, 60, 18, 100_000).contains("○ VOICE —"));
    }

    fn paused_app() -> App {
        let mut app = live_app(Vec::new(), None);
        app.apply_later_snapshot(crate::app::Snapshot {
            through_event_version: 43,
            messaging_paused: true,
            ..app.snapshot.clone()
        })
        .unwrap();
        app
    }

    fn versioned_app(harold: Option<&str>, dash: &str, paused: bool) -> App {
        let mut app = if paused {
            paused_app()
        } else {
            let mut app = live_app(Vec::new(), None);
            app.apply_later_snapshot(crate::app::Snapshot {
                through_event_version: 43,
                ..app.snapshot.clone()
            })
            .unwrap();
            app
        };
        app.snapshot.harold_version = harold.map(str::to_owned);
        app.dash_version = dash.to_owned();
        app
    }

    #[test]
    fn masthead_shows_the_dashs_own_version_only() {
        let differs = rendered(
            &versioned_app(Some("v0.8.0"), "v0.9.0", false),
            140,
            38,
            100_000,
        );
        assert!(differs.contains("HAROLD / TMUX  v0.9.0  ·  TRANSPORT LIVE  ·  REV #00043"));
        assert!(!differs.contains("v0.8.0"));
        assert!(!differs.contains("(DASH"));

        let same = rendered(
            &versioned_app(Some("v0.9.0"), "v0.9.0", false),
            140,
            38,
            100_000,
        );
        assert!(same.contains("HAROLD / TMUX  v0.9.0  ·  TRANSPORT LIVE"));
        assert!(!same.contains("AGENT SIGNAL BOARD"));

        let source = rendered(
            &versioned_app(Some("source"), "source", false),
            140,
            38,
            100_000,
        );
        assert!(source.contains("HAROLD / TMUX  source  ·"));

        let none = rendered(&versioned_app(None, "v0.9.0", false), 140, 38, 100_000);
        assert!(none.contains("HAROLD / TMUX  v0.9.0  ·  TRANSPORT LIVE"));
    }

    #[test]
    fn masthead_marks_paused_messaging_at_every_supported_width() {
        let app = versioned_app(Some("v0.8.0"), "v0.9.0", true);

        let wide = rendered(&app, 140, 38, 100_000);
        assert!(wide.contains("HAROLD / TMUX  v0.9.0  ·  TRANSPORT LIVE  ·  REV #00043  ·  ● VOICE ON  ·  ■ MSG PAUSED"));

        // HAROLD / TMUX becomes TMX DASH.
        let medium = rendered(&app, 84, 38, 100_000);
        assert!(medium.contains(
            "TMX DASH v0.9.0 · TRANSPORT LIVE  ·  REV #00043  ·  ● VOICE ON  ·  ■ MSG PAUSED"
        ));
        assert!(!medium.contains("v0.8.0"));

        // Then TMX DASH and REV; the version, transport, voice and the paused marker remain.
        let minimum = rendered(&app, 60, 18, 100_000);
        assert!(minimum.contains("v0.9.0  ·  TRANSPORT LIVE  ·  ● VOICE ON  ·  ■ MSG PAUSED"));
        assert!(!minimum.contains("TMX DASH"));
        assert!(!minimum.contains("REV #"));
    }

    #[test]
    fn masthead_keeps_the_version_without_the_title_at_the_minimum_width() {
        let app = versioned_app(Some("source"), "source", false);
        let minimum = rendered(&app, 60, 18, 100_000);
        assert!(minimum.contains(" source  ·  TRANSPORT LIVE  ·  REV #"));
        assert!(!minimum.contains("TMX DASH"));
    }

    #[test]
    fn masthead_cuts_a_long_dash_tag_to_the_limit() {
        let tag = "v1.2.3-a-very-long-release-tag-012345678";
        assert_eq!(tag.chars().count(), 40);
        let cut: String = tag.chars().take(HAROLD_VERSION_LIMIT).collect();
        let app = versioned_app(None, tag, false);
        let content = rendered(&app, 200, 38, 100_000);
        assert!(content.contains(&format!("HAROLD / TMUX  {cut}  ·  TRANSPORT LIVE")));
        assert!(!content.contains("(DASH"));
    }

    #[test]
    fn masthead_shows_nothing_about_messaging_while_it_is_running() {
        let app = live_app(Vec::new(), None);
        for (width, height) in [(140, 38), (84, 38), (60, 18)] {
            let content = rendered(&app, width, height, 100_000);
            assert!(!content.contains("MSG"), "{width}x{height}");
            assert!(content.contains("REV #00042"), "{width}x{height}");
        }
    }

    #[test]
    fn paused_marker_is_coral() {
        let buffer = rendered_buffer(&paused_app(), 140, 38, 100_000);
        assert_styled_occurrences(&buffer, "■ MSG PAUSED", Color::Rgb(224, 114, 98), 1);
    }

    #[test]
    fn messaging_failure_is_reported_in_the_monitor_line() {
        let mut app = live_app(Vec::new(), None);
        app.set_runtime_status(crate::app::RuntimeStatus::MessagingFailed(
            "pause refused".into(),
        ));
        assert!(rendered(&app, 140, 38, 100_000).contains("MESSAGING ERROR: pause refused"));
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

        assert!(!content.contains("TMX DASH"));
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
        // The work column takes whatever width the fixed columns leave, and
        // the current layout leaves it wider than before (40 -> 42 glyphs).
        assert_eq!(content.matches('界').count(), 42);
    }

    #[test]
    fn long_shared_names_keep_their_pane_address_at_compact_and_wide_widths() {
        let name = "voice-mute-palette-extended";
        let mut first = row(
            incarnation("%1", 1, 2, "codex"),
            AgentState::Busy,
            "Codex",
            "work:0.3",
            "a",
            90_000,
        );
        first.working_directory = format!("/w/{name}");
        first.pane_index = 3;
        let mut second = row(
            incarnation("%2", 4, 5, "codex"),
            AgentState::Idle,
            "Codex",
            "work:0.4",
            "b",
            90_000,
        );
        second.working_directory = format!("/w/{name}");
        second.pane_index = 4;
        let app = live_app(vec![first, second], None);
        for width in [70, 84, 100, 140] {
            let content = rendered(&app, width, 29, 100_000);
            assert!(content.contains(" :0.3"), "{width}: {content}");
            assert!(content.contains(" :0.4"), "{width}: {content}");
            assert!(content.contains("voice-mute"), "{width}");
        }
    }

    #[test]
    fn fit_with_suffix_truncates_the_name_and_keeps_the_suffix() {
        assert_eq!(
            super::fit_with_suffix("", ("tmux-agent-dash", " :0.3"), 12),
            "tmux-a… :0.3"
        );
        assert_eq!(super::fit_with_suffix("", ("sre", ""), 18), "sre");
        assert_eq!(super::fit_with_suffix("", ("abc", " :0.3"), 3), "ab…");
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
            harold_version: None,
            tts_muted: false,
            messaging_paused: false,
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
                harold_version: None,
                through_event_version: 0,
                server_time_ms: 0,
                monitor_health: Vec::new(),
                rows: Vec::new(),
                tts_muted: false,
                messaging_paused: false,
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
    fn footer_lists_every_hint_untruncated_at_all_widths() {
        let list = [
            "j/k",
            "n/N",
            "f search",
            "/ commands",
            "Enter",
            "^L retry",
            "q quit",
        ];
        for (width, height) in [(140, 38), (84, 30), (70, 30), (60, 18)] {
            for app in [user_layout(), question_app(false)] {
                let content = rendered(&app, width, height, 100_000);
                for hint in list {
                    assert!(content.contains(hint), "{hint} missing at {width}x{height}");
                }
            }
        }
    }

    fn call_layout(view: CallView) -> App {
        let mut app = user_layout();
        app.observe_call(Some(view));
        app
    }

    fn chief_call(state: CallState) -> CallView {
        CallView {
            state,
            chief_pane_id: "%3".into(),
            last_transcript: String::new(),
            last_result: None,
        }
    }

    /// The screen's text, one string per terminal row, without the cells a wide
    /// character covers, so `📞 call` reads as written.
    fn text_lines(app: &App, width: u16, height: u16, now_ms: i64) -> Vec<String> {
        let buffer = rendered_buffer(app, width, height, now_ms);
        buffer
            .content()
            .chunks(usize::from(buffer.area.width))
            .map(|cells| {
                let mut line = String::new();
                let mut hidden = 0;
                for cell in cells {
                    if hidden > 0 {
                        hidden -= 1;
                        continue;
                    }
                    line.push_str(cell.symbol());
                    hidden = Line::raw(cell.symbol()).width().saturating_sub(1);
                }
                line
            })
            .collect()
    }

    fn chief_line(lines: &[String]) -> &str {
        lines
            .iter()
            .find(|line| line.contains("sre") && line.contains("IDLE"))
            .expect("the chief's row")
    }

    /// The style of the first cell of the first `needle` on screen.
    fn style_at(buffer: &Buffer, needle: &str) -> ratatui::style::Style {
        let symbols: Vec<String> = needle.chars().map(String::from).collect();
        buffer
            .content()
            .windows(symbols.len())
            .find(|cells| {
                cells
                    .iter()
                    .zip(&symbols)
                    .all(|(cell, symbol)| cell.symbol() == symbol)
            })
            .map(|cells| cells[0].style())
            .unwrap_or_else(|| panic!("{needle:?} is not on screen"))
    }

    const CALL_WIDTHS: [u16; 6] = [60, 70, 84, 100, 125, 140];
    const IN_CALL_AND_OFF: [CallState; 4] = [
        CallState::Off,
        CallState::Listening,
        CallState::Thinking,
        CallState::Speaking,
    ];

    /// The chief's buttons on its row: from `[c]` to the end of the work cell,
    /// which is the list's border or, in the wide form, the AGE cell after it.
    fn chief_buttons_text(lines: &[String]) -> String {
        let line = chief_line(lines);
        let start = line.find("[c]").expect("the call button");
        let tail = &line[start..];
        let cell = tail[..tail.find('│').unwrap_or(tail.len())].trim_end();
        cell.strip_suffix("<1m")
            .unwrap_or(cell)
            .trim_end()
            .to_owned()
    }

    #[test]
    fn the_button_is_only_on_the_chief_row() {
        for (state, width, height) in [
            (CallState::Off, 140, 38),
            (CallState::Off, 100, 30),
            (CallState::Speaking, 60, 30),
            (CallState::Listening, 70, 30),
        ] {
            let app = call_layout(chief_call(state));
            let lines = text_lines(&app, width, height, 100_000);
            let expected = if state == CallState::Off {
                "[c] 📞 call"
            } else {
                "end call"
            };
            assert!(
                chief_line(&lines).contains(expected),
                "{state:?} {width}: {lines:#?}"
            );
            assert_eq!(
                lines.iter().filter(|line| line.contains("[c]")).count(),
                1,
                "{state:?} {width}"
            );
        }

        let mut moved = call_layout(chief_call(CallState::Off));
        moved.observe_call(Some(CallView {
            chief_pane_id: "%1".into(),
            ..chief_call(CallState::Off)
        }));
        let lines = text_lines(&moved, 140, 38, 100_000);
        let main = lines
            .iter()
            .find(|line| line.contains("main") && line.contains("BUSY"))
            .unwrap();
        assert!(main.contains("[c] 📞 call"));
        assert!(!chief_line(&lines).contains("[c]"));

        let mut unknown = call_layout(chief_call(CallState::Off));
        unknown.observe_call(None);
        assert!(!rendered(&unknown, 140, 38, 100_000).contains("[c]"));
    }

    #[test]
    fn each_state_shows_the_specs_text() {
        let now_ms = 100_000;
        let key_cap = Color::Rgb(58, 58, 48);
        let ink = Color::Rgb(237, 231, 216);
        let board = Color::Rgb(20, 20, 16);
        let green = Color::Rgb(125, 200, 139);
        let amber = Color::Rgb(224, 174, 85);
        let muted = Color::Rgb(143, 138, 127);
        let speaking = format!(
            "[c] 📞 {} chief speaking · end call",
            super::speaking_bars(now_ms)
        );
        for (state, voice_muted, text, fg, bg) in [
            (CallState::Off, false, "[c] 📞 call", ink, Some(key_cap)),
            (
                CallState::Listening,
                false,
                "[c] ● listening · end call",
                board,
                Some(green),
            ),
            (
                CallState::Thinking,
                false,
                "[c] … chief thinking · end call",
                board,
                Some(amber),
            ),
            (
                CallState::Speaking,
                false,
                speaking.as_str(),
                board,
                Some(amber),
            ),
            (
                CallState::NotSetUp,
                false,
                "[c] 📞 call: not set up",
                muted,
                None,
            ),
            (
                CallState::Off,
                true,
                "[c] 📞 call: voice muted",
                muted,
                None,
            ),
        ] {
            // The full texts, with room to spare.
            let spans = super::call_buttons(&chief_call(state), voice_muted, now_ms, 200);
            assert_eq!(spans[0].content, text, "{state:?}");
            assert_eq!(spans[0].style.fg, Some(fg), "{state:?}");
            assert_eq!(spans[0].style.bg, bg, "{state:?}");
            let lit = matches!(
                state,
                CallState::Listening | CallState::Thinking | CallState::Speaking
            );
            assert_eq!(
                spans[0]
                    .style
                    .add_modifier
                    .contains(ratatui::style::Modifier::BOLD),
                lit,
                "{state:?}"
            );
            if state == CallState::Speaking {
                assert_eq!(spans.len(), 3);
                assert_eq!(spans[2].content, "[t] talk now");
                assert_eq!(
                    (spans[2].style.fg, spans[2].style.bg),
                    (Some(ink), Some(key_cap))
                );
            } else {
                assert_eq!(spans.len(), 1, "{state:?}");
            }

            // On screen at 140 columns, beside the detail panel. Its 39-column cell
            // holds every full text but the chief speaking with talk now.
            let mut app = call_layout(chief_call(state));
            app.snapshot.tts_muted = voice_muted;
            let lines = text_lines(&app, 140, 38, now_ms);
            let on_screen = if state == CallState::Speaking {
                format!(
                    "[c] 📞 {} end call [t] talk now",
                    super::speaking_bars(now_ms)
                )
            } else {
                text.to_owned()
            };
            assert_eq!(
                chief_buttons_text(&lines),
                on_screen,
                "{state:?}: {lines:#?}"
            );
            let style = style_at(&rendered_buffer(&app, 140, 38, now_ms), "[c]");
            assert_eq!(style.fg, Some(fg), "{state:?}");
            if let Some(bg) = bg {
                assert_eq!(style.bg, Some(bg), "{state:?}");
            }
        }

        // The spec's full speaking row, wherever the cell is 51 columns or more: at
        // 110 columns there is no detail panel and the cell is 54.
        let speaking = call_layout(chief_call(CallState::Speaking));
        let lines = text_lines(&speaking, 110, 38, now_ms);
        assert_eq!(
            chief_buttons_text(&lines),
            format!("{} [t] talk now", speaking_full(now_ms))
        );
        let style = style_at(&rendered_buffer(&speaking, 110, 38, now_ms), "[t]");
        assert_eq!((style.fg, style.bg), (Some(ink), Some(key_cap)));
    }

    fn speaking_full(now_ms: i64) -> String {
        format!(
            "[c] 📞 {} chief speaking · end call",
            super::speaking_bars(now_ms)
        )
    }

    #[test]
    fn call_buttons_shorten_in_the_spec_order() {
        let now_ms = 100_000;
        let bars = super::speaking_bars(now_ms);
        let texts = |state, voice_muted, width| {
            super::call_buttons(&chief_call(state), voice_muted, now_ms, width)
                .iter()
                .map(|span| span.content.clone().into_owned())
                .collect::<String>()
        };
        for (state, voice_muted, short) in [
            (CallState::Listening, false, "[c] ● end call"),
            (CallState::Thinking, false, "[c] … end call"),
            (CallState::NotSetUp, false, "[c] 📞 not set up"),
            (CallState::Off, true, "[c] 📞 voice muted"),
        ] {
            let full = super::call_buttons(&chief_call(state), voice_muted, now_ms, 200)[0].width();
            assert_eq!(texts(state, voice_muted, full - 1), short, "{state:?}");
        }
        // Off has one text.
        assert_eq!(texts(CallState::Off, false, 4), "[c] 📞 call");

        // Speaking: the first pair that fits, talk now outlasting the state words.
        let full = format!("[c] 📞 {bars} chief speaking · end call [t] talk now");
        let middle = format!("[c] 📞 {bars} end call [t] talk now");
        let short = "[c] 📞 end call [t] talk now".to_owned();
        for (width, expected) in [
            (200, full.clone()),
            (51, full.clone()),
            (50, middle.clone()),
            (34, middle.clone()),
            (33, short.clone()),
            (28, short.clone()),
            (27, "[c] 📞 end call [t] talk".to_owned()),
            (24, "[c] 📞 end call [t] talk".to_owned()),
            (23, "[c] 📞 end call".to_owned()),
            (15, "[c] 📞 end call".to_owned()),
            (5, "[c] 📞 end call".to_owned()),
        ] {
            assert_eq!(
                texts(CallState::Speaking, false, width),
                expected,
                "{width}"
            );
        }
    }

    #[test]
    fn the_chief_row_keeps_end_call_at_every_width() {
        let now_ms = 100_000;
        let bars = super::speaking_bars(now_ms);
        for width in CALL_WIDTHS {
            for state in IN_CALL_AND_OFF {
                let lines = text_lines(&call_layout(chief_call(state)), width, 38, now_ms);
                let expected = if state == CallState::Off {
                    "[c] 📞 call"
                } else {
                    "end call"
                };
                assert!(
                    chief_buttons_text(&lines).contains(expected),
                    "{state:?} at {width}: {lines:#?}"
                );
            }
        }
        // The exact texts the order gives. Talk now shows from 70 columns, where the
        // cell (28 columns or more) has room for it.
        let thinking = "[c] … chief thinking · end call".to_owned();
        let short_thinking = "[c] … end call".to_owned();
        let middle = format!("[c] 📞 {bars} end call [t] talk now");
        let short = "[c] 📞 end call [t] talk now".to_owned();
        for (width, state, expected) in [
            (60, CallState::Thinking, short_thinking.clone()),
            (70, CallState::Thinking, short_thinking.clone()),
            (84, CallState::Thinking, short_thinking.clone()),
            (100, CallState::Thinking, thinking.clone()),
            (125, CallState::Thinking, short_thinking.clone()),
            (140, CallState::Thinking, thinking.clone()),
            (60, CallState::Speaking, "[c] 📞 end call".to_owned()),
            (70, CallState::Speaking, short.clone()),
            (84, CallState::Speaking, short.clone()),
            (100, CallState::Speaking, middle.clone()),
            (125, CallState::Speaking, short.clone()),
            (140, CallState::Speaking, middle.clone()),
        ] {
            let lines = text_lines(&call_layout(chief_call(state)), width, 38, now_ms);
            assert_eq!(chief_buttons_text(&lines), expected, "{state:?} at {width}");
        }
    }

    #[test]
    fn a_call_changes_only_the_chief_row() {
        let now_ms = 100_000;
        let quiet = {
            let mut app = user_layout();
            app.observe_call(None);
            app
        };
        for width in CALL_WIDTHS {
            let before = rendered_buffer(&quiet, width, 38, now_ms);
            let before_lines = text_lines(&quiet, width, 38, now_ms);
            let chief_index = before_lines
                .iter()
                .position(|line| line.contains("sre") && line.contains("IDLE"))
                .unwrap();
            let main = before_lines
                .iter()
                .find(|line| line.contains("main") && line.contains("BUSY"))
                .unwrap();
            assert!(
                main.contains("cc") && main.contains("fixing handler"),
                "{width}"
            );
            for state in IN_CALL_AND_OFF {
                let during = rendered_buffer(&call_layout(chief_call(state)), width, 38, now_ms);
                let row_width = usize::from(width);
                for (index, (old, new)) in before
                    .content()
                    .chunks(row_width)
                    .zip(during.content().chunks(row_width))
                    .enumerate()
                {
                    if index != chief_index {
                        assert_eq!(old, new, "{state:?} at {width}, row {index}");
                    }
                }
            }
        }
    }

    #[test]
    fn bars_change_every_150_ms_and_only_while_speaking() {
        use super::speaking_bars;
        assert_eq!(speaking_bars(0), speaking_bars(149));
        assert_ne!(speaking_bars(0), speaking_bars(150));
        assert_eq!(speaking_bars(150), speaking_bars(299));
        assert_eq!(speaking_bars(0).chars().count(), 5);
        assert!(speaking_bars(0).chars().all(|bar| "▁▂▃▄▅▆▇".contains(bar)));
        // The pattern repeats after twelve steps.
        assert_eq!(speaking_bars(0), speaking_bars(12 * 150));

        let speaking = call_layout(chief_call(CallState::Speaking));
        assert_ne!(
            rendered(&speaking, 140, 38, 100_000),
            rendered(&speaking, 140, 38, 100_150)
        );
        for state in [
            CallState::Off,
            CallState::Listening,
            CallState::Thinking,
            CallState::NotSetUp,
        ] {
            let content = rendered(&call_layout(chief_call(state)), 140, 38, 100_000);
            assert!(!content.chars().any(|c| "▁▂▃▄▅▆▇".contains(c)), "{state:?}");
        }
    }

    #[test]
    fn footer_lists_call_and_talk_now() {
        for (width, height) in [
            (140, 38),
            (super::WIDE_FOOTER_WIDTH, 38),
            (super::WIDE_FOOTER_WIDTH - 1, 38),
            (100, 30),
            (84, 30),
            (70, 30),
            (60, 18),
        ] {
            let content = rendered(&user_layout(), width, height, 100_000);
            for hint in [
                "j/k",
                "n/N",
                "f search",
                "/ commands",
                "Enter",
                "c call",
                "t talk now",
                "q quit",
            ] {
                assert!(content.contains(hint), "{hint} missing at {width}x{height}");
            }
        }
        // The long form fits exactly at its threshold, inside the footer's borders.
        assert_eq!(
            Line::from(super::footer_hints(false, None, false, None, false, false)).width() + 2,
            usize::from(super::WIDE_FOOTER_WIDTH)
        );
    }

    #[test]
    fn the_transcript_shows_on_the_chief_row_during_a_call() {
        let heard = CallView {
            last_transcript: "approve q19".into(),
            last_result: Some("typed"),
            ..chief_call(CallState::Listening)
        };
        let lines = text_lines(&call_layout(heard.clone()), 100, 30, 100_000);
        assert!(chief_line(&lines).contains("🎙 approve q19"), "{lines:#?}");
        assert!(!chief_line(&lines).contains("waiting"));

        let dropped = CallView {
            last_transcript: String::new(),
            last_result: Some("nothing heard"),
            ..chief_call(CallState::Listening)
        };
        let lines = text_lines(&call_layout(dropped), 100, 30, 100_000);
        assert!(chief_line(&lines).contains("nothing heard"));

        // Where the cell is tight the transcript is cut first; the button stays whole.
        let lines = text_lines(&call_layout(heard.clone()), 60, 30, 100_000);
        assert!(chief_line(&lines).contains("🎙 ap…"), "{lines:#?}");
        assert!(chief_line(&lines).contains("[c] ● end call"));

        // Before any turn the summary stays.
        let lines = text_lines(
            &call_layout(chief_call(CallState::Listening)),
            140,
            38,
            100_000,
        );
        assert!(chief_line(&lines).contains("waiting"));

        // Once the call ends the work summary comes back.
        let ended = CallView {
            state: CallState::Off,
            ..heard
        };
        let lines = text_lines(&call_layout(ended), 140, 38, 100_000);
        assert!(chief_line(&lines).contains("waiting"));
        assert!(!chief_line(&lines).contains("🎙"));
    }

    #[test]
    fn a_failed_call_is_reported_in_the_monitor_line() {
        let mut app = live_app(Vec::new(), None);
        app.set_runtime_status(crate::app::RuntimeStatus::CallFailed(
            "start refused".into(),
        ));
        assert!(rendered(&app, 140, 38, 100_000).contains("CALL ERROR: start refused"));
    }

    #[test]
    fn age_shows_one_unit_rounded_down() {
        const SECOND: i64 = 1_000;
        const MINUTE: i64 = 60 * SECOND;
        const HOUR: i64 = 60 * MINUTE;
        const DAY: i64 = 24 * HOUR;
        let now_ms = 1_777_000_000_000;
        for (elapsed_ms, expected) in [
            (0, "<1m"),
            (42 * SECOND, "<1m"),
            (59 * SECOND, "<1m"),
            (MINUTE, "1m"),
            (12 * MINUTE + 50 * SECOND, "12m"),
            (59 * MINUTE + 59 * SECOND, "59m"),
            (HOUR, "1h"),
            (3 * HOUR + 59 * MINUTE, "3h"),
            (23 * HOUR + 59 * MINUTE, "23h"),
            (DAY, "1d"),
            (2 * DAY + 5 * HOUR, "2d"),
            (400 * DAY, "400d"),
            // A state change timestamped in the future, when the clocks differ.
            (-5 * MINUTE, "<1m"),
        ] {
            assert_eq!(
                super::age(now_ms, now_ms - elapsed_ms),
                expected,
                "{elapsed_ms} ms"
            );
        }
    }

    #[test]
    fn seconds_ago_keeps_seconds() {
        assert_eq!(super::seconds_ago(100_000, 95_000), "5s");
        assert_eq!(super::seconds_ago(700_000, 100_000), "600s");
        assert_eq!(super::seconds_ago(100_000, 105_000), "0s");
    }

    #[test]
    fn pane_list_and_detail_panel_show_no_age_in_seconds() {
        let selected = incarnation("%17", 91700, 91844, "codex");
        let now_ms = 1_777_000_000_000;
        let app = App::new(
            ConnectionState::Live,
            Snapshot {
                harold_version: None,
                tts_muted: false,
                messaging_paused: false,
                through_event_version: 42,
                server_time_ms: now_ms,
                monitor_health: vec![health(MonitorHealthState::Healthy, "ok")],
                rows: vec![row(
                    selected.clone(),
                    AgentState::Idle,
                    "Codex",
                    "agents:2.7",
                    "Review the projection contract",
                    now_ms - 5 * 60_000,
                )],
            },
            empty_search(),
            Some(selected),
        );

        let lines = rendered_lines(&app, 140, 38, now_ms);

        assert_eq!(lines.matches("5m").count(), 2, "pane list and detail panel");
        let has_seconds_age = lines.split_whitespace().any(|word| {
            word.strip_suffix('s').is_some_and(|digits| {
                !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
            })
        });
        assert!(!has_seconds_age, "an age in seconds is shown:\n{lines}");
    }

    /// The queue of `question_app`: Q25 with one decision and two options.
    fn q25_queue() -> crate::questions::Queue {
        use crate::questions::{DecisionView, OptionView, QuestionView, Queue};
        let option = |key: &str, label: &str, impact: &str, suggested: bool| OptionView {
            key: key.into(),
            label: label.into(),
            impact: impact.into(),
            suggested,
            free: false,
        };
        Queue {
            questions: vec![QuestionView {
                id: "Q25".into(),
                product: "harold".into(),
                spec: "durable-server".into(),
                sha: "d84bf386a2a5".into(),
                link: "https://example.com/spec".into(),
                intro: "The server needs a wire protocol.".into(),
                version: 1,
                deferred: false,
                title: String::new(),
                steps: vec![],
                ticked: vec![],
                decisions: vec![DecisionView {
                    ask: "Which protocol?".into(),
                    options: vec![
                        option(
                            "h",
                            "libSQL's Hrana",
                            "one client for local and remote",
                            true,
                        ),
                        option("g", "gRPC", "a second schema to keep", false),
                    ],
                }],
            }],
            waiting: 2,
            dismissed: Vec::new(),
        }
    }

    /// One question, Q25, with one decision asking "Which protocol?" and two options,
    /// under no chief row: its rows are at the top of the list. `focused` puts the cursor
    /// on its header.
    fn question_app(focused: bool) -> App {
        let selected = incarnation("%17", 91700, 91844, "codex");
        let mut app = live_app(
            vec![row(
                selected.clone(),
                AgentState::Busy,
                "Codex",
                "tmx-agent-dash:0.1",
                "Build the question block",
                90_000,
            )],
            Some(selected),
        );
        app.observe_questions(q25_queue());
        if focused {
            app.handle_key(KeyCode::Char('k'));
            assert!(app.cursor_on_question());
        }
        app
    }

    /// Opens the question and a revise note through its action row.
    fn open_revise_note(app: &mut App) {
        for code in [
            KeyCode::Char('l'),
            KeyCode::Char('j'),
            KeyCode::Char('j'),
            KeyCode::Char('j'),
            KeyCode::Char('j'),
            KeyCode::Char('j'),
            KeyCode::Enter,
        ] {
            app.handle_key(code);
        }
        assert!(app.questions.note_open());
    }

    /// The row holding `needle`, and its index.
    fn row_with(lines: &str, needle: &str) -> usize {
        lines
            .lines()
            .position(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("missing {needle:?}:\n{lines}"))
    }

    const NOW: &str = "Ship the backlog queue tool, then import the live lists after install";

    fn now_app() -> App {
        let selected = incarnation("%17", 91700, 91844, "codex");
        let mut app = live_app(
            vec![row(
                selected.clone(),
                AgentState::Busy,
                "Codex",
                "tmx-agent-dash:0.1",
                "Build the question block",
                90_000,
            )],
            Some(selected),
        );
        app.observe_now(Some(NOW.into()));
        app
    }

    #[test]
    fn the_now_line_sits_under_the_masthead_at_140_84_and_60_columns() {
        let app = now_app();
        for width in [140, 84, 60] {
            let lines = rendered_lines(&app, width, 30, 100_000);
            println!("--- NOW line at {width} columns ---\n{lines}");
            let rows: Vec<&str> = lines.lines().collect();
            assert!(rows[1].contains("TRANSPORT LIVE"), "{lines}");
            assert!(rows[2].starts_with('└'), "the masthead is 3 rows:\n{lines}");
            let expected = fit(&format!("NOW  {NOW}"), usize::from(width - 2)).into_owned();
            assert_eq!(rows[3].trim_end(), format!(" {expected}"), "{lines}");
            assert_eq!(width == 60, expected.ends_with('…'), "cut only at 60");
            assert_eq!(row_with(&lines, "NOW  "), 3, "one Now row:\n{lines}");

            let buffer = rendered_buffer(&app, width, 30, 100_000);
            let n = &buffer[(1, 3)];
            assert_eq!(n.symbol(), "N");
            assert_eq!(n.fg, MUTED, "the Now line is dim secondary text");
        }
    }

    #[test]
    fn the_now_line_is_hidden_with_no_now_item() {
        let mut app = now_app();
        app.observe_now(None);
        for width in [140, 84, 60] {
            let lines = rendered_lines(&app, width, 30, 100_000);
            assert!(!lines.contains("NOW  "), "{lines}");
            assert!(!lines.contains("Ship the backlog"), "{lines}");
        }
    }

    #[test]
    fn the_now_line_gives_way_on_the_smallest_terminal() {
        // At the minimum height the list keeps its six rows, so the line hides; one row
        // more and it shows. The footer stays in view either way.
        let with = now_app();
        let mut without = now_app();
        without.observe_now(None);
        assert_eq!(
            rendered_lines(&with, 60, 18, 100_000),
            rendered_lines(&without, 60, 18, 100_000)
        );
        let lines = rendered_lines(&with, 60, 19, 100_000);
        assert_eq!(row_with(&lines, "NOW  "), 3, "{lines}");
        row_with(&lines, "q quit");
    }

    #[test]
    fn the_footer_names_esc_while_keys_are_swallowed() {
        let mut app = question_app(true);
        app.hide_questions();
        let lines = rendered_lines(&app, 140, 38, 100_000);
        row_with(&lines, "Esc back to the list (the questions are away)");
        assert!(!lines.contains("Q25"), "the block is hidden");
        app.handle_key(KeyCode::Esc);
        let lines = rendered_lines(&app, 140, 38, 100_000);
        row_with(&lines, "q quit");
    }

    #[test]
    fn the_footer_says_esc_to_continue_while_block_keys_are_held() {
        let mut app = question_app(true);
        open_revise_note(&mut app);
        // The question is withdrawn while its note is being typed.
        app.observe_questions(crate::questions::Queue::default());
        assert!(app.holding_block_keys());
        let lines = rendered_lines(&app, 140, 38, 100_000);
        row_with(&lines, "esc to continue");
    }

    /// The footer's rows: the last three of the screen.
    fn footer_rows(lines: &str) -> String {
        let rows: Vec<&str> = lines.lines().collect();
        rows[rows.len() - 3..].join("\n")
    }

    fn hint_text(compact: bool, stop: Option<QStop>, open: bool) -> String {
        super::footer_hints(compact, stop, open, None, true, false)
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    const ALL_STOPS: [Option<QStop>; 4] = [
        None,
        Some(QStop::Header),
        Some(QStop::Option {
            decision: 0,
            option: 0,
        }),
        Some(QStop::Action(0)),
    ];

    #[test]
    fn a_closed_header_footer_does_not_list_esc() {
        for compact in [false, true] {
            let closed = hint_text(compact, Some(QStop::Header), false);
            assert!(!closed.contains("Esc"), "{closed}");
            let open = hint_text(compact, Some(QStop::Header), true);
            assert!(open.contains("Esc"), "{open}");
        }
    }

    #[test]
    fn agent_row_footer_is_the_list_hints_without_tab() {
        for compact in [false, true] {
            let text = hint_text(compact, None, false);
            for hint in ["j/k", "n/N", "f", "/", "Enter", "c", "t", "^L", "q"] {
                assert!(text.contains(hint), "{hint} missing: {text}");
            }
            assert!(!text.to_lowercase().contains("tab"), "{text}");
            assert!(!text.contains("^Q"), "{text}");
        }
    }

    #[test]
    fn question_row_footer_names_j_k_l_h_space_enter_n_o_esc() {
        let has = |text: &str, key: &str, words: &str| {
            text.contains(&format!("{key} {words}")) || text.contains(&format!("{key}{words}"))
        };
        for compact in [false, true] {
            let closed = hint_text(compact, Some(QStop::Header), false);
            let header = hint_text(compact, Some(QStop::Header), true);
            let option = hint_text(
                compact,
                Some(QStop::Option {
                    decision: 0,
                    option: 0,
                }),
                true,
            );
            let action = hint_text(compact, Some(QStop::Action(1)), true);
            assert!(
                has(&closed, "l", "open") && !has(&closed, "h", "close"),
                "{closed}"
            );
            assert!(!closed.contains("Enter"), "{closed}");
            assert!(
                has(&header, "h", "close") && !has(&header, "l", "open"),
                "{header}"
            );
            assert!(!header.contains("Enter"), "{header}");
            assert!(
                has(&option, "Space", "pick") && has(&option, "h", "close"),
                "{option}"
            );
            assert!(action.contains("Enter"), "{action}");
            assert!(!header.contains("Space") && !action.contains("Space"));
            assert!(!closed.contains("Esc"), "{closed}");
            for text in [&closed, &header, &option, &action] {
                for key in ["j/k", "n/N", "o", "^L", "^Q"] {
                    assert!(text.contains(key), "{key} missing: {text}");
                }
                assert!(
                    std::ptr::eq(text, &closed) || text.contains("Esc"),
                    "{text}"
                );
                assert!(
                    !text.contains("f search") && !text.contains("talk now"),
                    "{text}"
                );
            }
        }
    }

    #[test]
    fn no_footer_contains_tab() {
        for stop in ALL_STOPS {
            for compact in [false, true] {
                let text = hint_text(compact, stop, true);
                assert!(!text.to_lowercase().contains("tab"), "{stop:?}: {text}");
            }
        }
    }

    #[test]
    fn swallow_footer_reads_esc_back_to_the_list_the_questions_are_away() {
        let mut app = question_app(true);
        app.hide_questions();
        let lines = footer_rows(&rendered_lines(&app, 140, 38, 100_000));
        assert!(
            lines.contains("Esc back to the list (the questions are away)"),
            "{lines}"
        );
    }

    #[test]
    fn hold_footer_reads_esc_to_continue() {
        let mut app = question_app(true);
        open_revise_note(&mut app);
        app.observe_questions(crate::questions::Queue::default());
        let lines = footer_rows(&rendered_lines(&app, 140, 38, 100_000));
        assert!(lines.contains("esc to continue"), "{lines}");
    }

    #[test]
    fn footer_wraps_to_two_lines_at_the_break_key() {
        for (stop, break_key) in [(None, "c"), (Some(QStop::Header), "^L")] {
            let hints = super::footer_hints(true, stop, false, None, false, false);
            let at = hints.iter().position(|s| s.content == break_key).unwrap();
            let first = Line::from(hints[..at].to_vec()).width();
            let width = u16::try_from(first + 2).unwrap();
            let backend = TestBackend::new(width, 3);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| {
                    super::render_footer(
                        frame,
                        frame.area(),
                        true,
                        stop,
                        false,
                        None,
                        super::TodoFacts::default(),
                    )
                })
                .unwrap();
            let buffer = terminal.backend().buffer().clone();
            let row = |y: u16| -> String {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_owned())
                    .collect()
            };
            assert!(row(0).contains("j/k"), "{}", row(0));
            assert!(
                !row(0).split_whitespace().any(|w| w == break_key),
                "{stop:?}: {}",
                row(0)
            );
            assert!(
                row(1).trim_start_matches([' ', '│']).starts_with(break_key),
                "{stop:?}: {}",
                row(1)
            );
        }
    }

    #[test]
    fn the_frame_footer_follows_the_row_under_the_cursor() {
        let mut app = question_app(true);
        app.handle_key(KeyCode::Char('l'));
        app.handle_key(KeyCode::Char('j'));
        assert!(matches!(app.question_stop, Some(QStop::Option { .. })));
        let lines = footer_rows(&rendered_lines(&app, 140, 38, 100_000));
        assert!(lines.contains("Space"), "{lines}");
        assert!(!lines.contains("talk now"), "{lines}");
        assert!(!lines.to_lowercase().contains("tab"), "{lines}");

        let app = question_app(false);
        assert!(app.question_stop.is_none());
        let lines = footer_rows(&rendered_lines(&app, 140, 38, 100_000));
        assert!(lines.contains("talk now"), "{lines}");
        assert!(!lines.contains("Space"), "{lines}");
        assert!(!lines.to_lowercase().contains("tab"), "{lines}");
    }

    #[test]
    fn unavailable_workspace_says_ctrl_l() {
        let app = App::new(
            ConnectionState::Unavailable,
            snapshot(Vec::new(), Vec::new()),
            empty_search(),
            None,
        );
        let content = rendered(&app, 104, 28, 100_000);
        assert!(content.contains("Press Ctrl+L to retry now"));
        assert!(!content.contains("Press r"));
    }

    #[test]
    fn the_list_border_has_no_waiting_text() {
        for width in [100, 160] {
            let lines = rendered_lines(&waiting_app(), width, 38, 100_000);
            for gone in ["WAITING", "+1 more", "◀ h", "l ▶", "focus tracking off"] {
                assert!(!lines.contains(gone), "{gone} at {width}:\n{lines}");
            }
            for name in ["sre", "voice-mute-palette"] {
                let row = lines
                    .lines()
                    .find(|line| line.contains(name) && line.contains("IDLE"))
                    .unwrap_or_else(|| panic!("no {name} row at {width}:\n{lines}"));
                assert!(row.contains('◆'), "{row}");
            }
        }

        let lines = rendered_lines(&user_layout(), 140, 38, 100_000);
        let border = lines.lines().nth(row_with(&lines, "AGENT BLOCK OCCUPANCY"));
        assert!(
            border.unwrap().contains("(focus tracking off)"),
            "{border:?}"
        );
        assert!(!lines.contains("WAITING"), "{lines}");
    }

    /// The README's `## Keys` table gives one row per key of the scheme, in its order,
    /// and the old waiting line, `r` retry and `l` later are gone from the README.
    #[test]
    fn readme_key_table_matches_the_scheme() {
        let readme = include_str!("../README.md");
        let keys_section = readme
            .split("\n## Keys\n")
            .nth(1)
            .expect("README has a ## Keys section")
            .split("\n## ")
            .next()
            .unwrap_or_default();
        let table: Vec<&str> = keys_section
            .lines()
            .skip_while(|line| !line.starts_with('|'))
            .take_while(|line| line.starts_with('|'))
            .skip(2)
            .map(|line| line.split('|').nth(1).unwrap_or_default().trim())
            .collect();
        assert_eq!(
            table,
            [
                "`j` / `k`, ↓ / ↑",
                "`l` / →",
                "`h` / ←",
                "Space",
                "Enter",
                "Esc",
                "`n` / `N`",
                "`o`",
                "`g` / `G`",
                "`c` / `t`, `f`, `/`, `q`",
                "Ctrl+L, Ctrl+Q, Ctrl+C",
            ]
        );
        for gone in ["WAITING n", "`r` | Retry", "`l` | Later", "Press `r`"] {
            assert!(!readme.contains(gone), "README still holds {gone}");
        }
        // No Tab key row, no `s` row, no section of the old block, no summary.
        assert!(
            !table.iter().any(|key| key.contains("Tab") || *key == "`s`"),
            "the Keys table still has a Tab or s row"
        );
        let lower = readme.to_lowercase();
        for gone in [
            "question block",
            "summary screen",
            "send now",
            "send-now",
            "(s to mark)",
            "`tab`",
        ] {
            assert!(!lower.contains(gone), "README still describes {gone}");
        }
        // The table's header names the spec's columns.
        let header = keys_section
            .lines()
            .find(|line| line.starts_with('|'))
            .unwrap_or_default();
        assert_eq!(
            header.split('|').map(str::trim).collect::<Vec<_>>()[1..8],
            [
                "Key",
                "Agent row",
                "Todo header",
                "Option row",
                "Action row",
                "Step row",
                "Text field, search, palette, note",
            ]
        );
        // The Option row's cells for Space, Enter, h and l.
        let row_of = |key: &str| -> Vec<String> {
            keys_section
                .lines()
                .find(|line| line.starts_with(&format!("| {key} |")))
                .map(|line| line.split('|').map(|c| c.trim().to_string()).collect())
                .unwrap_or_default()
        };
        assert_eq!(
            row_of("Space")[4],
            "Pick the option; a free option opens its field"
        );
        assert_eq!(row_of("Enter")[4], "Nothing");
        assert_eq!(row_of("`h` / ←")[4], "Close the todo, cursor to its header");
        assert_eq!(row_of("`l` / →")[4], "Nothing");
        assert_eq!(row_of("Space")[6], "Tick the step, or untick it");
        // What the question rows' footers and held keys say is in the README.
        for says in [
            "▸ 2 todos waiting",
            "Esc back to the list (the questions are away)",
            "esc to continue",
            "▲",
            "▼",
            "· from the chief",
        ] {
            assert!(readme.contains(says), "README lacks {says}");
        }
    }

    // --- The question rows of the list ---

    /// Q1, with two decisions: three options (one free, one with no impact) and two.
    fn t1(intro: &str) -> crate::questions::QuestionView {
        use crate::questions::{DecisionView, OptionView, QuestionView};
        let option =
            |key: &str, label: &str, impact: &str, suggested: bool, free: bool| OptionView {
                key: key.into(),
                label: label.into(),
                impact: impact.into(),
                suggested,
                free,
            };
        QuestionView {
            id: "Q1".into(),
            product: "harold".into(),
            spec: "durable-server".into(),
            sha: "d84bf386a2a5".into(),
            link: "https://example.com/spec".into(),
            intro: intro.into(),
            version: 1,
            deferred: false,
            title: String::new(),
            steps: vec![],
            ticked: vec![],
            decisions: vec![
                DecisionView {
                    ask: "Which protocol should clients speak?".into(),
                    options: vec![
                        option(
                            "a",
                            "libSQL's Hrana",
                            "Existing libSQL clients work unchanged.",
                            true,
                            false,
                        ),
                        option("b", "Postgres wire", "", false, false),
                        option("z", "Something else", "", false, true),
                    ],
                },
                DecisionView {
                    ask: "Should the build run on every push?".into(),
                    options: vec![
                        option("y", "Yes", "", true, false),
                        option("n", "No", "", false, false),
                    ],
                },
            ],
        }
    }

    /// The layout of `user_layout` with `%3` named as the chief's pane and T1 listed
    /// with `waiting` todos waiting, closed.
    fn chief_questions(waiting: u32, intro: &str) -> App {
        let mut app = call_layout(chief_call(CallState::Off));
        app.selected = app.chief_row().map(|row| row.incarnation.clone());
        app.observe_questions(crate::questions::Queue {
            questions: (1..=waiting)
                .map(|n| {
                    let mut q = t1(intro);
                    if n > 1 {
                        q.id = format!("Q{n}");
                    }
                    q
                })
                .collect(),
            waiting,
            dismissed: Vec::new(),
        });
        app
    }

    fn keys(app: &mut App, text: &str) {
        for c in text.chars() {
            app.handle_key(KeyCode::Char(c));
        }
    }

    /// The chief with Q1 open and the cursor on its header.
    fn open_chief_question() -> App {
        let mut app = chief_questions(2, "The server needs one wire protocol.");
        keys(&mut app, "jl");
        assert!(app.questions.is_open() && app.cursor_on_question());
        app
    }

    /// What a screen line holds between the list's borders.
    fn inner(line: &str) -> &str {
        line.trim_start_matches('│').trim_end_matches('│').trim()
    }

    #[test]
    fn closed_row_under_chief_reads_n_questions_waiting_and_no_block_between_filter_and_list() {
        let app = chief_questions(2, "intro");
        let lines = text_lines(&app, 100, 60, 100_000);
        let chief = lines
            .iter()
            .position(|l| l.contains("sre") && l.contains("IDLE"))
            .expect("the chief's row");
        assert_eq!(inner(&lines[chief + 1]), "▸ 2 todos waiting");
        assert!(
            lines[12].contains("AGENT BLOCK OCCUPANCY"),
            "the list follows the filter at once:\n{}",
            lines.join("\n")
        );
        let one = chief_questions(1, "intro");
        assert!(rendered(&one, 100, 60, 100_000).contains("▸ 1 todo waiting"));
    }

    #[test]
    fn open_question_shows_blank_rows_between_intro_decisions_and_actions() {
        let lines = text_lines(&open_chief_question(), 100, 60, 100_000);
        let at = |needle: &str| {
            lines
                .iter()
                .position(|l| l.contains(needle))
                .unwrap_or_else(|| panic!("missing {needle:?}:\n{}", lines.join("\n")))
        };
        assert_eq!(
            inner(&lines[at("▾ Q1")]),
            "▶   ▾ Q1 · question · 1 of 2 todos · harold · durable-server"
        );
        assert_eq!(inner(&lines[at("The server needs") + 1]), "");
        assert_eq!(inner(&lines[at("Should the build") - 1]), "");
        assert_eq!(inner(&lines[at("send") - 1]), "");
    }

    #[test]
    fn option_marks_follow_picks() {
        let mut app = open_chief_question();
        let screen = rendered_lines(&app, 100, 60, 100_000);
        assert!(
            screen.contains("● a  libSQL's Hrana  (suggested)"),
            "{screen}"
        );
        assert!(screen.contains("○ b  Postgres wire"), "{screen}");
        assert!(screen.contains("● y  Yes  (suggested)"), "{screen}");
        keys(&mut app, "jj ");
        let screen = rendered_lines(&app, 100, 60, 100_000);
        assert!(screen.contains("● b  Postgres wire"), "{screen}");
        assert!(
            screen.contains("○ a  libSQL's Hrana  (suggested)"),
            "{screen}"
        );
    }

    #[test]
    fn impact_on_own_row_and_no_dash_without_impact() {
        let lines = text_lines(&open_chief_question(), 100, 60, 100_000);
        let a = lines.iter().position(|l| l.contains("● a")).unwrap();
        assert_eq!(
            inner(&lines[a + 1]),
            "Existing libSQL clients work unchanged."
        );
        let b = lines.iter().position(|l| l.contains("○ b")).unwrap();
        assert!(lines[b + 1].contains("○ z"), "no impact row after b");
        assert!(
            !lines[12..].iter().any(|l| l.contains('—')),
            "no dash in the list:\n{}",
            lines.join("\n")
        );
        let impact = style_at(
            &rendered_buffer(&open_chief_question(), 100, 60, 100_000),
            "Existing libSQL",
        );
        assert_eq!(impact.fg, Some(MUTED));
    }

    #[test]
    fn send_is_first_action_then_rule_then_later_revise_reject() {
        let lines = text_lines(&open_chief_question(), 100, 60, 100_000);
        let at = |needle: &str| {
            lines
                .iter()
                .position(|l| inner(l).ends_with(needle))
                .unwrap_or_else(|| panic!("missing {needle:?}:\n{}", lines.join("\n")))
        };
        let n = at("○ n  No");
        assert_eq!(inner(&lines[n + 1]), "");
        assert_eq!(at("send"), n + 2);
        assert_eq!(at("─────────────"), n + 3);
        assert_eq!(at("later"), n + 4);
        assert_eq!(at("revise…"), n + 5);
        assert_eq!(at("reject…"), n + 6);
    }

    #[test]
    fn no_summary_screen_or_send_now_row_drawn() {
        let mut app = open_chief_question();
        keys(&mut app, "jjjjjj");
        let screen = rendered_lines(&app, 100, 60, 100_000);
        for gone in ["send now", "picked:", "enter record"] {
            assert!(!screen.contains(gone), "{gone:?} drawn:\n{screen}");
        }
    }

    #[test]
    fn free_pick_shows_text_after_label_cut_to_fit() {
        let mut app = open_chief_question();
        keys(&mut app, "jjj ");
        assert!(app.questions.note_open());
        keys(&mut app, "my own protocol");
        let lines = text_lines(&app, 100, 60, 100_000);
        let z = lines
            .iter()
            .position(|l| l.contains("z  Something else"))
            .unwrap();
        assert_eq!(inner(&lines[z + 1]), "answer: my own protocol█");
        app.handle_key(KeyCode::Enter);
        let lines = text_lines(&app, 100, 60, 100_000);
        let z = lines
            .iter()
            .position(|l| l.contains("z  Something else"))
            .unwrap();
        assert!(inner(&lines[z]).contains("● z  Something else: my own protocol"));
        // Too long for a narrow list: cut with an ellipsis, on the one row.
        keys(&mut app, " ");
        keys(
            &mut app,
            " and a very long tail that cannot fit on the row at all",
        );
        app.handle_key(KeyCode::Enter);
        let lines = text_lines(&app, 60, 40, 100_000);
        let z = lines
            .iter()
            .position(|l| l.contains("z  Something else"))
            .unwrap();
        assert!(lines[z].contains('…'), "{}", lines[z]);
        assert!(lines[z + 1].contains("send") || inner(&lines[z + 1]).is_empty());
    }

    #[test]
    fn long_text_wraps_within_list_width() {
        let intro = "alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo lima mike november oscar papa quebec romeo sierra tango uniform victor whiskey xray yankee zulu";
        let app = {
            let mut app = chief_questions(2, intro);
            keys(&mut app, "jl");
            app
        };
        let lines = text_lines(&app, 60, 60, 100_000);
        let first = lines.iter().position(|l| l.contains("alpha")).unwrap();
        let last = lines.iter().position(|l| l.contains("zulu")).unwrap();
        assert!(
            last > first,
            "wrapped onto more rows:\n{}",
            lines.join("\n")
        );
        for line in &lines[first..=last] {
            assert!(line.starts_with('│') && line.ends_with('│'), "{line:?}");
        }
        let words: String = lines[first..=last]
            .iter()
            .map(|l| inner(l))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(words, intro);
    }

    #[test]
    fn filtered_out_chief_puts_rows_at_top_with_from_the_chief() {
        let mut app = chief_questions(2, "intro");
        keys(&mut app, "fmain");
        app.handle_key(KeyCode::Enter);
        assert!(
            app.visible_rows()
                .iter()
                .all(|row| row.incarnation.pane_id != "%3")
        );
        let lines = text_lines(&app, 100, 60, 100_000);
        assert_eq!(
            inner(&lines[14]),
            "▸ 2 todos waiting · from the chief",
            "{}",
            lines.join("\n")
        );
    }

    #[test]
    fn no_chief_pane_same() {
        let mut app = user_layout();
        app.observe_questions(crate::questions::Queue {
            questions: vec![t1("intro"), {
                let mut q = t1("intro");
                q.id = "Q2".into();
                q
            }],
            waiting: 2,
            dismissed: Vec::new(),
        });
        let lines = text_lines(&app, 100, 60, 100_000);
        assert_eq!(
            inner(&lines[14]),
            "▸ 2 todos waiting · from the chief",
            "{}",
            lines.join("\n")
        );
    }

    #[test]
    fn header_scrolled_off_shows_edge_marker() {
        // Cursor at the end of an open question: the header is above the view.
        let mut app = open_chief_question();
        // The second question's row follows `reject…`, so the cursor is walked there with fewer `j`.
        keys(&mut app, "jjjjjjj");
        keys(&mut app, "jj");
        let buffer = rendered_buffer(&app, 100, 24, 100_000);
        let screen = text_lines(&app, 100, 24, 100_000).join("\n");
        assert!(screen.contains("▲ 2 todos waiting"), "{screen}");
        assert_eq!(style_at(&buffer, "▲ 2 todos").fg, Some(Color::Yellow));
        assert!(
            screen
                .lines()
                .any(|l| l.contains('▶') && l.contains("reject…")),
            "{screen}"
        );
        // Cursor on the chief's row, the header just below the view.
        let mut app = chief_questions(2, "intro");
        keys(&mut app, "jlk");
        let buffer = rendered_buffer(&app, 100, 24, 100_000);
        let screen = text_lines(&app, 100, 24, 100_000).join("\n");
        assert!(screen.contains("▼ 2 todos waiting"), "{screen}");
        assert_eq!(style_at(&buffer, "▼ 2 todos").fg, Some(Color::Yellow));
        assert!(
            screen.lines().any(|l| l.contains('▶') && l.contains("sre")),
            "{screen}"
        );
    }

    #[test]
    fn edge_marker_never_hides_the_cursor_row_on_the_chief() {
        for (width, height) in [(100, 24), (100, 20), (100, 18), (60, 18)] {
            let mut app = chief_questions(2, "intro");
            keys(&mut app, "jlk");
            let screen = text_lines(&app, width, height, 100_000).join("\n");
            assert!(
                screen.lines().any(|l| l.contains('▶') && l.contains("sre")),
                "{width}x{height}:\n{screen}"
            );
        }
    }

    #[test]
    fn eighteen_row_terminal_keeps_cursor_row_in_view_with_six_list_rows() {
        let mut app = open_chief_question();
        app.observe_now(Some(NOW.into()));
        keys(&mut app, &"j".repeat(9));
        let lines = text_lines(&app, 60, 18, 100_000);
        let list: Vec<&String> = lines[9..15].iter().collect();
        assert!(
            list[0].contains("AGENT BLOCK OCCUPANCY"),
            "{}",
            lines.join("\n")
        );
        assert!(list[5].starts_with('└'), "{}", lines.join("\n"));
        assert!(
            list.iter()
                .any(|l| l.contains("▶") && l.contains("reject…")),
            "{}",
            lines.join("\n")
        );
        assert!(!lines.iter().any(|l| l.contains("Ship the backlog")));
    }

    #[test]
    fn cursor_marker_on_question_row_not_on_chief_row() {
        let mut app = open_chief_question();
        let lines = text_lines(&app, 100, 60, 100_000);
        assert!(!chief_line(&lines).contains('▶'));
        let header = lines.iter().find(|l| l.contains("▾ Q1")).unwrap();
        assert!(header.starts_with("│▶"), "{header}");
        let buffer = rendered_buffer(&app, 100, 60, 100_000);
        assert_eq!(style_at(&buffer, "▾ Q1").fg, Some(Color::Yellow));
        app.handle_key(KeyCode::Char('k'));
        let lines = text_lines(&app, 100, 60, 100_000);
        assert!(chief_line(&lines).contains('▶'));
        assert!(!lines.iter().any(|l| l.contains("▶") && l.contains("▾ Q1")));
    }

    #[test]
    fn detail_panel_shows_the_asker_while_cursor_on_question_rows() {
        let mut app = open_chief_question();
        keys(&mut app, "jj");
        let screen = rendered_lines(&app, 140, 40, 100_000);
        assert!(screen.contains("SELECTED SIGNAL"), "{screen}");
        assert!(screen.contains("home:0.3"), "{screen}");
    }

    #[test]
    fn notice_only_block_draws_one_coral_row_under_the_chief() {
        let mut app = call_layout(chief_call(CallState::Off));
        app.questions
            .set_notice(crate::questions::UNAVAILABLE_NOTICE.into());
        let lines = text_lines(&app, 100, 60, 100_000);
        let chief = lines
            .iter()
            .position(|l| l.contains("sre") && l.contains("IDLE"))
            .unwrap();
        assert_eq!(
            inner(&lines[chief + 1]),
            crate::questions::UNAVAILABLE_NOTICE
        );
        let buffer = rendered_buffer(&app, 100, 60, 100_000);
        assert_eq!(
            style_at(&buffer, "Harold can't save").fg,
            Some(super::CORAL)
        );
    }
    #[test]
    fn a_note_stays_visible_on_its_row_when_hidden_questions_return() {
        // Scenario A: the rows hide and come back unchanged under a half-typed note.
        let mut app = question_app(true);
        open_revise_note(&mut app);
        for character in "xyz".chars() {
            app.handle_key(KeyCode::Char(character));
        }
        app.hide_questions();
        app.observe_questions(q25_queue());
        assert_eq!(app.question_stop, Some(QStop::Action(2)));
        let lines = rendered_lines(&app, 140, 38, 100_000);
        row_with(&lines, "revise: xyz");
        // The note takes the keys, so j types and Enter sends it.
        app.handle_key(KeyCode::Char('j'));
        assert_eq!(app.question_stop, Some(QStop::Action(2)));
        row_with(&rendered_lines(&app, 140, 38, 100_000), "revise: xyzj");

        // Scenario B: Esc while the rows are away, then `g` onto the header.
        let mut app = question_app(true);
        open_revise_note(&mut app);
        for character in "xyz".chars() {
            app.handle_key(KeyCode::Char(character));
        }
        app.hide_questions();
        app.handle_key(KeyCode::Esc);
        app.handle_key(KeyCode::Char('g'));
        app.observe_questions(q25_queue());
        assert_eq!(app.question_stop, Some(QStop::Action(2)));
        row_with(&rendered_lines(&app, 140, 38, 100_000), "revise: xyz");
    }

    #[test]
    fn the_footer_names_only_the_keys_a_note_takes() {
        let mut app = question_app(true);
        open_revise_note(&mut app);
        for (width, height) in [(140, 38), (84, 30), (60, 18)] {
            let footer = footer_rows(&rendered_lines(&app, width, height, 100_000));
            assert!(footer.contains("Enter send"), "{footer}");
            assert!(footer.contains("Esc discard"), "{footer}");
            for gone in ["j/k", "close", "Enter close"] {
                assert!(!footer.contains(gone), "{gone} in {footer}");
            }
        }
        let long = super::footer_hints(
            false,
            Some(QStop::Option {
                decision: 0,
                option: 0,
            }),
            true,
            Some(crate::questions::NoteKind::FreeText),
            true,
            false,
        )
        .iter()
        .map(|s| s.content.as_ref())
        .collect::<String>();
        assert!(
            long.contains("Enter keep") && long.contains("Esc discard"),
            "{long}"
        );
        assert!(!long.contains("j/k"), "{long}");
    }

    // --- Todos: the mockups of the spec ---

    fn step(text: &str, detail: &str) -> crate::questions::StepView {
        crate::questions::StepView {
            text: text.into(),
            detail: detail.into(),
        }
    }

    /// I1 of the spec's mockups: four steps, two with a detail.
    fn i1(ticked: &[u32]) -> crate::questions::QuestionView {
        crate::questions::QuestionView {
            id: "I1".into(),
            product: "harold".into(),
            spec: String::new(),
            sha: String::new(),
            link: "https://example.com/i1".into(),
            intro: "Do these on PasM1Mac, then tick each one here.".into(),
            version: 1,
            deferred: false,
            decisions: vec![],
            title: "Install Harold 0.15.0 on PasM1Mac".into(),
            steps: vec![
                step("Open Terminal", ""),
                step(
                    "Run make deploy in ~/Dev/p/harold/main",
                    "It asks for your password once.",
                ),
                step(
                    "Turn on harold in System Settings \u{203a} Privacy & Security \u{203a} Accessibility",
                    "harold shows in the list after its first start.",
                ),
                step("Run harold status and check it says running 0.15.0", ""),
            ],
            ticked: ticked.to_vec(),
        }
    }

    /// Q1 of the spec's mockups: one decision with two options.
    fn q1_single() -> crate::questions::QuestionView {
        use crate::questions::{DecisionView, OptionView};
        let mut q = t1("The server needs one wire protocol for its clients.");
        q.decisions = vec![DecisionView {
            ask: "Which protocol should clients speak?".into(),
            options: vec![
                OptionView {
                    key: "a".into(),
                    label: "libSQL's Hrana".into(),
                    impact: "Existing libSQL clients work unchanged.".into(),
                    suggested: true,
                    free: false,
                },
                OptionView {
                    key: "b".into(),
                    label: "Postgres wire".into(),
                    impact: "Any Postgres client connects.".into(),
                    suggested: false,
                    free: false,
                },
            ],
        }];
        q
    }

    fn todo_app(questions: Vec<crate::questions::QuestionView>) -> App {
        let mut app = call_layout(chief_call(CallState::Off));
        app.selected = app.chief_row().map(|row| row.incarnation.clone());
        let waiting = questions.len() as u32;
        app.observe_questions(crate::questions::Queue {
            questions,
            waiting,
            dismissed: Vec::new(),
        });
        app
    }

    /// I1 then Q1, closed, the cursor on the chief.
    fn closed_todos() -> App {
        todo_app(vec![i1(&[]), q1_single()])
    }

    /// I1 open with `ticked`, the cursor on its header.
    fn open_i1(ticked: &[u32]) -> App {
        let mut app = todo_app(vec![i1(ticked), q1_single()]);
        keys(&mut app, "jl");
        assert!(app.questions.is_open() && app.cursor_on_question());
        app
    }

    /// The todo's rows as the mockups draw them: what follows the list's left border,
    /// from the row holding `first` for `count` rows, trailing spaces cut.
    fn todo_block(app: &App, width: u16, first: &str, count: usize) -> Vec<String> {
        let lines = text_lines(app, width, 60, 100_000);
        let at = lines
            .iter()
            .position(|l| l.contains(first))
            .unwrap_or_else(|| panic!("missing {first:?}:\n{}", lines.join("\n")));
        lines[at..at + count]
            .iter()
            .map(|l| {
                l.trim_start_matches('\u{2502}')
                    .trim_end_matches('\u{2502}')
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    fn footer_inner(app: &App, width: u16, height: u16) -> String {
        let lines = text_lines(app, width, height, 100_000);
        lines[lines.len() - 3..]
            .iter()
            .filter(|l| !l.starts_with('\u{250c}') && !l.starts_with('\u{2514}'))
            .map(|l| inner(l).to_string())
            .collect::<Vec<_>>()
            .join(" / ")
    }

    const MOCK_I1_STEPS: [&str; 12] = [
        "        Do these on PasM1Mac, then tick each one here.",
        "",
        "          [ ] 1  Open Terminal",
        "          [ ] 2  Run make deploy in ~/Dev/p/harold/main",
        "                 It asks for your password once.",
        "          [ ] 3  Turn on harold in System Settings \u{203a} Privacy & Security \u{203a} Accessibility",
        "                 harold shows in the list after its first start.",
        "          [ ] 4  Run harold status and check it says running 0.15.0",
        "",
        "          done",
        "          \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}",
        "          ignore",
    ];

    #[test]
    fn mockup_1_closed_row_counts_both_kinds() {
        let mut app = closed_todos();
        let lines = text_lines(&app, 107, 40, 100_000);
        let chief = lines
            .iter()
            .position(|l| l.contains("sre") && l.contains("IDLE"))
            .unwrap();
        assert_eq!(inner(&lines[chief + 1]), "\u{25b8} 2 todos waiting");
        keys(&mut app, "j");
        assert_eq!(
            footer_inner(&app, 107, 40),
            "TODO  l open  j/k move  n/N todo  ^L retry  ^Q quit"
        );
    }

    #[test]
    fn mockup_2_instructions_open_with_the_cursor_on_the_header() {
        let app = open_i1(&[]);
        let rows = todo_block(&app, 107, "\u{25be} I1", 15);
        assert_eq!(
            rows[0],
            "\u{25b6}   \u{25be} I1 \u{b7} instructions \u{b7} 1 of 2 todos \u{b7} 0 of 4 ticked \u{b7} harold \u{b7} Install Harold 0.15.0 on PasM1Mac"
        );
        assert_eq!(rows[1..13], MOCK_I1_STEPS);
        assert_eq!(
            rows[13],
            "    \u{25b8} Q1 \u{b7} question \u{b7} harold \u{b7} durable-server"
        );
        assert_eq!(
            footer_inner(&app, 107, 40),
            "TODO  h close  j/k move  n/N todo  o link  Esc close  ^L retry  ^Q quit"
        );
        let buffer = rendered_buffer(&app, 107, 40, 100_000);
        assert_eq!(style_at(&buffer, "\u{25be} I1").fg, Some(Color::Yellow));
        assert!(
            style_at(&buffer, "It asks for your password")
                .add_modifier
                .contains(ratatui::style::Modifier::DIM)
        );
        assert_eq!(
            style_at(&buffer, "\u{25b8} Q1").fg,
            Some(Color::Yellow),
            "other todo rows read like headers"
        );
    }

    #[test]
    fn mockup_3_partly_ticked_with_the_cursor_on_step_3() {
        let mut app = open_i1(&[1, 2]);
        keys(&mut app, "jjj");
        let rows = todo_block(&app, 107, "\u{25be} I1", 11);
        assert!(
            rows[0]
                .ends_with("2 of 4 ticked \u{b7} harold \u{b7} Install Harold 0.15.0 on PasM1Mac")
        );
        assert!(rows[0].starts_with("    \u{25be} I1"), "{}", rows[0]);
        assert_eq!(rows[3], "          [\u{2713}] 1  Open Terminal");
        assert_eq!(
            rows[4],
            "          [\u{2713}] 2  Run make deploy in ~/Dev/p/harold/main"
        );
        assert_eq!(
            rows[6],
            "\u{25b6}         [ ] 3  Turn on harold in System Settings \u{203a} Privacy & Security \u{203a} Accessibility"
        );
        assert_eq!(
            footer_inner(&app, 107, 40),
            "TODO  Space tick  h close  j/k move  n/N todo  o link  Esc close  ^L retry  ^Q quit"
        );
        let buffer = rendered_buffer(&app, 107, 40, 100_000);
        assert_eq!(style_at(&buffer, "[ ] 3").fg, Some(Color::Yellow));
    }

    #[test]
    fn mockup_4_every_step_ticked_points_to_done() {
        let mut app = open_i1(&[1, 2, 3, 4]);
        keys(&mut app, "jjjjj");
        let rows = todo_block(&app, 107, "\u{25be} I1", 17);
        assert!(rows[0].contains("4 of 4 ticked"));
        assert_eq!(rows[9], "");
        assert_eq!(
            rows[10],
            "        \u{2713} All 4 ticked. Press done to tell the chief."
        );
        assert_eq!(rows[11], "");
        assert_eq!(rows[12], "\u{25b6}         done");
        assert_eq!(rows[14], "          ignore");
        assert_eq!(
            footer_inner(&app, 107, 40),
            "TODO  Enter done  h close  j/k move  n/N todo  o link  Esc close  ^L retry  ^Q quit"
        );
        let buffer = rendered_buffer(&app, 107, 40, 100_000);
        assert_eq!(style_at(&buffer, "All 4 ticked").fg, Some(super::INK));
        assert_eq!(
            style_at(&buffer, "\u{25b6}         done").fg,
            Some(Color::Yellow)
        );
        keys(&mut app, "jj");
        assert_eq!(
            footer_inner(&app, 107, 40),
            "TODO  l open  j/k move  n/N todo  ^L retry  ^Q quit"
        );
        keys(&mut app, "k");
        assert!(footer_inner(&app, 107, 40).starts_with("TODO  Enter ignore  h close"));
    }

    #[test]
    fn mockup_5_the_question_after_done_has_one_of_one_todo() {
        let app = {
            let mut app = todo_app(vec![q1_single()]);
            keys(&mut app, "jl");
            app
        };
        let rows = todo_block(&app, 107, "\u{25be} Q1", 13);
        assert_eq!(
            rows[0],
            "\u{25b6}   \u{25be} Q1 \u{b7} question \u{b7} 1 of 1 todo \u{b7} harold \u{b7} durable-server"
        );
        assert_eq!(
            rows[1],
            "        The server needs one wire protocol for its clients."
        );
        assert_eq!(rows[3], "        Which protocol should clients speak?");
        assert_eq!(rows[4], "          \u{25cf} a  libSQL's Hrana  (suggested)");
        assert_eq!(rows[9], "          send");
        assert_eq!(
            footer_inner(&app, 107, 40),
            "TODO  h close  j/k move  n/N todo  o link  Esc close  ^L retry  ^Q quit"
        );
    }

    #[test]
    fn question_footers_by_stop_in_both_layouts() {
        let mut app = todo_app(vec![q1_single(), i1(&[])]);
        keys(&mut app, "jl");
        keys(&mut app, "j");
        assert!(footer_inner(&app, 107, 40).starts_with("TODO  Space pick  h close  j/k move"));
        keys(&mut app, "jjj");
        assert!(footer_inner(&app, 107, 40).starts_with("TODO  Enter choose  h close  j/k move"));
        for compact in [false, true] {
            let text = hint_text(compact, Some(QStop::Step(0)), true);
            assert_eq!(
                text,
                " TODO  Space tick  h close  j/k move  n/N todo  o link  Esc close  ^L retry  ^Q quit "
            );
            let no_link: String =
                super::footer_hints(compact, Some(QStop::Header), true, None, false, false)
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect();
            assert!(!no_link.contains("o link"), "{no_link}");
        }
    }

    #[test]
    fn instructions_footer_wraps_at_60_columns_with_both_halves() {
        let mut app = open_i1(&[1, 2]);
        keys(&mut app, "jjj");
        let footer = footer_inner(&app, 60, 18);
        assert!(footer.contains("TODO  Space tick"), "{footer}");
        assert!(footer.contains("^Q quit"), "{footer}");
    }

    #[test]
    fn todo_states_fit_60x18_and_keep_the_cursor_row_in_view() {
        for (ticked, moves, want) in [
            (vec![], "", "I1"),
            (vec![1, 2], "jjj", "[ ] 3"),
            (vec![1, 2, 3, 4], "jjjjj", "done"),
            (vec![1, 2, 3, 4], "jjjjjjjj", "Q1"),
        ] {
            let mut app = open_i1(&ticked);
            keys(&mut app, moves);
            let screen = text_lines(&app, 60, 18, 100_000);
            assert!(
                screen
                    .iter()
                    .any(|l| l.contains('\u{25b6}') && l.contains(want)),
                "{want}:\n{}",
                screen.join("\n")
            );
        }
        let app = closed_todos();
        let screen = text_lines(&app, 60, 18, 100_000).join("\n");
        assert!(screen.contains("2 todos waiting"), "{screen}");
    }

    #[test]
    fn edge_markers_count_todos_for_instructions() {
        let mut app = open_i1(&[1, 2, 3, 4]);
        keys(&mut app, "jjjjjjj");
        let screen = text_lines(&app, 60, 18, 100_000).join("\n");
        assert!(screen.contains("\u{25b2} 2 todos waiting"), "{screen}");
    }

    #[test]
    fn kind_is_named_in_every_header_and_row() {
        let app = open_i1(&[]);
        let screen = text_lines(&app, 107, 60, 100_000).join("\n");
        assert!(screen.contains("I1 \u{b7} instructions \u{b7}"), "{screen}");
        assert!(screen.contains("Q1 \u{b7} question \u{b7}"), "{screen}");
        let q = open_chief_question();
        let screen = text_lines(&q, 107, 60, 100_000).join("\n");
        assert!(
            screen.contains("Q1 \u{b7} question \u{b7} 1 of 2 todos"),
            "{screen}"
        );
        assert!(
            screen.contains("\u{25b8} Q2 \u{b7} question \u{b7} harold \u{b7} durable-server"),
            "{screen}"
        );
    }

    #[test]
    fn filtered_out_chief_reads_todos_from_the_chief() {
        let mut app = closed_todos();
        keys(&mut app, "fmain");
        app.handle_key(KeyCode::Enter);
        let lines = text_lines(&app, 100, 60, 100_000);
        assert_eq!(
            inner(&lines[14]),
            "\u{25b8} 2 todos waiting \u{b7} from the chief"
        );
    }
}
