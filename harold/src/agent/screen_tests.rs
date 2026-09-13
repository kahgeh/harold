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
        screen_adapter: crate::settings::ScreenAdapter::GenericV1,
        screen_history_lines: 2000,
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
fn extractor_skips_codex_idle_placeholder_below_the_real_prompt() {
    let mut codex = provider();
    codex.idle_all = vec!["Ask Codex to do anything".to_string()];
    let visible = concat!(
        "› E2E-CODEX inspect the dashboard state transitions\n",
        "\n",
        "• Completed the requested inspection.\n",
        "\n",
        "› Ask Codex to do anything\n",
    );

    let observation = observe_visible_text(&pane("%34"), &codex, visible, 99);

    assert_eq!(observation.state, Some(ObservedAgentState::Idle));
    assert_eq!(
        observation.fallback_summary.as_deref(),
        Some("E2E-CODEX inspect the dashboard state transitions")
    );
}

#[test]
fn extractor_returns_no_summary_for_only_the_codex_idle_placeholder() {
    let mut codex = provider();
    codex.idle_all = vec!["Ask Codex to do anything".to_string()];

    let observation = observe_visible_text(
        &pane("%34"),
        &codex,
        "Codex ready\n› Ask Codex to do anything\n",
        99,
    );

    assert_eq!(observation.state, Some(ObservedAgentState::Idle));
    assert_eq!(observation.fallback_summary, None);
}

