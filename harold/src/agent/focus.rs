//! Reads which pane the user looks at, from Rex or from tmux.
//!
//! The logic comes from the control-tower navigation code. Harold cannot depend on that crate.

use super::rex::{SystemRex, run_program};
use super::summary::sanitize_bounded_metadata;
use serde_json::{Value, json};
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const CALL_TIMEOUT: Duration = Duration::from_secs(3);
const PANE_ID_MAX_SCALARS: usize = 256;
const MAX_REPLY_BYTES: usize = 1 << 20;

/// Why Harold cannot tell which pane the user looks at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnknownReason {
    ManySessions,
    RexUnreachable,
    TmuxNoClient,
}

impl UnknownReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::ManySessions => "many_sessions",
            Self::RexUnreachable => "rex_unreachable",
            Self::TmuxNoClient => "tmux_no_client",
        }
    }
}

/// Runs one command and returns its output.
pub(crate) trait FocusPort: Send + Sync {
    fn run(&self, program: &str, args: &[&str]) -> Result<String, String>;
}

/// Runs the real `rex` and `tmux` programs. It starts no shell and starts no Rex server.
pub(crate) struct CommandFocusPort {
    rex: SystemRex,
}

impl CommandFocusPort {
    pub(crate) fn new(rex_setting: Option<String>) -> Self {
        Self {
            rex: SystemRex::new(rex_setting),
        }
    }
}

impl FocusPort for CommandFocusPort {
    fn run(&self, program: &str, args: &[&str]) -> Result<String, String> {
        if program == "rex" {
            let path = self.rex.program().map_err(|_| "rex not found".to_owned())?;
            return run_program(&path, args, CALL_TIMEOUT).map_err(|_| "rex failed".to_owned());
        }
        run_with_timeout(program, args, CALL_TIMEOUT)
    }
}

fn run_with_timeout(program: &str, args: &[&str], timeout: Duration) -> Result<String, String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("{program}: {error}"))?;
    let Some(mut stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("{program}: no output"));
    };
    let reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        stdout
            .by_ref()
            .take(MAX_REPLY_BYTES as u64 + 1)
            .read_to_end(&mut buffer)
            .map(|_| buffer)
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{program}: {error}"));
            }
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("{program}: timeout"));
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    let output = reader
        .join()
        .map_err(|_| format!("{program}: read failed"))?
        .map_err(|error| format!("{program}: {error}"))?;
    if !status.success() {
        return Err(format!("{program}: exited unsuccessfully"));
    }
    if output.len() > MAX_REPLY_BYTES {
        return Err(format!("{program}: reply too long"));
    }
    String::from_utf8(output).map_err(|_| format!("{program}: reply is not UTF-8"))
}

/// The pane the user looks at. `in_rex` is true when the first local pane is a Rex block.
/// `Ok(None)` means the multiplexer shows no focused pane.
pub(crate) fn read_focus(
    port: &dyn FocusPort,
    in_rex: bool,
) -> Result<Option<String>, UnknownReason> {
    if in_rex {
        rex_focus(port)
    } else {
        tmux_focus(port)
    }
}

fn display(text: &str) -> String {
    sanitize_bounded_metadata(text.trim(), PANE_ID_MAX_SCALARS)
        .trim()
        .to_owned()
}

/// The focused pane of the best attached tmux client: focused first, then newest activity.
/// Suspended clients do not count.
fn tmux_focus(port: &dyn FocusPort) -> Result<Option<String>, UnknownReason> {
    let listing = port
        .run(
            "tmux",
            &[
                "list-clients",
                "-F",
                "#{client_activity}\t#{client_flags}\t#{pane_id}",
            ],
        )
        .map_err(|_| UnknownReason::TmuxNoClient)?;
    let best = listing
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let activity = fields.next()?.trim().parse::<u64>().ok()?;
            let flags = fields.next()?;
            let pane = display(fields.next()?);
            let has = |flag: &str| flags.split(',').any(|candidate| candidate == flag);
            (!pane.is_empty() && !has("suspended")).then(|| (has("focused"), activity, pane))
        })
        .max_by(|left, right| (left.0, left.1).cmp(&(right.0, right.1)));
    best.map(|(_, _, pane)| Some(pane))
        .ok_or(UnknownReason::TmuxNoClient)
}

/// The focused block of the active window of the one session the Rex app shows. Rex does not
/// say which session the app has in front, so with many sessions Harold does not guess.
fn rex_focus(port: &dyn FocusPort) -> Result<Option<String>, UnknownReason> {
    let clients = port
        .run("rex", &["client", "ls", "--json"])
        .map_err(|_| UnknownReason::RexUnreachable)?;
    let (app_listed, sessions) =
        rex_app_session_ids(&clients).ok_or(UnknownReason::RexUnreachable)?;
    if !app_listed {
        return Err(UnknownReason::RexUnreachable);
    }
    let [session] = sessions.as_slice() else {
        return if sessions.is_empty() {
            Err(UnknownReason::RexUnreachable)
        } else {
            Err(UnknownReason::ManySessions)
        };
    };
    let request = json!({ "session_id": session }).to_string();
    let view = port
        .run("rex", &["api", "call", "session.view", &request])
        .map_err(|_| UnknownReason::RexUnreachable)?;
    let view: Value = serde_json::from_str(&view).map_err(|_| UnknownReason::RexUnreachable)?;
    rex_focused_block_of(&view)
        .map(|block| display(&block))
        .filter(|block| !block.is_empty())
        .map(Some)
        .ok_or(UnknownReason::RexUnreachable)
}

/// Whether the listing has a Rex app client, and the sessions that the app clients show.
/// `None` when the text is not the expected JSON.
fn rex_app_session_ids(clients: &str) -> Option<(bool, Vec<String>)> {
    let value: Value = serde_json::from_str(clients).ok()?;
    let mut app_listed = false;
    let mut ids = Vec::new();
    for client in value.get("clients")?.as_array()? {
        if client.pointer("/info/kind").and_then(Value::as_str) != Some("app") {
            continue;
        }
        app_listed = true;
        let sessions = client.get("session_ids").and_then(Value::as_array);
        ids.extend(
            sessions
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned),
        );
    }
    Some((app_listed, ids))
}

/// The focused block of the active window in a `session.view` result.
fn rex_focused_block_of(view: &Value) -> Option<String> {
    let active = view.get("active_window_id")?.as_str()?;
    view.get("windows")?
        .as_array()?
        .iter()
        .find(|window| window.get("window_id").and_then(Value::as_str) == Some(active))?
        .get("focused_block_id")?
        .as_str()
        .map(str::to_owned)
}
