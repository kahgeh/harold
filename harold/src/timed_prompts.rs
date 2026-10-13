//! Timed prompts: at a set local time each day Harold types a set text into the pane of
//! the Claude Code agent running in a set folder, only when that agent is idle and its
//! input box is empty. The list is `[[timed_prompts]]` in the configuration.

#![allow(
    dead_code,
    reason = "wired into the daemon in the last timed-prompts code task"
)]

use crate::agent::domain::{AgentSnapshot, EffectiveAgentState};
use crate::outbound::left_out::same_folder;
use crate::settings::{AgentProviderSettings, ScreenAdapter};

const SECONDS_IN_A_DAY: u32 = 86_400;

/// The monitor health component that reports whether the last listing of tmux panes
/// worked (`set_health("inventory", ..)` in `agent/runtime.rs`).
const INVENTORY_COMPONENT: &str = "inventory";

// ---------------------------------------------------------------------------
// The clock
// ---------------------------------------------------------------------------

/// The Mac's local date and time of day.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocalNow {
    /// The local date, such as `2026-10-14`.
    pub day: String,
    /// Seconds since local midnight, 0 to 86,399.
    pub seconds_of_day: u32,
}

fn two_digits(text: &str) -> Option<u32> {
    (text.len() == 2 && text.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| text.parse().ok())
        .flatten()
}

/// Reads the output of `/bin/date "+%Y-%m-%d %H:%M:%S"`.
pub(crate) fn parse_local_now(text: &str) -> Option<LocalNow> {
    let (day, time) = text.trim().split_once(' ')?;
    let day_is_a_date = day.len() == 10
        && day.bytes().enumerate().all(|(index, byte)| {
            if index == 4 || index == 7 {
                byte == b'-'
            } else {
                byte.is_ascii_digit()
            }
        });
    let mut parts = time.split(':');
    let hours = two_digits(parts.next()?)?;
    let minutes = two_digits(parts.next()?)?;
    let seconds = two_digits(parts.next()?)?;
    if !day_is_a_date || parts.next().is_some() || hours > 23 || minutes > 59 || seconds > 60 {
        return None;
    }
    Some(LocalNow {
        day: day.to_string(),
        seconds_of_day: hours * 3600 + minutes * 60 + seconds.min(59),
    })
}

// ---------------------------------------------------------------------------
// The window
// ---------------------------------------------------------------------------

/// Where a time of day falls against a prompt's window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    /// Earlier than the prompt's time.
    Before,
    /// At or after the prompt's time and before the window's end.
    Due,
    /// At or after the window's end.
    Ended,
}

/// The part of each day a prompt stays due for, in seconds since local midnight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Window {
    pub at_s: u32,
    /// `at` plus the give-up time, or midnight if that comes first.
    pub end_s: u32,
}

impl Window {
    pub(crate) fn new(at_minute: u32, give_up_after_minutes: u32) -> Self {
        Self {
            at_s: at_minute * 60,
            end_s: ((at_minute + give_up_after_minutes) * 60).min(SECONDS_IN_A_DAY),
        }
    }

    pub(crate) fn phase(&self, seconds_of_day: u32) -> Phase {
        if seconds_of_day < self.at_s {
            Phase::Before
        } else if seconds_of_day < self.end_s {
            Phase::Due
        } else {
            Phase::Ended
        }
    }
}

// ---------------------------------------------------------------------------
// The input box
// ---------------------------------------------------------------------------

/// What a screen shows in Claude Code's input box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputBox {
    Empty,
    HoldsText,
    NotFound,
}

/// Reads Claude Code's input box from a pane's visible screen. The box is the lowest row
/// that starts with `❯`, when the rows directly above and below it start with `─`. It is
/// empty when nothing but spaces follows the `❯`. Anything else is not found, so doubt
/// always means not typing: a box more than one row tall, a permission question or a
/// menu in its place.
pub(crate) fn read_input_box(screen: &str) -> InputBox {
    let rows: Vec<&str> = screen
        .lines()
        .map(|row| row.trim_end_matches('\r'))
        .collect();
    let Some(row) = rows.iter().rposition(|row| row.starts_with('❯')) else {
        return InputBox::NotFound;
    };
    let is_rule = |index: Option<usize>| {
        index
            .and_then(|index| rows.get(index))
            .is_some_and(|row| row.starts_with('─'))
    };
    if !is_rule(row.checked_sub(1)) || !is_rule(Some(row + 1)) {
        return InputBox::NotFound;
    }
    let typed = &rows[row]['❯'.len_utf8()..];
    if typed.chars().all(char::is_whitespace) {
        InputBox::Empty
    } else {
        InputBox::HoldsText
    }
}

