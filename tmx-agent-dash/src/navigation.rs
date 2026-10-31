use crate::text::sanitize_display;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

const ERROR_DETAIL_MAX_SCALARS: usize = 512;

/// The copy of `rex` that the running Rex server keeps, under the user's home folder.
const REX_SERVER_COPY: &str = "Library/Application Support/com.superlogical.rex/RexServer/Versions/Current/Rex Server.app/Contents/MacOS/rex";
/// The copy of `rex` inside a Rex app bundle in `/Applications`.
const REX_BUNDLE_COPY: &str = "Contents/Helpers/Rex Server.rex/Contents/MacOS/rex";
/// A Rex pane id starts with this. A tmux pane id starts with `%`.
pub const REX_BLOCK_PREFIX: &str = "block:";

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

    fn is_file(&self, path: &Path) -> bool {
        path.is_file()
    }

    /// The Rex app bundles in `/Applications`.
    fn rex_app_bundles(&self) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir("/Applications") else {
            return Vec::new();
        };
        let mut bundles: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(OsStr::to_str)
                    .is_some_and(|name| name.starts_with("Rex") && name.ends_with(".app"))
            })
            .collect();
        bundles.sort();
        bundles
    }
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
    /// Only an `https://` or `http://` link is opened.
    NotWebLink,
    /// This navigator cannot open links.
    LinksUnavailable,
    /// This navigator cannot open an editor.
    EditorUnavailable,
    /// Only the absolute path of a file is opened in the editor.
    NotAFilePath,
    /// The pane is a Rex block, and no `rex` program was found.
    RexNotFound,
    /// The Rex block id or session name is empty or holds a control character.
    BadRexTarget,
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
            Self::NotWebLink => formatter.write_str("only an http(s) link can be opened"),
            Self::LinksUnavailable => formatter.write_str("links cannot be opened here"),
            Self::EditorUnavailable => formatter.write_str("an editor cannot be opened here"),
            Self::NotAFilePath => formatter.write_str("only a file's absolute path can be opened"),
            Self::RexNotFound => formatter.write_str("the rex program was not found"),
            Self::BadRexTarget => formatter.write_str("the Rex session or block is not valid"),
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

    /// Shows a Rex block in the Rex app.
    fn focus_rex_block(&self, _session: &str, _block: &str) -> Result<(), NavigationError> {
        Err(NavigationError::RexNotFound)
    }

    /// Opens a question's link in the user's browser.
    fn open_link(&self, _link: &str) -> Result<(), NavigationError> {
        Err(NavigationError::LinksUnavailable)
    }

    /// Opens a file in `$EDITOR` in a new tmux window.
    fn open_editor(&self, _file: &str) -> Result<(), NavigationError> {
        Err(NavigationError::EditorUnavailable)
    }
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

    /// The pane the user is looking at: the active pane of the best attached client.
    /// Suspended clients are ignored; a focused client wins; otherwise the most recently active.
    pub fn focused_pane(&self) -> Result<Option<String>, NavigationError> {
        let output = self.run_tmux(
            "read tmux clients",
            &[
                "list-clients",
                "-F",
                "#{client_activity}\t#{client_flags}\t#{pane_id}",
            ],
        )?;
        let listing = String::from_utf8_lossy(&output.stdout);
        let best = listing
            .lines()
            .filter_map(|line| {
                let mut fields = line.split('\t');
                let activity = fields.next()?.trim().parse::<u64>().ok()?;
                let flags = fields.next()?;
                let pane = sanitize_display(fields.next()?.trim(), ERROR_DETAIL_MAX_SCALARS);
                let has = |flag: &str| flags.split(',').any(|candidate| candidate == flag);
                (!pane.is_empty() && !has("suspended")).then(|| (has("focused"), activity, pane))
            })
            .max_by(|left, right| (left.0, left.1).cmp(&(right.0, right.1)));
        Ok(best.map(|(_, _, pane)| pane))
    }

    /// Finds `rex`: the server's own copy, then `PATH`, then an app bundle.
    fn find_rex(&self) -> Option<PathBuf> {
        let from_home = self
            .context
            .var_os("HOME".as_ref())
            .map(|home| PathBuf::from(home).join(REX_SERVER_COPY));
        let from_path: Vec<PathBuf> = self
            .context
            .var_os("PATH".as_ref())
            .map(|path| env::split_paths(&path).map(|dir| dir.join("rex")).collect())
            .unwrap_or_default();
        let from_bundles = self
            .context
            .rex_app_bundles()
            .into_iter()
            .map(|bundle| bundle.join(REX_BUNDLE_COPY));
        from_home
            .into_iter()
            .chain(from_path)
            .chain(from_bundles)
            .find(|candidate| candidate.is_absolute() && self.context.is_file(candidate))
    }

    /// Runs `rex` with its first argument, then `--autostart=false` (the dash never
    /// starts a Rex server), then the rest.
    fn run_rex(
        &self,
        operation: &'static str,
        args: &[&str],
    ) -> Result<CommandOutput, NavigationError> {
        let rex = self.find_rex().ok_or(NavigationError::RexNotFound)?;
        let mut full = vec![args[0], "--autostart=false", "--timeout=3s"];
        full.extend_from_slice(&args[1..]);
        self.run_command(&rex.to_string_lossy(), operation, &full)
    }

    /// The Rex block the user sees: the focused block of the active window of the one session
    /// the Rex app shows. Rex does not say which session the app has in front, so with
    /// more than one session this is `None` and the dash does not guess.
    pub fn rex_focused_block(&self) -> Result<Option<String>, NavigationError> {
        if self.find_rex().is_none() {
            return Ok(None);
        }
        let clients = self.run_rex("read Rex clients", &["client", "ls", "--json"])?;
        let clients = String::from_utf8_lossy(&clients.stdout);
        let sessions = rex_app_session_ids(&clients);
        let [session] = sessions.as_slice() else {
            return Ok(None);
        };
        let request = format!("{{\"session_id\":\"{session}\"}}");
        let view = self.run_rex(
            "read Rex session",
            &["api", "call", "session.view", &request],
        )?;
        let view = String::from_utf8_lossy(&view.stdout);
        Ok(rex_focused_block_of(&view)
            .map(|block| sanitize_display(&block, ERROR_DETAIL_MAX_SCALARS)))
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
        self.run_command("tmux", operation, args)
    }

    fn run_command(
        &self,
        program: &str,
        operation: &'static str,
        args: &[&str],
    ) -> Result<CommandOutput, NavigationError> {
        let output =
            self.runner
                .run(program, args)
                .map_err(|error| NavigationError::CommandIo {
                    operation,
                    detail: sanitize_detail(&error.to_string()),
                })?;

        if !output.success {
            return Err(NavigationError::CommandFailed {
                operation,
                detail: command_failure_detail(program, &output.stderr),
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

        // No `-c`: a client name is its tty path, which a suspended client on the same
        // tty shares, and tmux resolves a shared name to the oldest match. Left to
        // itself tmux moves the most recently active client of this session.
        self.run_tmux("switch tmux client", &["switch-client", "-t", pane_id])?;
        Ok(())
    }

    /// Runs `rex focus` for the block, then `rex do session.select` so the Rex app shows the
    /// session. The block id and the session name are separate arguments.
    fn focus_rex_block(&self, session: &str, block: &str) -> Result<(), NavigationError> {
        let clean = |text: &str| !text.trim().is_empty() && !text.chars().any(char::is_control);
        if !clean(session) || !clean(block) || !block.starts_with(REX_BLOCK_PREFIX) {
            return Err(NavigationError::BadRexTarget);
        }
        let session_flag = format!("--session={session}");
        self.run_rex("focus Rex block", &["focus", &session_flag, block])?;
        let sessions = self.run_rex("list Rex sessions", &["ls", "--json"])?;
        let sessions = String::from_utf8_lossy(&sessions.stdout);
        let Some(session_id) = rex_session_id(&sessions, session) else {
            return Err(NavigationError::CommandFailed {
                operation: "show Rex session",
                detail: "no Rex session has this name".into(),
            });
        };
        let argument = format!("session_id={session_id}");
        self.run_rex("show Rex session", &["do", "session.select", &argument])?;
        Ok(())
    }

    /// Runs `open <link>`. The scheme check means the link can never be read as a flag
    /// or name a local file or app.
    fn open_link(&self, link: &str) -> Result<(), NavigationError> {
        if !(link.starts_with("https://") || link.starts_with("http://")) {
            return Err(NavigationError::NotWebLink);
        }
        self.run_command("open", "open link", &[link])?;
        Ok(())
    }

    /// Runs `tmux new-window` with `$EDITOR` (`vi` when it is unset) on the file. The editor
    /// is handed over as a variable and the path as an argument, so neither is read as
    /// shell text beyond the editor's own words.
    fn open_editor(&self, file: &str) -> Result<(), NavigationError> {
        if !file.starts_with('/') || file.chars().any(char::is_control) {
            return Err(NavigationError::NotAFilePath);
        }
        let editor = self
            .context
            .var_os("EDITOR".as_ref())
            .map(|value| value.to_string_lossy().into_owned())
            .filter(|value| !value.trim().is_empty() && !value.chars().any(char::is_control))
            .unwrap_or_else(|| "vi".to_owned());
        let editor_variable = format!("EDITOR={editor}");
        self.run_tmux(
            "open editor",
            &[
                "new-window",
                "-n",
                "note",
                "--",
                "env",
                &editor_variable,
                "sh",
                "-c",
                "exec $EDITOR \"$1\"",
                "sh",
                file,
            ],
        )?;
        Ok(())
    }
}

/// The string value of the first `"key"` at or after `from`: its text and the offset after it.
fn json_string(text: &str, key: &str, from: usize) -> Option<(String, usize)> {
    let needle = format!("\"{key}\"");
    let at = from + text.get(from..)?.find(&needle)? + needle.len();
    let rest = text.get(at..)?.trim_start().strip_prefix(':')?.trim_start();
    let start = text.len() - rest.len();
    let body = rest.strip_prefix('"')?;
    let mut value = String::new();
    let mut chars = body.char_indices();
    while let Some((offset, character)) = chars.next() {
        match character {
            '"' => return Some((value, start + 1 + offset + 1)),
            '\\' => match chars.next()?.1 {
                'u' => {
                    let hex: String = chars.by_ref().take(4).map(|(_, digit)| digit).collect();
                    let code = u32::from_str_radix(&hex, 16).ok()?;
                    value.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                }
                other => value.push(other),
            },
            other => value.push(other),
        }
    }
    None
}

/// The id of the session with this name in `rex ls --json`.
fn rex_session_id(listing: &str, name: &str) -> Option<String> {
    let mut at = 0;
    while let Some((id, after)) = json_string(listing, "session_id", at) {
        let end = json_string(listing, "session_id", after).map_or(listing.len(), |(_, n)| n);
        let label = json_string(listing.get(..end)?, "label", after);
        if label.is_some_and(|(label, _)| label == name) {
            return Some(id);
        }
        at = after;
    }
    None
}

/// The sessions that the Rex app shows: `session_ids` of the `app` client in `rex client ls --json`.
fn rex_app_session_ids(clients: &str) -> Vec<String> {
    let mut ids = Vec::new();
    for client in clients.split("\"client_id\"").skip(1) {
        let is_app = json_string(client, "kind", 0).is_some_and(|(kind, _)| kind == "app");
        let Some(list) = client.find("\"session_ids\"").filter(|_| is_app) else {
            continue;
        };
        let body = client.get(list..).unwrap_or_default();
        let body = body.split_once('[').map_or("", |(_, rest)| rest);
        let body = body.split(']').next().unwrap_or_default();
        ids.extend(body.split('"').skip(1).step_by(2).map(str::to_owned));
    }
    ids
}

/// The focused block of the active window in a `session.view` result.
fn rex_focused_block_of(view: &str) -> Option<String> {
    let (active, _) = json_string(view, "active_window_id", 0)?;
    let mut at = 0;
    while let Some((window, after)) = json_string(view, "window_id", at) {
        if window == active {
            return json_string(view, "focused_block_id", after).map(|(block, _)| block);
        }
        at = after;
    }
    None
}

fn command_failure_detail(program: &str, stderr: &[u8]) -> String {
    let detail = sanitize_detail(&String::from_utf8_lossy(stderr));
    if detail.is_empty() {
        return format!("{program} exited unsuccessfully");
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
        editor: Option<OsString>,
    }

    impl FakeContext {
        fn valid() -> Self {
            Self {
                tmux: Some("/tmp/tmux-501/default,123,0".into()),
                tmux_pane: Some("%23".into()),
                editor: None,
            }
        }

        fn with_editor(editor: &str) -> Self {
            Self {
                editor: Some(editor.into()),
                ..Self::valid()
            }
        }
    }

    impl ProcessContext for FakeContext {
        fn var_os(&self, name: &OsStr) -> Option<OsString> {
            match name.to_str() {
                Some("TMUX") => self.tmux.clone(),
                Some("TMUX_PANE") => self.tmux_pane.clone(),
                Some("EDITOR") => self.editor.clone(),
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
            let (navigator, observed_runner) = navigator_with(
                runner,
                FakeContext {
                    tmux,
                    tmux_pane,
                    editor: None,
                },
            );

            assert_eq!(navigator.discover_client().unwrap(), None, "{name}");
            assert!(observed_runner.calls().is_empty(), "{name}");
        }
    }

    #[test]
    fn jump_lets_tmux_choose_the_client_instead_of_naming_it() {
        let runner = FakeRunner::successful("");
        let (navigator, observed_runner) = navigator_with(runner, FakeContext::valid());

        // Client names are tty paths and are not unique: a suspended client on the
        // same tty shares the name and tmux resolves the name to the oldest match.
        navigator.jump_to("/dev/ttys000", "%27").unwrap();

        assert_eq!(
            observed_runner.calls(),
            vec![(
                "tmux".into(),
                vec!["switch-client".into(), "-t".into(), "%27".into()]
            )]
        );
    }

    #[test]
    fn open_editor_opens_a_tmux_window_with_the_editor_and_the_file_as_separate_arguments() {
        let (navigator, runner) = navigator_with(
            FakeRunner::successful(""),
            FakeContext::with_editor("nvim -u NONE"),
        );
        navigator
            .open_editor("/vault/references/a note.md")
            .unwrap();
        let calls = runner.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "tmux");
        assert_eq!(
            calls[0].1,
            vec![
                "new-window",
                "-n",
                "note",
                "--",
                "env",
                "EDITOR=nvim -u NONE",
                "sh",
                "-c",
                "exec $EDITOR \"$1\"",
                "sh",
                "/vault/references/a note.md",
            ]
        );
    }

    #[test]
    fn open_editor_uses_vi_without_an_editor_and_refuses_anything_but_an_absolute_path() {
        let (navigator, runner) = navigator_with(FakeRunner::successful(""), FakeContext::valid());
        navigator.open_editor("/vault/a.md").unwrap();
        assert!(runner.calls()[0].1.contains(&"EDITOR=vi".to_string()));

        let (navigator, runner) =
            navigator_with(FakeRunner::with_outputs(Vec::new()), FakeContext::valid());
        for file in ["", "relative.md", "-n", "/vault/a\n.md"] {
            assert_eq!(
                navigator.open_editor(file),
                Err(NavigationError::NotAFilePath)
            );
        }
        assert!(runner.calls().is_empty());

        let (navigator, _) = navigator_with(FakeRunner::failed("no server"), FakeContext::valid());
        assert!(matches!(
            navigator.open_editor("/vault/a.md"),
            Err(NavigationError::CommandFailed {
                operation: "open editor",
                ..
            })
        ));
    }

    #[test]
    fn open_link_runs_open_with_the_link() {
        let runner = FakeRunner::successful("");
        let (navigator, observed_runner) = navigator_with(runner, FakeContext::valid());

        navigator.open_link("https://example.com/spec").unwrap();

        assert_eq!(
            observed_runner.calls(),
            vec![("open".into(), vec!["https://example.com/spec".into()])]
        );

        // Anything but an http(s) link is refused without running a command.
        let runner = FakeRunner::with_outputs(Vec::new());
        let (navigator, observed_runner) = navigator_with(runner, FakeContext::valid());
        for link in ["-a Calculator", "file:///etc/passwd", ""] {
            assert_eq!(navigator.open_link(link), Err(NavigationError::NotWebLink));
        }
        assert!(observed_runner.calls().is_empty());

        let (navigator, _) = navigator_with(FakeRunner::failed("no app"), FakeContext::valid());
        assert!(matches!(
            navigator.open_link("http://example.com"),
            Err(NavigationError::CommandFailed {
                operation: "open link",
                ..
            })
        ));
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
                harold_version: None,
                tts_muted: false,
                messaging_paused: false,
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

    #[test]
    fn focused_pane_uses_exact_argv_and_ignores_suspended_clients() {
        let (navigator, runner) = navigator_with(
            FakeRunner::successful(
                "1790945360\tattached,focused,suspended,UTF-8\t%0\n\
                 1791018376\tattached,focused,suspended,UTF-8\t%36\n\
                 1791453304\tattached,focused,UTF-8\t%53\n",
            ),
            FakeContext::valid(),
        );
        assert_eq!(navigator.focused_pane(), Ok(Some("%53".to_owned())));
        assert_eq!(
            runner.calls(),
            vec![(
                "tmux".into(),
                vec![
                    "list-clients".into(),
                    "-F".into(),
                    "#{client_activity}\t#{client_flags}\t#{pane_id}".into()
                ]
            )]
        );
    }

    #[test]
    fn focused_client_beats_a_more_recent_unfocused_one_and_activity_breaks_ties() {
        let (navigator, _) = navigator_with(
            FakeRunner::successful("900\tattached,UTF-8\t%1\n100\tattached,focused\t%2\n"),
            FakeContext::valid(),
        );
        assert_eq!(navigator.focused_pane(), Ok(Some("%2".to_owned())));

        let (navigator, _) = navigator_with(
            FakeRunner::successful("100\tattached\t%1\n900\tattached\t%2\n500\tattached\t%3\n"),
            FakeContext::valid(),
        );
        assert_eq!(navigator.focused_pane(), Ok(Some("%2".to_owned())));
    }

    #[test]
    fn no_usable_client_is_none_and_command_failure_is_an_error() {
        for stdout in [
            "",
            "\n",
            "garbage\n",
            "x\tattached\t%1\n",
            "5\tattached,suspended\t%1\n",
            "5\tattached\t\n",
        ] {
            let (navigator, _) =
                navigator_with(FakeRunner::successful(stdout), FakeContext::valid());
            assert_eq!(navigator.focused_pane(), Ok(None), "{stdout:?}");
        }
        let (navigator, _) = navigator_with(
            FakeRunner::failed("no server running"),
            FakeContext::valid(),
        );
        assert!(navigator.focused_pane().is_err());
    }

    // ---- Rex ----

    use std::path::{Path, PathBuf};

    const REX_SERVER: &str = "/Users/u/Library/Application Support/com.superlogical.rex/RexServer/Versions/Current/Rex Server.app/Contents/MacOS/rex";
    const REX_BUNDLE: &str =
        "/Applications/Rex Beta.app/Contents/Helpers/Rex Server.rex/Contents/MacOS/rex";

    struct RexContext {
        path: Option<&'static str>,
        files: Vec<&'static str>,
    }

    impl ProcessContext for RexContext {
        fn var_os(&self, name: &OsStr) -> Option<OsString> {
            match name.to_str() {
                Some("HOME") => Some("/Users/u".into()),
                Some("PATH") => self.path.map(Into::into),
                _ => None,
            }
        }

        fn is_file(&self, path: &Path) -> bool {
            self.files.iter().any(|file| Path::new(file) == path)
        }

        fn rex_app_bundles(&self) -> Vec<PathBuf> {
            vec!["/Applications/Rex Beta.app".into()]
        }
    }

    fn rex_navigator(files: Vec<&'static str>, outputs: Vec<&str>) -> (TmuxNavigator, FakeRunner) {
        let runner = FakeRunner::with_outputs(
            outputs
                .into_iter()
                .map(|stdout| {
                    Ok(CommandOutput {
                        success: true,
                        stdout: stdout.as_bytes().to_vec(),
                        stderr: Vec::new(),
                    })
                })
                .collect(),
        );
        let observed = runner.clone();
        let context = RexContext {
            path: Some("/opt/bin:/usr/bin"),
            files,
        };
        (TmuxNavigator::with_ports(runner, context), observed)
    }

    const SESSIONS: &str = r#"{
  "sessions": [
    {
      "session_id": "session:aaa",
      "label": "arid-moor"
    },
    {
      "session_id": "session:bbb",
      "label": "rexsup-dash-28440"
    }
  ]
}"#;

    const CLIENTS: &str = r#"{
  "clients": [
    {
      "client_id": "client:cli",
      "info": { "kind": "cli", "name": "rex" },
      "connection_state": "connected"
    },
    {
      "client_id": "client:app",
      "info": {
        "kind": "app",
        "name": "Rex Beta"
      },
      "connection_state": "connected",
      "session_ids": [
        "session:aaa",
        "session:bbb"
      ]
    },
    {
      "client_id": "client:other",
      "info": { "kind": "cli", "name": "rex" },
      "session_ids": ["session:zzz"]
    }
  ]
}"#;

    const CLIENTS_ONE_SESSION: &str = r#"{"clients":[{"client_id":"client:app","info":{"kind":"app"},"session_ids":["session:aaa"]}]}"#;

    const VIEW_OF_AAA: &str = r#"{"revision":1,"session_id":"session:aaa","label":"arid-moor","active_window_id":"window:2","windows":[{"window_id":"window:1","label":"One","focused_block_id":"block:one","layers":[]},{"window_id":"window:2","label":"Two","focused_block_id":"block:two","layers":[{"layer_id":"layer:x","blocks":[{"block_id":"block:two"}]}]}]}"#;

    fn args(call: &RecordedCall) -> Vec<&str> {
        call.1.iter().map(String::as_str).collect()
    }

    #[test]
    fn rex_is_found_in_the_server_copy_then_the_path_then_the_app_bundle() {
        let all = vec![REX_SERVER, "/opt/bin/rex", REX_BUNDLE];
        let (navigator, _) = rex_navigator(all.clone(), vec![]);
        assert_eq!(navigator.find_rex(), Some(PathBuf::from(REX_SERVER)));

        let (navigator, _) = rex_navigator(all[1..].to_vec(), vec![]);
        assert_eq!(navigator.find_rex(), Some(PathBuf::from("/opt/bin/rex")));

        let (navigator, _) = rex_navigator(all[2..].to_vec(), vec![]);
        assert_eq!(navigator.find_rex(), Some(PathBuf::from(REX_BUNDLE)));

        let (navigator, _) = rex_navigator(vec![], vec![]);
        assert_eq!(navigator.find_rex(), None);
    }

    #[test]
    fn enter_on_a_rex_block_focuses_it_then_selects_its_session_in_the_app() {
        let (navigator, runner) = rex_navigator(vec![REX_SERVER], vec!["", SESSIONS, ""]);

        navigator
            .focus_rex_block("rexsup-dash-28440", "block:abc")
            .unwrap();

        let calls = runner.calls();
        assert_eq!(calls.len(), 3);
        assert!(calls.iter().all(|call| call.0 == REX_SERVER));
        assert_eq!(
            args(&calls[0]),
            [
                "focus",
                "--autostart=false",
                "--timeout=3s",
                "--session=rexsup-dash-28440",
                "block:abc"
            ]
        );
        assert_eq!(
            args(&calls[1]),
            ["ls", "--autostart=false", "--timeout=3s", "--json"]
        );
        assert_eq!(
            args(&calls[2]),
            [
                "do",
                "--autostart=false",
                "--timeout=3s",
                "session.select",
                "session_id=session:bbb"
            ]
        );
    }

    #[test]
    fn a_rex_block_in_an_unknown_session_is_not_selected() {
        let (navigator, runner) = rex_navigator(vec![REX_SERVER], vec!["", SESSIONS]);

        let error = navigator.focus_rex_block("gone", "block:abc").unwrap_err();

        assert!(matches!(error, NavigationError::CommandFailed { .. }));
        assert_eq!(runner.calls().len(), 2);
    }

    #[test]
    fn a_bad_rex_target_or_a_missing_rex_runs_nothing() {
        let (navigator, runner) = rex_navigator(vec![REX_SERVER], vec![]);
        for (session, block) in [
            ("", "block:a"),
            ("s", ""),
            ("s", "%4"),
            ("s", "block:a\n"),
            ("s\x1b", "block:a"),
        ] {
            assert_eq!(
                navigator.focus_rex_block(session, block),
                Err(NavigationError::BadRexTarget),
                "{session:?} {block:?}"
            );
        }
        assert!(runner.calls().is_empty());

        let (navigator, runner) = rex_navigator(vec![], vec![]);
        assert_eq!(
            navigator.focus_rex_block("s", "block:a"),
            Err(NavigationError::RexNotFound)
        );
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn a_failed_rex_command_is_an_error_with_its_text() {
        let runner = FakeRunner::failed("no such block");
        let observed = runner.clone();
        let context = RexContext {
            path: None,
            files: vec![REX_SERVER],
        };
        let navigator = TmuxNavigator::with_ports(runner, context);

        let error = navigator.focus_rex_block("s", "block:a").unwrap_err();

        assert!(error.to_string().contains("no such block"), "{error}");
        assert_eq!(observed.calls().len(), 1);
    }

    #[test]
    fn the_rex_focus_is_the_focused_block_of_the_active_window_of_the_shown_session() {
        let (navigator, runner) =
            rex_navigator(vec![REX_SERVER], vec![CLIENTS_ONE_SESSION, VIEW_OF_AAA]);

        assert_eq!(
            navigator.rex_focused_block().unwrap(),
            Some("block:two".into())
        );

        let calls = runner.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(
            args(&calls[0]),
            [
                "client",
                "--autostart=false",
                "--timeout=3s",
                "ls",
                "--json"
            ]
        );
        assert_eq!(
            args(&calls[1]),
            [
                "api",
                "--autostart=false",
                "--timeout=3s",
                "call",
                "session.view",
                r#"{"session_id":"session:aaa"}"#
            ]
        );
    }

    #[test]
    fn the_rex_focus_is_unknown_with_several_sessions_or_no_rex() {
        let (navigator, runner) = rex_navigator(vec![REX_SERVER], vec![CLIENTS]);
        assert_eq!(navigator.rex_focused_block().unwrap(), None);
        assert_eq!(runner.calls().len(), 1);

        let (navigator, runner) = rex_navigator(vec![], vec![]);
        assert_eq!(navigator.rex_focused_block().unwrap(), None);
        assert!(runner.calls().is_empty());

        let (navigator, _) = rex_navigator(vec![REX_SERVER], vec![r#"{"clients":[]}"#]);
        assert_eq!(navigator.rex_focused_block().unwrap(), None);
    }

    #[test]
    fn only_the_app_clients_sessions_count_as_shown() {
        assert_eq!(
            super::rex_app_session_ids(CLIENTS),
            vec!["session:aaa".to_owned(), "session:bbb".to_owned()]
        );
    }

    #[test]
    fn a_session_name_with_an_escaped_character_is_matched() {
        let listing = r#"{"sessions":[{"session_id":"session:x","label":"r\u0026d \"q\""}]}"#;

        assert_eq!(
            super::rex_session_id(listing, "r&d \"q\""),
            Some("session:x".into())
        );
    }
}
