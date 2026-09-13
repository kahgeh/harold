#![allow(
    dead_code,
    reason = "the visible-screen adapter is consumed by the monitor runtime slice"
)]

use std::io;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::settings::{AgentProviderSettings, ScreenAdapter};
use sha2::{Digest, Sha256};

#[path = "screen_capture.rs"]
mod capture;
#[path = "screen_codex.rs"]
mod codex;
use capture::{CaptureRequest, CaptureScope, PaneCapturePort, StyledPaneCapture, TmuxPaneCapture};

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

    fn scan_prompts(
        &self,
        _pane: &AgentPaneObservation,
        _provider: &AgentProviderSettings,
    ) -> Result<PromptScan, ScreenError> {
        Ok(PromptScan::default())
    }
}

#[derive(Clone, Default)]
pub(crate) struct PromptScan {
    pub(crate) blocks: Vec<PromptBlock>,
}

#[derive(Clone)]
pub(crate) struct PromptBlock {
    pub(crate) fingerprint: [u8; 32],
    pub(crate) candidate: Option<String>,
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
        capture::bounded_output(
            program,
            args,
            std::time::Duration::from_secs(2),
            capture::MAX_CAPTURE_BYTES,
        )
    }
}

pub(crate) struct TmuxVisibleScreen<R = SystemCommandRunner> {
    capture: TmuxPaneCapture<R>,
    now_ms: fn() -> i64,
}

impl TmuxVisibleScreen<SystemCommandRunner> {
    pub(crate) fn new() -> Self {
        Self {
            capture: TmuxPaneCapture {
                runner: SystemCommandRunner,
            },
            now_ms: system_now_ms,
        }
    }
}

impl<R> TmuxVisibleScreen<R> {
    pub(super) fn with_runner(runner: R, now_ms: fn() -> i64) -> Self {
        Self {
            capture: TmuxPaneCapture { runner },
            now_ms,
        }
    }
}

impl<R: CommandRunner> VisibleScreenPort for TmuxVisibleScreen<R> {
    fn observe(
        &self,
        pane: &AgentPaneObservation,
        provider: &AgentProviderSettings,
    ) -> Result<ScreenObservation, ScreenError> {
        let captured = self.capture.capture(
            &pane.incarnation.pane_id,
            CaptureRequest {
                scope: CaptureScope::Visible,
                preserve_styles: false,
            },
        )?;
        Ok(ScreenObservation {
            incarnation: pane.incarnation.clone(),
            state: adapter(provider).classify_visible(&captured),
            fallback_summary: None,
            classifier_id: CLASSIFIER_ID.to_string(),
            observed_at_ms: (self.now_ms)(),
        })
    }

    fn scan_prompts(
        &self,
        pane: &AgentPaneObservation,
        provider: &AgentProviderSettings,
    ) -> Result<PromptScan, ScreenError> {
        let captured = self.capture.capture(
            &pane.incarnation.pane_id,
            CaptureRequest {
                scope: CaptureScope::RecentHistory {
                    lines: provider.screen_history_lines,
                },
                preserve_styles: true,
            },
        )?;
        Ok(adapter(provider).scan_prompts(&captured))
    }
}

trait ProviderScreenAdapter {
    fn classify_visible(&self, capture: &StyledPaneCapture) -> Option<ObservedAgentState>;
    fn scan_prompts(&self, capture: &StyledPaneCapture) -> PromptScan;
}

fn adapter(provider: &AgentProviderSettings) -> Box<dyn ProviderScreenAdapter + '_> {
    match provider.screen_adapter {
        ScreenAdapter::GenericV1 => Box::new(GenericAdapter(provider)),
        ScreenAdapter::CodexV1 => Box::new(codex::CodexAdapter(provider)),
    }
}

struct GenericAdapter<'a>(&'a AgentProviderSettings);

impl ProviderScreenAdapter for GenericAdapter<'_> {
    fn classify_visible(&self, capture: &StyledPaneCapture) -> Option<ObservedAgentState> {
        classify_state(&capture.text, self.0)
    }

    fn scan_prompts(&self, capture: &StyledPaneCapture) -> PromptScan {
        let text = normalize_visible_grid(&capture.text);
        let blocks = text
            .lines()
            .filter_map(|line| {
                self.0
                    .summary_line_prefixes
                    .iter()
                    .find_map(|prefix| line.strip_prefix(prefix))
                    .map(|candidate| prompt_block(candidate, &self.0.idle_all))
            })
            .collect();
        PromptScan { blocks }
    }
}

fn prompt_block(text: &str, placeholders: &[String]) -> PromptBlock {
    // Fingerprint the full normalized block, not its truncated public candidate.
    let normalized = normalize_visible_grid(text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    PromptBlock {
        fingerprint: Sha256::digest(normalized.as_bytes()).into(),
        candidate: normalize_fallback_summary(&normalized, placeholders),
    }
}

fn classify_state(text: &str, provider: &AgentProviderSettings) -> Option<ObservedAgentState> {
    let visible_grid = normalize_visible_grid(text);
    if clause_matches(&visible_grid, &provider.busy_all) {
        return Some(ObservedAgentState::Busy);
    }
    clause_matches(&visible_grid, &provider.idle_all).then_some(ObservedAgentState::Idle)
}

#[cfg(test)]
pub(super) fn observe_visible_text(
    pane: &AgentPaneObservation,
    provider: &AgentProviderSettings,
    visible_text: &str,
    observed_at_ms: i64,
) -> ScreenObservation {
    let capture = StyledPaneCapture {
        text: visible_text.to_string(),
        request: CaptureRequest {
            scope: CaptureScope::Visible,
            preserve_styles: false,
        },
    };
    let adapter = GenericAdapter(provider);
    ScreenObservation {
        incarnation: pane.incarnation.clone(),
        state: adapter.classify_visible(&capture),
        fallback_summary: adapter
            .scan_prompts(&capture)
            .blocks
            .into_iter()
            .rev()
            .find_map(|block| block.candidate),
        classifier_id: CLASSIFIER_ID.to_string(),
        observed_at_ms,
    }
}

pub(crate) fn normalize_fallback_summary(input: &str, idle_fragments: &[String]) -> Option<String> {
    let normalized = normalize_visible_grid(input)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let is_placeholder = idle_fragments.iter().any(|fragment| {
        normalize_visible_grid(fragment)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            == normalized
    });
    if is_placeholder {
        return None;
    }
    normalize_work_summary(&normalized)
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