// ---------------------------------------------------------------------------
// Reasons
// ---------------------------------------------------------------------------

/// Why a due prompt was not typed, or why it was skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Reason {
    PaneListFailing,
    NoAgentInFolder,
    SeveralAgentsInFolder {
        agents: usize,
    },
    /// The folder's agent is not Claude Code, the only agent whose input box Harold reads.
    InputBoxUnreadable,
    AgentBusy,
    AgentStateUnknown,
    InputBoxNotFound,
    InputBoxHoldsText,
    /// Every check passed and typing the text failed.
    TypingFailed,
    /// The window ended and Harold never saw the prompt due.
    WindowPassed,
}

impl Reason {
    /// The name the log line and the stored event carry.
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::PaneListFailing => "pane_list_failing",
            Self::NoAgentInFolder => "no_agent_in_folder",
            Self::SeveralAgentsInFolder { .. } => "several_agents_in_folder",
            Self::InputBoxUnreadable => "input_box_unreadable",
            Self::AgentBusy => "agent_busy",
            Self::AgentStateUnknown => "agent_state_unknown",
            Self::InputBoxNotFound => "input_box_not_found",
            Self::InputBoxHoldsText => "input_box_holds_text",
            Self::TypingFailed => "typing_failed",
            Self::WindowPassed => "window_passed",
        }
    }
}

// ---------------------------------------------------------------------------
// The panes
// ---------------------------------------------------------------------------

/// One agent pane, with what the checks need to know about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentPane {
    pub pane_id: String,
    /// The working directory tmux reports for the pane.
    pub folder: String,
    /// Whether the pane's provider uses the `claude-v1` screen adapter.
    pub reads_as_claude: bool,
    pub state: EffectiveAgentState,
}

/// The agent panes Harold tracks, and whether its last listing of tmux panes failed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct AgentsView {
    pub listing_failed: bool,
    pub panes: Vec<AgentPane>,
}

pub(crate) fn agents_view(
    snapshot: &AgentSnapshot,
    providers: &[AgentProviderSettings],
) -> AgentsView {
    let reads_as_claude = |provider_id: &str| {
        providers.iter().any(|provider| {
            provider.id == provider_id && provider.screen_adapter == ScreenAdapter::ClaudeV1
        })
    };
    AgentsView {
        listing_failed: snapshot
            .monitor_health
            .iter()
            .any(|health| health.component == INVENTORY_COMPONENT && !health.healthy),
        panes: snapshot
            .panes
            .iter()
            .map(|projection| AgentPane {
                pane_id: projection.pane.incarnation.pane_id.clone(),
                folder: projection.pane.working_directory.clone(),
                reads_as_claude: reads_as_claude(&projection.pane.incarnation.provider_id),
                state: projection.effective_state,
            })
            .collect(),
    }
}

/// Checks 1 to 5: the pane a prompt for `folder` is typed into, or the reason of the
/// first check that fails. A pane in `typed_into` was typed into earlier in this round
/// and counts as busy: the snapshot still says idle for a moment after Harold types.
pub(crate) fn pick_pane<'a>(
    view: &'a AgentsView,
    folder: &str,
    typed_into: &[String],
) -> Result<&'a AgentPane, Reason> {
    if view.listing_failed {
        return Err(Reason::PaneListFailing);
    }
    let mut in_folder = view
        .panes
        .iter()
        .filter(|pane| same_folder(&pane.folder, folder));
    let Some(pane) = in_folder.next() else {
        return Err(Reason::NoAgentInFolder);
    };
    let others = in_folder.count();
    if others > 0 {
        return Err(Reason::SeveralAgentsInFolder { agents: others + 1 });
    }
    if !pane.reads_as_claude {
        return Err(Reason::InputBoxUnreadable);
    }
    match pane.state {
        EffectiveAgentState::Busy => Err(Reason::AgentBusy),
        EffectiveAgentState::Unknown => Err(Reason::AgentStateUnknown),
        EffectiveAgentState::Idle if typed_into.contains(&pane.pane_id) => Err(Reason::AgentBusy),
        EffectiveAgentState::Idle => Ok(pane),
    }
}

/// Checks 6 and 7 on a fresh read of the pane's screen, where `None` is a read that
/// failed or timed out. Returns the reason of the check that fails, or `None` when the
/// input box is there and empty.
pub(crate) fn box_reason(screen: Option<&str>) -> Option<Reason> {
    match screen.map(read_input_box) {
        None | Some(InputBox::NotFound) => Some(Reason::InputBoxNotFound),
        Some(InputBox::HoldsText) => Some(Reason::InputBoxHoldsText),
        Some(InputBox::Empty) => None,
    }
}

#[cfg(test)]
#[path = "timed_prompts_tests.rs"]
mod tests;
