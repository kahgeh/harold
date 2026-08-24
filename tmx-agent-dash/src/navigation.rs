use crate::text::sanitize_display;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io;
use std::process::Command;

const ERROR_DETAIL_MAX_SCALARS: usize = 512;

struct CommandOutput {
    success: bool,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

trait CommandRunner {
    fn run(&self, program: &str, args: &[&str]) -> io::Result<CommandOutput>;
}

struct ProcessCommandRunner;

impl CommandRunner for ProcessCommandRunner {
    fn run(&self, program: &str, args: &[&str]) -> io::Result<CommandOutput> {
        let output = Command::new(program).args(args).output()?;
        Ok(CommandOutput {
            success: output.status.success(),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
}

trait ProcessContext {
    fn var_os(&self, name: &OsStr) -> Option<OsString>;
}

struct ProcessEnvironment;

impl ProcessContext for ProcessEnvironment {
    fn var_os(&self, name: &OsStr) -> Option<OsString> {
        env::var_os(name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavigationError {
    EmptyClient,
    EmptyPane,
    CommandIo {
        operation: &'static str,
        detail: String,
    },
    CommandFailed {
        operation: &'static str,
        detail: String,
    },
}

impl fmt::Display for NavigationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyClient => formatter.write_str("tmux client must not be empty"),
            Self::EmptyPane => formatter.write_str("tmux pane ID must not be empty"),
            Self::CommandIo { operation, detail } => {
                write!(formatter, "could not {operation}: {detail}")
            }
            Self::CommandFailed { operation, detail } => {
                write!(formatter, "could not {operation}: {detail}")
            }
        }
    }
}

impl std::error::Error for NavigationError {}

pub trait PaneNavigator {
    fn jump_to(&self, client: &str, pane_id: &str) -> Result<(), NavigationError>;
}

pub struct TmuxNavigator {
    runner: Box<dyn CommandRunner>,
    context: Box<dyn ProcessContext>,
}

impl TmuxNavigator {
    pub fn new() -> Self {
        Self {
            runner: Box::new(ProcessCommandRunner),
            context: Box::new(ProcessEnvironment),
        }
    }

    #[cfg(test)]
    fn with_ports(
        runner: impl CommandRunner + 'static,
        context: impl ProcessContext + 'static,
    ) -> Self {
        Self {
            runner: Box::new(runner),
            context: Box::new(context),
        }
    }

    pub fn discover_client(&self) -> Result<Option<String>, NavigationError> {
        if !self.has_invoking_tmux_context() {
            return Ok(None);
        }

        let output = self.run_tmux(
            "discover tmux client",
            &["display-message", "-p", "#{client_name}"],
        )?;
        let client = String::from_utf8_lossy(&output.stdout);
        let client = sanitize_display(client.trim(), ERROR_DETAIL_MAX_SCALARS);

        if client.is_empty() {
            return Ok(None);
        }

        Ok(Some(client))
    }

    fn has_invoking_tmux_context(&self) -> bool {
        ["TMUX", "TMUX_PANE"]
            .into_iter()
            .all(|name| self.context_var_is_terminal_safe(name.as_ref()))
    }

    fn context_var_is_terminal_safe(&self, name: &OsStr) -> bool {
        self.context
            .var_os(name)
            .map(|value| value.to_string_lossy().into_owned())
            .map(|value| sanitize_display(value.trim(), ERROR_DETAIL_MAX_SCALARS))
            .is_some_and(|value| !value.is_empty())
    }

    fn run_tmux(
        &self,
        operation: &'static str,
        args: &[&str],
    ) -> Result<CommandOutput, NavigationError> {
        let output = self
            .runner
            .run("tmux", args)
            .map_err(|error| NavigationError::CommandIo {
                operation,
                detail: sanitize_detail(&error.to_string()),
            })?;

        if !output.success {
            return Err(NavigationError::CommandFailed {
                operation,
                detail: command_failure_detail(&output.stderr),
            });
        }

        Ok(output)
    }
}

impl Default for TmuxNavigator {
    fn default() -> Self {
        Self::new()
    }
}

impl PaneNavigator for TmuxNavigator {
    fn jump_to(&self, client: &str, pane_id: &str) -> Result<(), NavigationError> {
        if client.trim().is_empty() {
            return Err(NavigationError::EmptyClient);
        }
        if pane_id.trim().is_empty() {
            return Err(NavigationError::EmptyPane);
        }

        self.run_tmux(
            "switch tmux client",
            &["switch-client", "-c", client, "-t", pane_id],
        )?;
        Ok(())
    }
}

fn command_failure_detail(stderr: &[u8]) -> String {
    let detail = sanitize_detail(&String::from_utf8_lossy(stderr));
    if detail.is_empty() {
        return "tmux exited unsuccessfully".into();
    }
    detail
}

fn sanitize_detail(detail: &str) -> String {
    sanitize_display(detail, ERROR_DETAIL_MAX_SCALARS)
}

#[cfg(test)]
mod tests {
    use super::{
        CommandOutput, CommandRunner, NavigationError, PaneNavigator, ProcessContext, TmuxNavigator,
    };
    use crate::app::{
        AgentIncarnation, AgentRow, AgentState, App, ConnectionState, SearchState, Snapshot,
    };
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::ffi::{OsStr, OsString};
    use std::io;
    use std::rc::Rc;

    type RecordedCall = (String, Vec<String>);
    type RecordedCalls = Rc<RefCell<Vec<RecordedCall>>>;

    #[derive(Clone)]
    struct FakeRunner {
        outputs: Rc<RefCell<VecDeque<io::Result<CommandOutput>>>>,
        calls: RecordedCalls,
    }

    impl FakeRunner {
        fn with_outputs(outputs: Vec<io::Result<CommandOutput>>) -> Self {
            Self {
                outputs: Rc::new(RefCell::new(outputs.into())),
                calls: Rc::new(RefCell::new(Vec::new())),
            }
        }

        fn successful(stdout: &str) -> Self {
            Self::with_outputs(vec![Ok(CommandOutput {
                success: true,
                stdout: stdout.as_bytes().to_vec(),
                stderr: Vec::new(),
            })])
        }

        fn failed(stderr: &str) -> Self {
            Self::with_outputs(vec![Ok(CommandOutput {
                success: false,
                stdout: Vec::new(),
                stderr: stderr.as_bytes().to_vec(),
            })])
        }

        fn calls(&self) -> Vec<RecordedCall> {
            self.calls.borrow().clone()
        }
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, program: &str, args: &[&str]) -> io::Result<CommandOutput> {
            self.calls.borrow_mut().push((
                program.to_owned(),
                args.iter().map(|argument| (*argument).to_owned()).collect(),
            ));
            self.outputs
                .borrow_mut()
                .pop_front()
                .expect("test runner should have one output per expected call")
        }
    }

    #[derive(Debug)]
    struct FakeContext {
        tmux: Option<OsString>,
        tmux_pane: Option<OsString>,
    }

    impl FakeContext {
        fn valid() -> Self {
            Self {
                tmux: Some("/tmp/tmux-501/default,123,0".into()),
                tmux_pane: Some("%23".into()),
            }
        }
    }

    impl ProcessContext for FakeContext {
        fn var_os(&self, name: &OsStr) -> Option<OsString> {
            match name.to_str() {
                Some("TMUX") => self.tmux.clone(),
                Some("TMUX_PANE") => self.tmux_pane.clone(),
                _ => None,
            }
        }
    }

    fn navigator_with(runner: FakeRunner, context: FakeContext) -> (TmuxNavigator, FakeRunner) {
        let observed_runner = runner.clone();
        (TmuxNavigator::with_ports(runner, context), observed_runner)
    }

    #[test]
    fn discovers_the_invoking_client_with_exact_tmux_argv() {
        let runner = FakeRunner::successful("client-9\n");
        let (navigator, observed_runner) = navigator_with(runner, FakeContext::valid());

        assert_eq!(
            navigator.discover_client().unwrap(),
            Some("client-9".into())
        );
        assert_eq!(
            observed_runner.calls(),
            vec![(
                "tmux".into(),
                vec![
                    "display-message".into(),
                    "-p".into(),
                    "#{client_name}".into()
                ]
            )]
        );
    }

    #[test]
    fn an_empty_display_message_means_there_is_no_tmux_client() {
        let (navigator, _) = navigator_with(FakeRunner::successful("\n"), FakeContext::valid());

        assert_eq!(navigator.discover_client().unwrap(), None);
    }

    #[test]
    fn missing_or_terminal_empty_tmux_context_never_invokes_tmux() {
        let absent = None;
        let empty = Some(OsString::new());
        let whitespace = Some(OsString::from(" \t\n "));
        let control_only = Some(OsString::from("\x1b[31m"));

        for (name, tmux, tmux_pane) in [
            ("missing TMUX", absent.clone(), Some(OsString::from("%23"))),
            ("empty TMUX", empty.clone(), Some(OsString::from("%23"))),
            (
                "whitespace TMUX",
                whitespace.clone(),
                Some(OsString::from("%23")),
            ),
            (
                "control-only TMUX",
                control_only.clone(),
                Some(OsString::from("%23")),
            ),
            (
                "missing TMUX_PANE",
                Some(OsString::from("tmux-context")),
                absent,
            ),
            (
                "empty TMUX_PANE",
                Some(OsString::from("tmux-context")),
                empty,
            ),
            (
                "whitespace TMUX_PANE",
                Some(OsString::from("tmux-context")),
                whitespace,
            ),
            (
                "control-only TMUX_PANE",
                Some(OsString::from("tmux-context")),
                control_only,
            ),
        ] {
            let runner = FakeRunner::with_outputs(Vec::new());
            let (navigator, observed_runner) =
                navigator_with(runner, FakeContext { tmux, tmux_pane });

            assert_eq!(navigator.discover_client().unwrap(), None, "{name}");
            assert!(observed_runner.calls().is_empty(), "{name}");
        }
    }

    #[test]
    fn jumps_to_the_pane_with_captured_client_and_exact_tmux_argv() {
        let runner = FakeRunner::successful("");
        let (navigator, observed_runner) = navigator_with(runner, FakeContext::valid());

        navigator.jump_to("client-9", "%27").unwrap();

        assert_eq!(
            observed_runner.calls(),
            vec![(
                "tmux".into(),
                vec![
                    "switch-client".into(),
                    "-c".into(),
                    "client-9".into(),
                    "-t".into(),
                    "%27".into()
                ]
            )]
        );
    }

    #[test]
    fn rejects_empty_client_and_pane_without_spawning_tmux() {
        let runner = FakeRunner::with_outputs(Vec::new());
        let (navigator, observed_runner) = navigator_with(runner, FakeContext::valid());

        assert_eq!(
            navigator.jump_to("", "%1"),
            Err(NavigationError::EmptyClient)
        );
        assert_eq!(
            navigator.jump_to("client-1", ""),
            Err(NavigationError::EmptyPane)
        );
        assert!(observed_runner.calls().is_empty());
    }

    #[test]
    fn vanished_pane_error_sanitizes_and_caps_hostile_tmux_stderr() {
        let hostile = format!("\x1b[31mvanished\x1b[0m\n{}", "界".repeat(600));
        let (navigator, _) = navigator_with(FakeRunner::failed(&hostile), FakeContext::valid());

        let error = navigator.jump_to("client-1", "%404").unwrap_err();
        let NavigationError::CommandFailed { operation, detail } = error else {
            panic!("expected a non-zero tmux status error");
        };

        assert_eq!(operation, "switch tmux client");
        assert!(detail.starts_with("vanished"));
        assert!(!detail.contains('\x1b'));
        assert!(!detail.contains('\n'));
        assert_eq!(detail.chars().count(), 512);
    }

    #[test]
    fn public_error_formatting_never_includes_command_stdout() {
        let runner = FakeRunner::with_outputs(vec![Ok(CommandOutput {
            success: false,
            stdout: b"private stdout".to_vec(),
            stderr: b"pane vanished".to_vec(),
        })]);
        let (navigator, _) = navigator_with(runner, FakeContext::valid());

        let error = navigator.jump_to("client-1", "%404").unwrap_err();

        assert!(!error.to_string().contains("private stdout"));
        assert!(!format!("{error:?}").contains("private stdout"));
    }

    #[test]
    fn navigation_failure_does_not_optimistically_remove_a_row() {
        let incarnation = AgentIncarnation {
            pane_id: "%404".into(),
            pane_pid: 404,
            agent_pid: 4_004,
            agent_started_at_ms: 40_004,
            provider_id: "codex".into(),
        };
        let mut app = App::new(
            ConnectionState::Live,
            Snapshot {
                through_event_version: 1,
                server_time_ms: 1,
                monitor_health: Vec::new(),
                rows: vec![AgentRow {
                    incarnation: incarnation.clone(),
                    provider_display_name: "Codex".into(),
                    tmux_target: "agents:0.0".into(),
                    session_name: "agents".into(),
                    window_index: 0,
                    pane_index: 0,
                    working_directory: "/tmp/project".into(),
                    work_summary: Some("Task 5".into()),
                    state: AgentState::Busy,
                    last_transition_at_ms: 1,
                }],
            },
            SearchState {
                query: String::new(),
                editing: false,
            },
            Some(incarnation),
        );
        let before = app.clone();
        let (navigator, _) =
            navigator_with(FakeRunner::failed("can't find pane"), FakeContext::valid());

        assert!(navigator.jump_to("client-1", "%404").is_err());
        assert_eq!(app, before);

        // The application remains snapshot-owned; only a later Harold snapshot may change rows.
        app.mark_disconnected();
        assert_eq!(app.snapshot.rows.len(), 1);
    }
}