#[test]
fn extractor_keeps_a_prompt_that_mentions_the_codex_idle_placeholder() {
    let mut codex = provider();
    codex.idle_all = vec!["Ask Codex to do anything".to_string()];
    let prompt = "Explain why the UI says Ask Codex to do anything";

    let observation = observe_visible_text(
        &pane("%34"),
        &codex,
        &format!("Codex ready\n› {prompt}\n"),
        99,
    );

    assert_eq!(observation.state, Some(ObservedAgentState::Idle));
    assert_eq!(observation.fallback_summary.as_deref(), Some(prompt));
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
    assert_eq!(observation.fallback_summary, None);
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

#[test]
fn history_capture_preserves_styles_and_uses_only_the_configured_tail() {
    let runner = FakeRunner::output(true, "> Safe task\n");
    let calls = Arc::clone(&runner.calls);
    let screen = TmuxVisibleScreen::with_runner(runner, || 99);
    let scan = screen.scan_prompts(&pane("%7"), &provider()).unwrap();
    assert_eq!(scan.blocks.len(), 1);
    assert_eq!(scan.blocks[0].candidate.as_deref(), Some("Safe task"));
    assert_eq!(
        calls.lock().unwrap().as_slice(),
        [(
            "tmux".to_string(),
            ["capture-pane", "-p", "-e", "-S", "-2000", "-t", "%7"]
                .map(str::to_string)
                .to_vec()
        )]
    );
}

fn codex_scan(text: &str) -> super::screen::PromptScan {
    let mut config = provider();
    config.screen_adapter = crate::settings::ScreenAdapter::CodexV1;
    config.idle_all = vec!["Ask Codex to do anything".into()];
    TmuxVisibleScreen::with_runner(FakeRunner::output(true, text), || 99)
        .scan_prompts(&pane("%7"), &config)
        .unwrap()
}

#[test]
fn codex_current_styled_submission_excludes_normal_text_unsent_composer() {
    // Codex 0.153.4 live capture: submitted separator precedes reset; composer follows it.
    let scan = codex_scan(concat!(
        "\x1b[1;2m› \x1b[0mFix the retry loop.\n\n",
        "\x1b[2m• \x1b[0mReady.\n\n",
        "\x1b[1m›\x1b[0m UNSENT-DRAFT: Delete the database.\n",
    ));
    assert_eq!(scan.blocks.len(), 1);
    assert_eq!(
        scan.blocks[0].candidate.as_deref(),
        Some("Fix the retry loop.")
    );
}

#[test]
fn codex_reconstructs_wrapped_submission_excluding_following_assistant() {
    let scan = codex_scan(concat!(
        "\x1b[1;2m› \x1b[0mReview the stream\n",
        "  and test repeated submissions\n",
        "  across process restarts.\n\n",
        "\x1b[2m• \x1b[0mASSISTANT_SECRET\n",
    ));
    assert_eq!(scan.blocks.len(), 1);
    assert_eq!(
        scan.blocks[0].candidate.as_deref(),
        Some("Review the stream and test repeated submissions across process restarts.")
    );
}

#[test]
fn codex_keeps_placeholder_anchors_and_repeated_full_block_fingerprints() {
    let scan = codex_scan(concat!(
        "\x1b[1;2m› \x1b[0mAsk Codex to do anything\n\n",
        "\x1b[1;2m> \x1b[0mExplain Ask Codex to do anything\n\n",
        "\x1b[1;2m> \x1b[0mExplain Ask Codex to do anything\n\n",
        "\x1b[1m›\x1b[0m \x1b[2mAsk Codex to do anything\x1b[0m\n",
    ));
    assert_eq!(scan.blocks.len(), 3);
    assert_eq!(scan.blocks[0].candidate, None);
    assert_eq!(
        scan.blocks[1].candidate.as_deref(),
        Some("Explain Ask Codex to do anything")
    );
    assert_eq!(scan.blocks[1].fingerprint, scan.blocks[2].fingerprint);
}

#[test]
fn codex_rejects_unstyled_and_dim_candidates_and_malformed_controls() {
    for text in [
        "› Unstyled ambiguous row\n",
        "\x1b[1m›\x1b[0m Draft\n",
        "\x1b[1m› \x1b[0;2mDim draft\n",
        "\x1b[1;2m› \x1b[0mUnsafe \x1b[31",
        "\x1b[1;2m› \x1b[0mUnsafe \u{9b}broken",
        "\x1b]0;\x1b[1;2m› \x1b[0mHidden title\x07\n",
        "• assistant\n  > shell command\n",
    ] {
        assert!(
            codex_scan(text).blocks.is_empty(),
            "ambiguous fixture was accepted"
        );
    }
}

#[test]
fn codex_scans_deep_tail_and_caps_candidate_but_not_fingerprint() {
    let prefix = "🦀".repeat(160);
    let text = format!(
        "\x1b[1;2m› \x1b[0m{prefix}one\n\n{}\n\x1b[1m›\x1b[0m Draft",
        "• output\n".repeat(1990)
    );
    let scan = codex_scan(&text);
    assert_eq!(scan.blocks.len(), 1);
    assert_eq!(scan.blocks[0].candidate.as_deref(), Some(prefix.as_str()));
    let other = codex_scan(&format!("\x1b[1;2m› \x1b[0m{prefix}two\n"));
    assert_ne!(scan.blocks[0].fingerprint, other.blocks[0].fingerprint);
}

#[test]
fn generic_adapter_preserves_safe_prefixes_without_provider_id_magic() {
    let mut config = provider();
    config.id = "opencode".into();
    config.summary_line_prefixes.clear();
    let capture = "Ready\n› User-looking draft\n> tool output";
    let scan = TmuxVisibleScreen::with_runner(FakeRunner::output(true, capture), || 99)
        .scan_prompts(&pane("%7"), &config)
        .unwrap();
    assert!(scan.blocks.is_empty());
    config.id = "custom".into();
    config.summary_line_prefixes = vec!["Task:".into()];
    let scan = TmuxVisibleScreen::with_runner(
        FakeRunner::output(true, "Task: Review tests\nTask: Ready"),
        || 99,
    )
    .scan_prompts(&pane("%7"), &config)
    .unwrap();
    assert_eq!(scan.blocks.len(), 2);
    assert_eq!(scan.blocks[0].candidate.as_deref(), Some("Review tests"));
    assert_eq!(scan.blocks[1].candidate, None);
}

#[test]
fn history_capture_failure_and_oversize_return_only_bounded_error() {
    for runner in [
        FakeRunner::output(false, "SECRET"),
        FakeRunner::output(true, &"x".repeat(4 * 1024 * 1024 + 1)),
    ] {
        let screen = TmuxVisibleScreen::with_runner(runner, || 99);
        assert!(matches!(
            screen.scan_prompts(&pane("%7"), &provider()),
            Err(ScreenError::CaptureFailed)
        ));
    }
}

#[test]
fn history_capture_honors_nondefault_limit_and_rejects_invalid_direct_calls() {
    let mut config = provider();
    config.screen_history_lines = 17;
    let runner = FakeRunner::output(true, "");
    let calls = Arc::clone(&runner.calls);
    TmuxVisibleScreen::with_runner(runner, || 99)
        .scan_prompts(&pane("%7"), &config)
        .unwrap();
    assert_eq!(calls.lock().unwrap()[0].1[4], "-17");
    config.screen_history_lines = 0;
    let runner = FakeRunner::output(true, "");
    let calls = Arc::clone(&runner.calls);
    assert!(
        TmuxVisibleScreen::with_runner(runner, || 99)
            .scan_prompts(&pane("%7"), &config)
            .is_err()
    );
    assert!(calls.lock().unwrap().is_empty());
}

#[test]
fn codex_c1_styles_are_supported_without_treating_color_channels_as_attributes() {
    let scan = codex_scan("\u{9b}1;2m› \u{9b}0;38;2;1;2;0mSafe task\u{9b}0m\n");
    assert_eq!(scan.blocks.len(), 1);
    assert_eq!(scan.blocks[0].candidate.as_deref(), Some("Safe task"));
}

#[test]
fn malformed_wrapped_row_cannot_emit_a_truncated_submission() {
    let scan = codex_scan("\x1b[1;2m› \x1b[0mFirst row\n  second row \x1b[31");
    assert!(scan.blocks.is_empty());
}

#[test]
fn placeholder_matching_is_exact_before_public_truncation() {
    let mut config = provider();
    config.idle_all = vec!["a".repeat(160)];
    let capture = format!("> {} substantive task", "a".repeat(160));
    let scan = TmuxVisibleScreen::with_runner(FakeRunner::output(true, &capture), || 99)
        .scan_prompts(&pane("%7"), &config)
        .unwrap();
    assert_eq!(
        scan.blocks[0].candidate.as_deref(),
        Some("a".repeat(160).as_str())
    );
}

#[test]
#[ignore = "requires installed tmux; creates and removes an isolated synthetic tmux server"]
fn real_tmux_capture_excludes_prompt_older_than_configured_history_tail() {
    use std::process::Command;
    use std::time::{Duration, Instant};

    struct IsolatedTmux(String);
    impl IsolatedTmux {
        fn run(&self, args: &[&str]) -> Vec<u8> {
            let output = Command::new("tmux")
                .args(["-L", &self.0, "-f", "/dev/null"])
                .args(args)
                .output()
                .unwrap();
            assert!(output.status.success(), "isolated tmux command failed");
            output.stdout
        }
    }
    impl Drop for IsolatedTmux {
        fn drop(&mut self) {
            let _ = Command::new("tmux")
                .args(["-L", &self.0, "kill-server"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
    }
    struct SocketRunner(String);
    impl CommandRunner for SocketRunner {
        fn output(&self, program: &str, args: &[&str]) -> io::Result<CommandOutput> {
            let mut socket_args = vec!["-L", self.0.as_str()];
            socket_args.extend_from_slice(args);
            super::screen::SystemCommandRunner.output(program, &socket_args)
        }
    }
    let tmux = IsolatedTmux(format!("harold-screen-test-{}", uuid::Uuid::new_v4()));
    tmux.run(&[
        "new-session",
        "-d",
        "-s",
        "fixture",
        "-x",
        "100",
        "-y",
        "10",
    ]);
    tmux.run(&["set-option", "-g", "history-limit", "4000"]);
    let pane_id = tmux.run(&["new-window", "-d", "-P", "-F", "#{pane_id}", "-t", "fixture", "/bin/sh", "-c",
        r"printf '\033[1;2m> \033[0mRetained synthetic instruction\n\n'; i=0; while [ $i -lt 2050 ]; do printf 'Subsequent output row\n'; i=$((i+1)); done; printf 'CAPTURE-READY\n'; exec /bin/sleep 30"]);
    let pane_id = String::from_utf8(pane_id).unwrap();
    let pane_id = pane_id.trim();
    let started = Instant::now();
    while !String::from_utf8_lossy(&tmux.run(&["capture-pane", "-p", "-t", pane_id]))
        .contains("CAPTURE-READY")
    {
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "fixture output did not settle"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let screen = TmuxVisibleScreen::with_runner(SocketRunner(tmux.0.clone()), || 99);
    let mut config = provider();
    config.screen_adapter = crate::settings::ScreenAdapter::CodexV1;
    assert!(
        screen
            .scan_prompts(&pane(pane_id), &config)
            .unwrap()
            .blocks
            .is_empty()
    );
    config.screen_history_lines = 4000;
    let retained = screen.scan_prompts(&pane(pane_id), &config).unwrap();
    assert_eq!(retained.blocks.len(), 1);
    assert_eq!(
        retained.blocks[0].candidate.as_deref(),
        Some("Retained synthetic instruction")
    );
}

#[test]
fn malformed_sgr_cannot_lend_submission_style_to_a_later_row() {
    for text in [
        "\x1b[1?m\n› \x1b[0mFORGED\n",
        "\x1b[1m\x1b[999m\n› \x1b[0mFORGED\n",
        "\x1b[1m\x1b[31\n› \x1b[0mFORGED\n",
    ] {
        assert!(codex_scan(text).blocks.is_empty());
    }
    let recovered = codex_scan("\x1b[1?m\n\x1b[0m\n\x1b[1;2m› \x1b[0mGenuine\n");
    assert_eq!(recovered.blocks.len(), 1);
    assert_eq!(recovered.blocks[0].candidate.as_deref(), Some("Genuine"));
}
