//! Starting Claude Code for one watch run. The flags are decided in one place,
//! `claude_args`; `ClaudeRunner` starts the process in its own process group and
//! kills the whole group when the run is over time.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};

use crate::tools::BoxFuture;

/// Whether the installed Claude Code accepts `--max-turns 8` with `--print`. Settled
/// once, by the builder, from `claude --help` or one run (see the merge report); the
/// flag is passed only if this is true. Claude Code 2.1.304 does not list it in
/// `--help`, but one run in an empty folder with `--max-turns 8 --max-budget-usd 0.01`
/// was accepted and ended normally.
pub(super) const MAX_TURNS_ACCEPTED: bool = true;

pub(super) const MAX_TURNS: &str = "8";

/// A run that has not finished after this long is stopped.
pub(super) const RUN_LIMIT: Duration = Duration::from_secs(120);

/// The most output read from one run.
const OUTPUT_LIMIT: usize = 65_536;

/// Everything one model run needs.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ModelJob {
    pub folder: PathBuf,
    pub model: String,
    pub system_prompt: String,
    /// The skill's `allowed-tools`, exactly as written.
    pub allowed_tools: Option<String>,
    pub run_budget_usd_micros: Option<i64>,
    pub facts_json: Vec<u8>,
    pub timeout: Duration,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ModelResult {
    Exited {
        stdout: Vec<u8>,
        ok: bool,
    },
    /// Over time: the process group was killed.
    Killed,
    FailedToStart,
}

/// Runs the model once. The daemon's implementation is `ClaudeRunner`; the tests use a
/// fake.
pub(super) trait ModelRunner: Send + Sync {
    fn run<'a>(&'a self, job: &'a ModelJob) -> BoxFuture<'a, ModelResult>;
}

/// A dollar amount from millionths, with no trailing zeros: 30000 is `0.03`.
pub(super) fn usd_text(micros: i64) -> String {
    let text = format!("{}.{:06}", micros / 1_000_000, micros % 1_000_000);
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// The arguments of one run. `--allowedTools` is the skill's `allowed-tools` as
/// written, and is left out when the skill has none.
pub(super) fn claude_args(job: &ModelJob, max_turns: bool) -> Vec<OsString> {
    let mut args: Vec<OsString> = ["--print", "--safe-mode", "--no-session-persistence"]
        .into_iter()
        .map(OsString::from)
        .collect();
    let mut flag = |name: &str, value: &str| {
        args.push(name.into());
        args.push(value.into());
    };
    flag("--model", &job.model);
    flag("--output-format", "json");
    if let Some(micros) = job.run_budget_usd_micros {
        flag("--max-budget-usd", &usd_text(micros));
    }
    if max_turns {
        flag("--max-turns", MAX_TURNS);
    }
    if let Some(tools) = &job.allowed_tools {
        flag("--allowedTools", tools);
    }
    flag("--system-prompt", &job.system_prompt);
    args
}

/// Starts the real Claude Code.
pub(super) struct ClaudeRunner {
    cli_path: String,
    environment: Vec<(OsString, OsString)>,
    max_turns: bool,
}

impl ClaudeRunner {
    pub(super) fn new(cli_path: String, environment: Vec<(OsString, OsString)>) -> Self {
        Self {
            cli_path,
            environment,
            max_turns: MAX_TURNS_ACCEPTED,
        }
    }

    #[cfg(test)]
    pub(super) fn with_max_turns(mut self, max_turns: bool) -> Self {
        self.max_turns = max_turns;
        self
    }

    async fn run_job(&self, job: &ModelJob) -> ModelResult {
        let spawned = Command::new(&self.cli_path)
            .args(claude_args(job, self.max_turns))
            .env_clear()
            .envs(self.environment.iter().cloned())
            .current_dir(&job.folder)
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn();
        let Ok(mut child) = spawned else {
            return ModelResult::FailedToStart;
        };
        // Dropping this run (a `stop`, a replacement) kills the group too.
        let mut guard = GroupGuard(child.id());
        let result = tokio::time::timeout(job.timeout, exchange(&mut child, &job.facts_json)).await;
        // Whatever the run started must not outlive it: end the whole group, then reap.
        if let Some(group) = guard.0.take() {
            kill_group(group).await;
        }
        let _ = child.wait().await;
        match result {
            Err(_) => ModelResult::Killed,
            Ok(Some((stdout, ok))) => ModelResult::Exited { stdout, ok },
            Ok(None) => ModelResult::Exited {
                stdout: Vec::new(),
                ok: false,
            },
        }
    }
}

impl ModelRunner for ClaudeRunner {
    fn run<'a>(&'a self, job: &'a ModelJob) -> BoxFuture<'a, ModelResult> {
        Box::pin(self.run_job(job))
    }
}

/// Ends the process group of a run that is dropped before it finished.
struct GroupGuard(Option<u32>);

impl Drop for GroupGuard {
    fn drop(&mut self) {
        let Some(pid) = self.0.take() else {
            return;
        };
        let _ = std::process::Command::new("/bin/kill")
            .args(["-KILL", "--", &format!("-{pid}")])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// Writes the facts, reads the output up to the limit, and waits for the exit. `None`
/// when a pipe failed.
async fn exchange(child: &mut Child, facts: &[u8]) -> Option<(Vec<u8>, bool)> {
    let mut stdin = child.stdin.take()?;
    let stdout = child.stdout.take()?;
    let write = async move {
        // A model that exits before reading its input closes the pipe: not a failure of
        // the exchange, since its exit status says how it went.
        let _ = stdin.write_all(facts).await;
        let _ = stdin.shutdown().await;
    };
    let read = async move {
        let mut output = Vec::new();
        stdout
            .take(OUTPUT_LIMIT as u64)
            .read_to_end(&mut output)
            .await
            .ok()?;
        Some(output)
    };
    let (_, output) = tokio::join!(write, read);
    let status = child.wait().await.ok()?;
    Some((output?, status.success()))
}

/// Sends SIGKILL to every process in the group led by `pid`. Nothing to kill is not an
/// error: the group may have ended with its leader.
async fn kill_group(pid: u32) {
    let group = format!("-{pid}");
    let _ = Command::new("/bin/kill")
        .args([OsStr::new("-KILL"), OsStr::new("--"), OsStr::new(&group)])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
}

#[cfg(test)]
#[path = "runner_tests.rs"]
mod tests;
