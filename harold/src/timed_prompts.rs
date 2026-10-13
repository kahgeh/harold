//! Timed prompts: at a set local time each day Harold types a set text into the pane of
//! the Claude Code agent running in a set folder, only when that agent is idle and its
//! input box is empty. The list is `[[timed_prompts]]` in the configuration.

use std::collections::HashMap;
use std::future::Future;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use tracing::{error, info, warn};

use crate::agent::domain::{AgentSnapshot, EffectiveAgentState};
use crate::agent::snapshot::AgentSnapshotHub;
use crate::inbound::tmux::{press_enter_in_tmux_pane, type_text_into_tmux_pane};
use crate::outbound::left_out::same_folder;
use crate::settings::{
    AgentProviderSettings, ScreenAdapter, Settings, TimedPromptSettings, minute_of_day,
};
use crate::store::{self, HaroldStore, TimedPromptHandled, TimedPromptOutcome};

const SECONDS_IN_A_DAY: u32 = 86_400;

/// How many rounds after typing into a pane it still counts as busy when the snapshot
/// has not yet shown it busy: 12 rounds of 5 seconds, one minute.
const TYPED_PANE_HOLD_ROUNDS: u32 = 12;

/// The monitor health component that reports whether the last listing of tmux panes
/// worked (`set_health("inventory", ..)` in `agent/runtime.rs`).
const INVENTORY_COMPONENT: &str = "inventory";

/// How often the task looks at the clock and at each due prompt.
pub(crate) const ROUND_INTERVAL: Duration = Duration::from_secs(5);

/// The longest the task waits for `/bin/date` or for a screen capture.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(2);

/// The local date and time come from `/bin/date`: the `time` crate refuses to read the
/// local offset in a process with more than one thread.
const DATE_PROGRAM: &str = "/bin/date";
const DATE_ARGS: [&str; 1] = ["+%Y-%m-%d %H:%M:%S"];

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
/// first check that fails. A pane in `typed_into` counts as busy: Harold typed into it
/// earlier in this round or in an earlier round, and the snapshot still says idle for a
/// moment after Harold types. The round keeps a pane in `typed_into` until a round has
/// seen it not idle, for at most a minute.
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

// ---------------------------------------------------------------------------
// The ports
// ---------------------------------------------------------------------------

/// Everything a round reads or does outside this module. The daemon's implementation is
/// `TmuxPorts`; the tests use a fake.
pub(crate) trait Ports: Send + Sync + 'static {
    /// The Mac's local date and time, or `None` when it cannot be read.
    fn now(&self) -> impl Future<Output = Option<LocalNow>> + Send;

    /// The agent panes Harold tracks now.
    fn agents(&self) -> AgentsView;

    /// A fresh read of the pane's visible screen, or `None` when the read fails or takes
    /// more than 2 seconds.
    fn read_screen(&self, pane_id: &str) -> impl Future<Output = Option<String>> + Send;

    /// Types `text` into the pane as literal keys, without Enter.
    fn type_text(
        &self,
        pane_id: &str,
        text: &str,
    ) -> impl Future<Output = Result<(), String>> + Send;

    /// Sends Enter to the pane.
    fn press_enter(&self, pane_id: &str) -> impl Future<Output = Result<(), String>> + Send;
}

// ---------------------------------------------------------------------------
// Reports
// ---------------------------------------------------------------------------

/// One thing a round has to say. `log_report` writes each as a log line; none carries a
/// prompt's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Report {
    Typed {
        name: String,
        pane: String,
        /// Seconds after the prompt's time.
        late_s: u32,
    },
    /// The prompt is waiting, and for a reason other than the last one reported.
    Delayed {
        name: String,
        reason: Reason,
    },
    Skipped {
        name: String,
        reason: Reason,
    },
    /// The text was typed and Enter failed.
    Failed {
        name: String,
        pane: String,
        error: String,
    },
    /// The local time could not be read, so this round did nothing.
    ClockUnreadable,
    /// The event for a handled prompt could not be stored.
    RecordFailed {
        name: String,
        error: String,
    },
}

