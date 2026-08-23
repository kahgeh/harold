#![allow(
    dead_code,
    reason = "the visible-screen adapter is consumed by the monitor runtime slice"
)]

use std::io;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::settings::AgentProviderSettings;

use super::domain::{AgentPaneObservation, ObservedAgentState, ScreenObservation};
use super::summary::{normalize_visible_grid, normalize_work_summary};

const CLASSIFIER_ID: &str = "tmux-visible-v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScreenError {
    CaptureUnavailable,
    CaptureFailed,
    PaneDeparted,
}

pub(crate) trait VisibleScreenPort: Send + Sync {
    fn observe(
        &self,
        pane: &AgentPaneObservation,
        provider: &AgentProviderSettings,
    ) -> Result<ScreenObservation, ScreenError>;
}

pub(super) struct CommandOutput {
    pub(super) success: bool,
    pub(super) stdout: Vec<u8>,
}

pub(super) trait CommandRunner: Send + Sync {
    fn output(&self, program: &str, args: &[&str]) -> io::Result<CommandOutput>;
}

pub(crate) struct SystemCommandRunner;

impl CommandRunner for SystemCommandRunner {
    fn output(&self, program: &str, args: &[&str]) -> io::Result<CommandOutput> {
        let output = Command::new(program)
            .args(args)
            .stderr(Stdio::null())
            .output()?;
        Ok(CommandOutput {
            success: output.status.success(),
            stdout: output.stdout,
        })
    }
}

pub(crate) struct TmuxVisibleScreen<R = SystemCommandRunner> {
    runner: R,
    now_ms: fn() -> i64,
}

impl TmuxVisibleScreen<SystemCommandRunner> {
    pub(crate) fn new() -> Self {
        Self {
            runner: SystemCommandRunner,
            now_ms: system_now_ms,
        }
    }
}

impl<R> TmuxVisibleScreen<R> {
    pub(super) fn with_runner(runner: R, now_ms: fn() -> i64) -> Self {
        Self { runner, now_ms }
    }
}

impl<R: CommandRunner> VisibleScreenPort for TmuxVisibleScreen<R> {
    fn observe(
        &self,
        pane: &AgentPaneObservation,
        provider: &AgentProviderSettings,
    ) -> Result<ScreenObservation, ScreenError> {
        let pane_id = pane.incarnation.pane_id.as_str();
        let output = self
            .runner
            .output("tmux", &["capture-pane", "-p", "-S", "0", "-t", pane_id])
            .map_err(|error| {
                if error.kind() == io::ErrorKind::NotFound {
                    ScreenError::CaptureUnavailable
                } else {
                    ScreenError::CaptureFailed
                }
            })?;
        if !output.success {
            return Err(ScreenError::CaptureFailed);
        }

        let captured = String::from_utf8_lossy(&output.stdout);
        Ok(observe_visible_text(
            pane,
            provider,
            &captured,
            (self.now_ms)(),
        ))
    }
}

pub(super) fn observe_visible_text(
    pane: &AgentPaneObservation,
    provider: &AgentProviderSettings,
    visible_text: &str,
    observed_at_ms: i64,
) -> ScreenObservation {
    let visible_grid = normalize_visible_grid(visible_text);
    let busy = clause_matches(&visible_grid, &provider.busy_all);
    let idle = clause_matches(&visible_grid, &provider.idle_all);
    let state = if busy {
        Some(ObservedAgentState::Busy)
    } else if idle {
        Some(ObservedAgentState::Idle)
    } else {
        None
    };
    let fallback_summary = visible_grid.lines().rev().find_map(|line| {
        provider
            .summary_line_prefixes
            .iter()
            .find_map(|prefix| line.strip_prefix(prefix))
            .and_then(normalize_work_summary)
            .filter(|summary| !clause_matches(summary, &provider.idle_all))
    });

    ScreenObservation {
        incarnation: pane.incarnation.clone(),
        state,
        fallback_summary,
        classifier_id: CLASSIFIER_ID.to_string(),
        observed_at_ms,
    }
}

fn clause_matches(visible_grid: &str, fragments: &[String]) -> bool {
    !fragments.is_empty()
        && fragments
            .iter()
            .all(|fragment| visible_grid.contains(fragment))
}

fn system_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(i64::MAX)
}
