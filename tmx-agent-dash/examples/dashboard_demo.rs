use std::io::{self, stdout};
use std::time::Duration;

use crossterm::cursor::{Hide, Show};
use crossterm::event::{self, Event, KeyCode};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tmx_agent_dash::app::{
    AgentIncarnation, AgentRow, AgentState, App, ConnectionState, Effect, MonitorHealth,
    MonitorHealthState, SearchState, Snapshot,
};
use tmx_agent_dash::ui;

const NOW_MS: i64 = 1_777_000_000_000;

fn main() -> io::Result<()> {
    enable_raw_mode()?;
    let restore = TerminalRestore;
    let mut output = stdout();
    execute!(output, EnterAlternateScreen, Hide)?;
    let backend = CrosstermBackend::new(output);
    let mut terminal = Terminal::new(backend)?;
    let mut app = demo_app();

    loop {
        terminal.draw(|frame| ui::render(frame, &app, NOW_MS))?;
        if event::poll(Duration::from_millis(100))? {
            let Event::Key(key) = event::read()? else {
                continue;
            };
            if handle_demo_key(&mut app, key.code) {
                break;
            }
        }
    }

    drop(terminal);
    drop(restore);
    Ok(())
}

fn handle_demo_key(app: &mut App, code: KeyCode) -> bool {
    matches!(app.handle_key(code), Effect::Quit)
}

struct TerminalRestore;

impl Drop for TerminalRestore {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(stdout(), Show, LeaveAlternateScreen);
    }
}

fn demo_app() -> App {
    let selected = incarnation("%17", 91700, 91844, "codex");
    App::new(
        ConnectionState::Live,
        Snapshot {
            through_event_version: 1842,
            server_time_ms: NOW_MS,
            monitor_health: vec![
                MonitorHealth {
                    component: "inventory".into(),
                    state: MonitorHealthState::Degraded,
                    reason_code: "tmux_unavailable".into(),
                    observed_at_ms: NOW_MS - 1_000,
                },
                MonitorHealth {
                    component: "classifier".into(),
                    state: MonitorHealthState::Healthy,
                    reason_code: "ok".into(),
                    observed_at_ms: NOW_MS - 500,
                },
            ],
            rows: vec![
                row(
                    selected.clone(),
                    AgentState::Busy,
                    "Codex",
                    "tmx-agent-dash:0.1",
                    "/Users/kahgeh/Dev/p/tmx-agent-dash",
                    "Build event snapshot dashboard",
                    NOW_MS - 8_000,
                ),
                row(
                    incarnation("%22", 92000, 92100, "claude"),
                    AgentState::Idle,
                    "Claude",
                    "harold:2.1",
                    "/Users/kahgeh/Dev/p/harold",
                    "Review event projection contract",
                    NOW_MS - 30_000,
                ),
                row(
                    incarnation("%31", 93000, 93100, "opencode"),
                    AgentState::Unknown,
                    "OpenCode",
                    "lab:1.0",
                    "/Users/kahgeh/Dev/p/lab",
                    "Awaiting assignment",
                    NOW_MS - 100_000,
                ),
            ],
        },
        SearchState {
            query: "event".into(),
            editing: true,
        },
        Some(selected),
    )
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
        agent_started_at_ms: NOW_MS - 1_000_000 + i64::from(agent_pid),
        provider_id: provider_id.into(),
    }
}

#[allow(clippy::too_many_arguments)]
fn row(
    incarnation: AgentIncarnation,
    state: AgentState,
    provider_display_name: &str,
    tmux_target: &str,
    working_directory: &str,
    work_summary: &str,
    last_transition_at_ms: i64,
) -> AgentRow {
    AgentRow {
        incarnation,
        provider_display_name: provider_display_name.into(),
        tmux_target: tmux_target.into(),
        session_name: tmux_target.split(':').next().unwrap_or_default().into(),
        window_index: 0,
        pane_index: 0,
        working_directory: working_directory.into(),
        work_summary: Some(work_summary.into()),
        state,
        last_transition_at_ms,
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyCode;

    use super::{demo_app, handle_demo_key};

    #[test]
    fn escape_clears_the_demo_search_without_quitting() {
        let mut app = demo_app();

        assert!(!handle_demo_key(&mut app, KeyCode::Esc));
        assert!(app.search.query.is_empty());
        assert!(!app.search.editing);
    }

    #[test]
    fn q_is_the_demo_quit_key_outside_search_editing() {
        let mut app = demo_app();
        assert!(!handle_demo_key(&mut app, KeyCode::Esc));

        assert!(handle_demo_key(&mut app, KeyCode::Char('q')));
    }
}