pub(crate) fn log_report(report: &Report) {
    match report {
        Report::Typed { name, pane, late_s } => {
            info!(name = %name, pane = %pane, late_s, "timed prompt typed");
        }
        Report::Delayed {
            name,
            reason: reason @ Reason::SeveralAgentsInFolder { agents },
        } => {
            info!(name = %name, reason = reason.as_str(), agents, "timed prompt delayed");
        }
        Report::Delayed { name, reason } => {
            info!(name = %name, reason = reason.as_str(), "timed prompt delayed");
        }
        Report::Skipped { name, reason } => {
            warn!(name = %name, reason = reason.as_str(), "timed prompt skipped");
        }
        Report::Failed { name, pane, error } => {
            warn!(name = %name, pane = %pane, error = %error, "timed prompt failed");
        }
        Report::ClockUnreadable => {
            warn!("timed prompts cannot read the local time from /bin/date");
        }
        Report::RecordFailed { name, error } => {
            error!(name = %name, error = %error, "timed prompt record not stored");
        }
    }
}

// ---------------------------------------------------------------------------
// The round
// ---------------------------------------------------------------------------

struct Prompt {
    name: String,
    window: Window,
    folder: String,
    text: String,
}

/// A prompt that was due and not typed, and the reason last reported for it.
struct Waiting {
    day: String,
    reason: Reason,
}

/// What the task remembers about one prompt between rounds.
struct Progress {
    /// The last local date the prompt was typed, skipped or failed on.
    handled_day: Option<String>,
    waiting: Option<Waiting>,
}

/// A pane Harold typed into, which counts as busy until a round sees it not idle.
struct TypedPane {
    pane_id: String,
    /// Rounds it still counts as busy for while the snapshot keeps saying idle.
    rounds_left: u32,
}

/// How one try at a due prompt ended.
enum Attempt {
    Wait(Reason),
    Typed { pane: String },
    EnterFailed { pane: String, error: String },
}

/// The configured prompts and what has become of each.
pub(crate) struct TimedPrompts<P> {
    prompts: Vec<(Prompt, Progress)>,
    ports: P,
    store: Arc<HaroldStore>,
    clock_unreadable: bool,
    typed_panes: Vec<TypedPane>,
}

