use std::io;
use std::sync::{Arc, Mutex};

use crate::settings::AgentProviderSettings;

use super::domain::{AgentIncarnation, AgentPaneObservation, ObservedAgentState};
use super::screen::{
    CommandOutput, CommandRunner, ScreenError, TmuxVisibleScreen, VisibleScreenPort,
    observe_visible_text,
};

fn pane(pane_id: &str) -> AgentPaneObservation {
    AgentPaneObservation {
        incarnation: AgentIncarnation {
            pane_id: pane_id.to_string(),
            pane_pid: 10,
            agent_pid: 20,
            agent_started_at_ms: 1_000,
            provider_id: "codex".to_string(),
        },
        tmux_target: "harold:2.1".to_string(),
        session_name: "harold".to_string(),
        window_index: 2,
        pane_index: 1,
        working_directory: "/work/harold".to_string(),
        provider_display_name: "Codex".to_string(),
        observed_at_ms: 50,
    }
}

fn provider() -> AgentProviderSettings {
    AgentProviderSettings {
        id: "codex".to_string(),
        display_name: "Codex".to_string(),
        command_contains: vec!["codex".to_string()],
        busy_all: vec!["Working".to_string(), "esc to interrupt".to_string()],
        idle_all: vec!["Ready".to_string()],
        summary_line_prefixes: vec!["›".to_string(), ">".to_string()],
    }
}

#[test]
fn state_and_fallback_summary_are_independent_optional_facts() {
    let cases = [
        (
            "Working\nesc to interrupt\nReady",
            Some(ObservedAgentState::Busy),
            None,
        ),
        ("Ready", Some(ObservedAgentState::Idle), None),
        (
            "Working\nesc to interrupt",
            Some(ObservedAgentState::Busy),
            None,
        ),
        (
            "status unknown\n› Review projector",
            None,
            Some("Review projector"),
        ),
        (
            "Working\nesc to interrupt\n› Implement stream",
            Some(ObservedAgentState::Busy),
            Some("Implement stream"),
        ),
        ("status unknown", None, None),
        ("Working only", None, None),
    ];

    for (visible, expected_state, expected_summary) in cases {
        let observation = observe_visible_text(&pane("%7"), &provider(), visible, 99);
        assert_eq!(observation.state, expected_state);
        assert_eq!(observation.fallback_summary.as_deref(), expected_summary);
    }
}

#[test]
fn extractor_uses_sanitized_bottom_most_non_empty_prefixed_line() {
    let visible = concat!(
        "\u{1b}[32mWorking\u{1b}[0m\r\n",
        "esc to interrupt\r\n",
        "› First task\r\n",
        "› Unicode 🦀 task\u{1b}]0;window title\u{7}\r\n",
        "› \t\r\n",
    );

    let observation = observe_visible_text(&pane("%7"), &provider(), visible, 99);

    assert_eq!(observation.state, Some(ObservedAgentState::Busy));
    assert_eq!(
        observation.fallback_summary.as_deref(),
        Some("Unicode 🦀 task")
    );
}

#[test]
fn extracted_summary_is_capped_at_160_unicode_scalars() {
    let expected = "🦀".repeat(160);
    let visible = format!("› {expected}extra");

    let observation = observe_visible_text(&pane("%7"), &provider(), &visible, 99);

    assert_eq!(observation.fallback_summary, Some(expected));
}

#[test]
fn unrelated_screen_secret_never_enters_the_typed_observation() {
    let visible = "TOP_SECRET_UNRELATED\nWorking\nesc to interrupt\n› Safe task";

    let observation = observe_visible_text(&pane("%7"), &provider(), visible, 99);
    let debug = format!("{observation:?}");

    assert_eq!(observation.fallback_summary.as_deref(), Some("Safe task"));
    assert!(!debug.contains("TOP_SECRET_UNRELATED"));
}

enum FakeResult {
    Output { success: bool, stdout: Vec<u8> },
    Error(io::ErrorKind),
}

type CommandCall = (String, Vec<String>);
type RecordedCalls = Arc<Mutex<Vec<CommandCall>>>;

struct FakeRunner {
    result: Mutex<Option<FakeResult>>,
    calls: RecordedCalls,
}

impl FakeRunner {
    fn output(success: bool, stdout: &str) -> Self {
        Self {
            result: Mutex::new(Some(FakeResult::Output {
                success,
                stdout: stdout.as_bytes().to_vec(),
            })),
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn error(kind: io::ErrorKind) -> Self {
        Self {
            result: Mutex::new(Some(FakeResult::Error(kind))),
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl CommandRunner for FakeRunner {
    fn output(&self, program: &str, args: &[&str]) -> io::Result<CommandOutput> {
        self.calls.lock().unwrap().push((
            program.to_string(),
            args.iter().map(|arg| (*arg).to_string()).collect(),
        ));
        match self.result.lock().unwrap().take().unwrap() {
            FakeResult::Output { success, stdout } => Ok(CommandOutput { success, stdout }),
            FakeResult::Error(kind) => Err(io::Error::from(kind)),
        }
    }
}

#[test]
fn capture_uses_only_the_current_grid_with_literal_pane_argument() {
    let runner = FakeRunner::output(true, "Ready\n› Review tests");
    let calls = Arc::clone(&runner.calls);
    let screen = TmuxVisibleScreen::with_runner(runner, || 99);
    let hostile_pane_id = "%7; capture-pane -S -100";

    let observation = screen.observe(&pane(hostile_pane_id), &provider()).unwrap();

    assert_eq!(observation.observed_at_ms, 99);
    assert_eq!(observation.state, Some(ObservedAgentState::Idle));
    assert_eq!(
        observation.fallback_summary.as_deref(),
        Some("Review tests")
    );
    assert_eq!(
        calls.lock().unwrap().as_slice(),
        [(
            "tmux".to_string(),
            vec![
                "capture-pane".to_string(),
                "-p".to_string(),
                "-S".to_string(),
                "0".to_string(),
                "-t".to_string(),
                hostile_pane_id.to_string(),
            ],
        )]
    );
}

#[test]
fn capture_errors_are_bounded_units_without_command_output() {
    let unavailable =
        TmuxVisibleScreen::with_runner(FakeRunner::error(io::ErrorKind::NotFound), || 99);
    assert_eq!(
        unavailable.observe(&pane("%7"), &provider()),
        Err(ScreenError::CaptureUnavailable)
    );

    let failed = TmuxVisibleScreen::with_runner(
        FakeRunner::output(false, "TOP_SECRET_COMMAND_OUTPUT"),
        || 99,
    );
    let error = failed.observe(&pane("%7"), &provider()).unwrap_err();
    assert_eq!(error, ScreenError::CaptureFailed);
    assert!(!format!("{error:?}").contains("TOP_SECRET_COMMAND_OUTPUT"));
}