impl<P: Ports> TimedPrompts<P> {
    /// `settings` has passed `Settings::validate`. `last_days` is the last date each
    /// prompt was handled on, from `HaroldStore::timed_prompt_last_days`.
    pub(crate) fn new(
        settings: &[TimedPromptSettings],
        last_days: &HashMap<String, String>,
        ports: P,
        store: Arc<HaroldStore>,
    ) -> Self {
        let prompts = settings
            .iter()
            .filter_map(|entry| {
                let at_minute = minute_of_day(&entry.at)?;
                let give_up_after_minutes = u32::try_from(entry.give_up_after_minutes).ok()?;
                let prompt = Prompt {
                    name: entry.name.clone(),
                    window: Window::new(at_minute, give_up_after_minutes),
                    folder: entry.folder.clone(),
                    text: entry.text.clone(),
                };
                let progress = Progress {
                    handled_day: last_days.get(&entry.name).cloned(),
                    waiting: None,
                };
                Some((prompt, progress))
            })
            .collect();
        Self {
            prompts,
            ports,
            store,
            clock_unreadable: false,
            typed_panes: Vec::new(),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.prompts.is_empty()
    }

    /// One pass over the prompts, in the order of the configuration list. Returns what
    /// there is to log.
    pub(crate) async fn round(&mut self) -> Vec<Report> {
        let mut reports = Vec::new();
        if self.prompts.is_empty() {
            return reports;
        }
        let Some(now) = self.ports.now().await else {
            if !std::mem::replace(&mut self.clock_unreadable, true) {
                reports.push(Report::ClockUnreadable);
            }
            return reports;
        };
        self.clock_unreadable = false;
        let view = self.ports.agents();
        // A pane typed into in an earlier round stops counting as busy once the snapshot
        // has caught up or the pane is gone, and after `TYPED_PANE_HOLD_ROUNDS` rounds.
        let typed_panes = &mut self.typed_panes;
        typed_panes.retain_mut(|typed| {
            let reads_idle = view.panes.iter().any(|pane| {
                pane.pane_id == typed.pane_id && pane.state == EffectiveAgentState::Idle
            });
            if !reads_idle || typed.rounds_left == 0 {
                return false;
            }
            typed.rounds_left -= 1;
            true
        });
        // The panes that count as busy this round. See `pick_pane`.
        let mut typed_into: Vec<String> = typed_panes
            .iter()
            .map(|typed| typed.pane_id.clone())
            .collect();
        let ports = &self.ports;
        let store: &HaroldStore = &self.store;

        for (prompt, progress) in self.prompts.iter_mut() {
            // Still waiting from an earlier date: that date's window ran to midnight, so
            // it never had a moment on its own date when it had ended.
            if let Some(waiting) = progress.waiting.take_if(|waiting| waiting.day != now.day) {
                reports.push(Report::Skipped {
                    name: prompt.name.clone(),
                    reason: waiting.reason.clone(),
                });
                let event = TimedPromptHandled {
                    name: prompt.name.clone(),
                    day: waiting.day.clone(),
                    outcome: TimedPromptOutcome::Skipped,
                    reason: Some(waiting.reason.as_str().to_string()),
                    pane_id: None,
                };
                record(store, event, &mut reports).await;
                progress.handled_day = Some(waiting.day);
            }
            if progress.handled_day.as_deref() == Some(now.day.as_str()) {
                continue;
            }
            match prompt.window.phase(now.seconds_of_day) {
                Phase::Before => {}
                Phase::Ended => {
                    let reason = progress
                        .waiting
                        .take()
                        .map_or(Reason::WindowPassed, |waiting| waiting.reason);
                    reports.push(Report::Skipped {
                        name: prompt.name.clone(),
                        reason: reason.clone(),
                    });
                    let event = TimedPromptHandled {
                        name: prompt.name.clone(),
                        day: now.day.clone(),
                        outcome: TimedPromptOutcome::Skipped,
                        reason: Some(reason.as_str().to_string()),
                        pane_id: None,
                    };
                    record(store, event, &mut reports).await;
                    progress.handled_day = Some(now.day.clone());
                }
                Phase::Due => match attempt(ports, prompt, &view, &typed_into).await {
                    Attempt::Wait(reason) => {
                        let already_reported = progress
                            .waiting
                            .as_ref()
                            .is_some_and(|waiting| waiting.reason.as_str() == reason.as_str());
                        if !already_reported {
                            reports.push(Report::Delayed {
                                name: prompt.name.clone(),
                                reason: reason.clone(),
                            });
                        }
                        progress.waiting = Some(Waiting {
                            day: now.day.clone(),
                            reason,
                        });
                    }
                    Attempt::Typed { pane } => {
                        reports.push(Report::Typed {
                            name: prompt.name.clone(),
                            pane: pane.clone(),
                            late_s: now.seconds_of_day - prompt.window.at_s,
                        });
                        let event = TimedPromptHandled {
                            name: prompt.name.clone(),
                            day: now.day.clone(),
                            outcome: TimedPromptOutcome::Typed,
                            reason: None,
                            pane_id: Some(pane.clone()),
                        };
                        record(store, event, &mut reports).await;
                        progress.handled_day = Some(now.day.clone());
                        progress.waiting = None;
                        typed_panes.push(TypedPane {
                            pane_id: pane.clone(),
                            rounds_left: TYPED_PANE_HOLD_ROUNDS,
                        });
                        typed_into.push(pane);
                    }
                    Attempt::EnterFailed { pane, error } => {
                        reports.push(Report::Failed {
                            name: prompt.name.clone(),
                            pane: pane.clone(),
                            error,
                        });
                        let event = TimedPromptHandled {
                            name: prompt.name.clone(),
                            day: now.day.clone(),
                            outcome: TimedPromptOutcome::Failed,
                            reason: None,
                            pane_id: Some(pane.clone()),
                        };
                        record(store, event, &mut reports).await;
                        // Not retried: the text is in the box, and typing it again would
                        // put it there twice.
                        progress.handled_day = Some(now.day.clone());
                        progress.waiting = None;
                        typed_panes.push(TypedPane {
                            pane_id: pane.clone(),
                            rounds_left: TYPED_PANE_HOLD_ROUNDS,
                        });
                        typed_into.push(pane);
                    }
                },
            }
        }
        reports
    }
}

/// Runs the seven checks for a due prompt and types it when all pass.
async fn attempt<P: Ports>(
    ports: &P,
    prompt: &Prompt,
    view: &AgentsView,
    typed_into: &[String],
) -> Attempt {
    let pane = match pick_pane(view, &prompt.folder, typed_into) {
        Ok(pane) => pane.pane_id.clone(),
        Err(reason) => return Attempt::Wait(reason),
    };
    let screen = ports.read_screen(&pane).await;
    if let Some(reason) = box_reason(screen.as_deref()) {
        return Attempt::Wait(reason);
    }
    // Nothing is in the box yet, so a failure here is safe to try again next round.
    if ports.type_text(&pane, &prompt.text).await.is_err() {
        return Attempt::Wait(Reason::TypingFailed);
    }
    match ports.press_enter(&pane).await {
        Ok(()) => Attempt::Typed { pane },
        Err(error) => Attempt::EnterFailed { pane, error },
    }
}

/// Appends the event. When that fails the round still counts the prompt as handled, so
/// it is not typed again while Harold keeps running.
async fn record(store: &HaroldStore, event: TimedPromptHandled, reports: &mut Vec<Report>) {
    if let Err(error) = store::append_timed_prompt_handled(store, &event).await {
        reports.push(Report::RecordFailed {
            name: event.name,
            error: error.to_string(),
        });
    }
}

// ---------------------------------------------------------------------------
// The daemon's ports
// ---------------------------------------------------------------------------

/// What `program` prints, or `None` when it cannot start, fails, prints text that is not
/// UTF-8, or takes longer than `timeout`. A command that runs out of time is killed.
async fn bounded_stdout(program: &str, args: &[&str], timeout: Duration) -> Option<String> {
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(timeout, command.output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// The visible screen of one pane, as plain text.
fn capture_args(pane_id: &str) -> [&str; 4] {
    ["capture-pane", "-p", "-t", pane_id]
}

/// Runs one of the blocking typing steps off the async threads.
async fn typing_step(
    step: impl FnOnce() -> Result<(), String> + Send + 'static,
) -> Result<(), String> {
    tokio::task::spawn_blocking(step)
        .await
        .unwrap_or_else(|error| Err(format!("typing step did not finish: {error}")))
}

/// The ports of the running daemon: `/bin/date`, the agent monitor's snapshot, and tmux.
pub(crate) struct TmuxPorts {
    snapshots: AgentSnapshotHub,
    providers: Vec<AgentProviderSettings>,
}

impl Ports for TmuxPorts {
    async fn now(&self) -> Option<LocalNow> {
        let text = bounded_stdout(DATE_PROGRAM, &DATE_ARGS, COMMAND_TIMEOUT).await?;
        parse_local_now(&text)
    }

    fn agents(&self) -> AgentsView {
        agents_view(&self.snapshots.current(), &self.providers)
    }

    async fn read_screen(&self, pane_id: &str) -> Option<String> {
        bounded_stdout("tmux", &capture_args(pane_id), COMMAND_TIMEOUT).await
    }

    async fn type_text(&self, pane_id: &str, text: &str) -> Result<(), String> {
        let (pane_id, text) = (pane_id.to_string(), text.to_string());
        typing_step(move || type_text_into_tmux_pane(&pane_id, &text)).await
    }

    async fn press_enter(&self, pane_id: &str) -> Result<(), String> {
        let pane_id = pane_id.to_string();
        typing_step(move || press_enter_in_tmux_pane(&pane_id)).await
    }
}

// ---------------------------------------------------------------------------
// The task
// ---------------------------------------------------------------------------

/// Runs a round when it starts and every `ROUND_INTERVAL` after, until shutdown. With no
/// prompts it ends at once and logs nothing. Shutdown is looked at between rounds only:
/// a round is never dropped between typing a text and pressing Enter.
pub(crate) async fn run<P: Ports>(mut prompts: TimedPrompts<P>, mut shutdown: watch::Receiver<()>) {
    if prompts.is_empty() {
        return;
    }
    let mut ticker = tokio::time::interval(ROUND_INTERVAL);
    // After the Mac wakes, one round runs; the rounds missed while asleep are not made up.
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => break,
            _ = ticker.tick() => {}
        }
        for report in prompts.round().await {
            log_report(&report);
        }
    }
}

/// Starts the task for the prompts in `settings`. The caller has already read the whole
/// event stream into the state database, so the stored dates are current.
pub(crate) async fn start(
    settings: &Settings,
    store: Arc<HaroldStore>,
    snapshots: AgentSnapshotHub,
    shutdown: watch::Receiver<()>,
) -> events::Result<JoinHandle<()>> {
    let last_days = if settings.timed_prompts.is_empty() {
        HashMap::new()
    } else {
        store.timed_prompt_last_days().await?
    };
    let ports = TmuxPorts {
        snapshots,
        providers: settings.agents.0.clone(),
    };
    let prompts = TimedPrompts::new(&settings.timed_prompts, &last_days, ports, store);
    Ok(tokio::spawn(run(prompts, shutdown)))
}

#[cfg(test)]
#[path = "timed_prompts_tests.rs"]
mod tests;
