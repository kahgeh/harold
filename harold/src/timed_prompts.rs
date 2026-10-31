//! Timed prompts: at a set local time each day Harold types a set text into the pane of
//! the Claude Code agent running in a set folder, only when that agent is idle and its
//! input box is empty. The list is `[[timed_prompts]]` in the configuration.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, LazyLock, PoisonError};
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior};
use tracing::{error, info, warn};

use crate::agent::domain::{AgentSnapshot, EffectiveAgentState};
use crate::agent::snapshot::AgentSnapshotHub;
use crate::inbound::tmux::{press_enter_in_tmux_pane, type_text_into_tmux_pane};
use crate::outbound::left_out::{
    Chief, ChiefLookup, PanePosition, lookup_in_folder, resolve_chief, same_folder,
};
use crate::settings::{
    AgentMonitorSettings, AgentProviderSettings, ScreenAdapter, Settings, TimedPromptSettings,
    minute_of_day,
};
use crate::store::{self, HaroldStore, TimedPromptHandled, TimedPromptOutcome};

const SECONDS_IN_A_DAY: u32 = 86_400;

/// How many rounds after typing into a pane it still counts as busy when the snapshot
/// has not yet shown it busy: 12 rounds of 5 seconds, one minute.
pub(crate) const TYPED_PANE_HOLD_ROUNDS: u32 = 12;

/// How long a pane another writer typed into counts as busy while it still reads idle:
/// the same minute as `TYPED_PANE_HOLD_ROUNDS` rounds.
pub(crate) const TYPED_PANE_HOLD: Duration = ROUND_INTERVAL.saturating_mul(TYPED_PANE_HOLD_ROUNDS);

/// The monitor health component that reports whether the last listing of tmux panes
/// worked (`set_health("inventory", ..)` in `agent/runtime.rs`).
const INVENTORY_COMPONENT: &str = "inventory";

/// How often the task looks at the clock and at each due prompt.
pub(crate) const ROUND_INTERVAL: Duration = Duration::from_secs(5);

/// The longest the task waits for `/bin/date` or for a screen capture.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(2);

/// How long Claude Code gets to redraw its footer after Harold presses `i`.
const INSERT_REDRAW: Duration = Duration::from_millis(200);

/// How many times Harold reads the screen after it presses `i`, `INSERT_REDRAW` apart.
const INSERT_READS: usize = 5;

/// How long after Harold pressed `i` a lone `i` in the box still counts as Harold's own.
const STRAY_INSERT_KEY_AGE: Duration = Duration::from_secs(60);

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
    /// The box is empty and Claude Code's editor is Vim, but the footer does not show
    /// `-- INSERT --`: the box is in NORMAL mode, where a typed key is a Vim command.
    NotInsert,
    NotFound,
}

/// The footer text Claude Code shows below the input box in Vim INSERT mode.
const INSERT_FOOTER: &str = "-- INSERT --";

/// What the input box holds, read from a screen.
struct BoxRead {
    /// The text in the box, without the text drawn dim (a suggested prompt) or the cursor
    /// (a reverse-video space, or a reverse-video letter that dim text follows directly).
    typed: String,
    /// Whether a row below the lower rule holds `-- INSERT --`.
    insert_footer: bool,
}

/// One row of a screen with its escape codes removed, and the text the row holds that is
/// typed: not dim, and not the cursor.
struct Row {
    plain: String,
    plain_unstyled: String,
}

/// Reads one row of a `capture-pane -e` screen. Only SGR codes change what is dim (2 until
/// 22) or in reverse video (7 until 27); 0 and an empty code reset both. Every other
/// escape sequence is dropped. A row with no escape codes reads as it is. Reverse-video
/// text is typed text, except a space and except one letter that dim text follows
/// directly: that is the cursor on the first letter of a suggested prompt.
fn read_row(raw: &str) -> Row {
    let mut plain = String::new();
    let mut plain_unstyled = String::new();
    let (mut dim, mut reverse) = (false, false);
    // The reverse-video letters since the last other character. Dim text that follows
    // them directly drops one letter; any other character keeps them.
    let mut cursor_letters = String::new();
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            plain.push(c);
            let kept = std::mem::take(&mut cursor_letters);
            if !(dim && kept.chars().count() == 1) {
                plain_unstyled.push_str(&kept);
            }
            if dim {
                continue;
            }
            if !reverse {
                plain_unstyled.push(c);
            } else if c != ' ' {
                cursor_letters.push(c);
            }
            continue;
        }
        match chars.peek().copied() {
            Some('[') => {
                chars.next();
                let mut params = String::new();
                let mut last = None;
                for next in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&next) {
                        last = Some(next);
                        break;
                    }
                    params.push(next);
                }
                if last == Some('m') {
                    apply_sgr(&params, &mut dim, &mut reverse);
                }
            }
            Some(']') => {
                // An operating system command ends with BEL or ESC \.
                chars.next();
                while let Some(next) = chars.next() {
                    if next == '\u{7}' {
                        break;
                    }
                    if next == '\x1b' {
                        chars.next();
                        break;
                    }
                }
            }
            Some(_) => {
                chars.next();
            }
            None => {}
        }
    }
    plain_unstyled.push_str(&cursor_letters);
    Row {
        plain,
        plain_unstyled,
    }
}

fn apply_sgr(params: &str, dim: &mut bool, reverse: &mut bool) {
    let mut codes = params.split([';', ':']);
    while let Some(code) = codes.next() {
        match code {
            "" | "0" => (*dim, *reverse) = (false, false),
            "2" => *dim = true,
            "22" => *dim = false,
            "7" => *reverse = true,
            "27" => *reverse = false,
            // A colour with its own arguments: 38;5;n or 38;2;r;g;b.
            "38" | "48" | "58" => match codes.next() {
                Some("5") => {
                    codes.next();
                }
                Some("2") => {
                    codes.nth(2);
                }
                _ => {}
            },
            _ => {}
        }
    }
}

/// Reads Claude Code's input box from a pane's visible screen. The box is the lowest row
/// that starts with `❯`, when the rows directly above and below it start with `─`. Anything
/// else is not found, so doubt always means not typing: a box more than one row tall, a
/// permission question or a menu in its place.
fn read_box(screen: &str) -> Option<BoxRead> {
    let rows: Vec<Row> = screen
        .lines()
        .map(|row| read_row(row.trim_end_matches('\r')))
        .collect();
    let row = rows.iter().rposition(|row| row.plain.starts_with('❯'))?;
    let is_rule = |index: Option<usize>| {
        index
            .and_then(|index| rows.get(index))
            .is_some_and(|row| row.plain.starts_with('─'))
    };
    if !is_rule(row.checked_sub(1)) || !is_rule(Some(row + 1)) {
        return None;
    }
    // A row with no escape codes keeps its text whole; the `❯` itself is never text.
    let typed = rows[row].plain_unstyled.trim_start_matches('❯').to_string();
    let insert_footer = rows[row + 2..]
        .iter()
        .any(|row| row.plain.contains(INSERT_FOOTER));
    Some(BoxRead {
        typed,
        insert_footer,
    })
}

/// Reads the input box on a screen taken with `capture-pane -e`. The box is empty when
/// nothing but spaces follows the `❯` once the dim text (a suggested prompt) and the
/// cursor are left out. With `vim`, Claude Code's editor is Vim, and an empty
/// box is `Empty` only when the footer shows `-- INSERT --`; else it is `NotInsert`. Text in
/// the box is `HoldsText` in either mode.
pub(crate) fn read_input_box(screen: &str, vim: bool) -> InputBox {
    let Some(read) = read_box(screen) else {
        return InputBox::NotFound;
    };
    if !read.typed.chars().all(char::is_whitespace) {
        InputBox::HoldsText
    } else if vim && !read.insert_footer {
        InputBox::NotInsert
    } else {
        InputBox::Empty
    }
}

/// Whether the box holds the one letter `key` and nothing else.
fn box_holds_only(screen: &str, key: char) -> bool {
    read_box(screen).is_some_and(|read| read.typed.trim() == key.to_string())
}

/// Whether Claude Code's `editorMode` is `vim`, in `settings.json` in `config_dir`. A file
/// that is missing or cannot be read counts as not `vim`.
fn editor_mode_is_vim(config_dir: &Path) -> bool {
    std::fs::read_to_string(config_dir.join("settings.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|settings| {
            settings
                .get("editorMode")
                .and_then(serde_json::Value::as_str)
                .map(|mode| mode == "vim")
        })
        .unwrap_or(false)
}

/// Claude Code's config folder: `$CLAUDE_CONFIG_DIR`, else `~/.claude`.
fn claude_config_dir() -> Option<PathBuf> {
    match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) if !dir.is_empty() => Some(PathBuf::from(dir)),
        _ => std::env::var_os("HOME").map(|home| Path::new(&home).join(".claude")),
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
    /// Claude Code's editor is Vim and the empty box is not in INSERT mode, and typing `i`
    /// did not put it there.
    InputBoxNotInsert,
    /// Every check passed and typing the text failed.
    TypingFailed,
    /// The window ended and Harold never saw the prompt due.
    WindowPassed,
    /// Harold created a new store after the prompt's time today.
    NewStore,
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
            Self::InputBoxNotInsert => "input_box_not_insert",
            Self::TypingFailed => "typing_failed",
            Self::WindowPassed => "window_passed",
            Self::NewStore => "new_store",
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
    /// The tmux session the pane is in.
    pub session: String,
    pub window_index: u32,
    pub pane_index: u32,
    /// When the agent process started.
    pub started_at_ms: i64,
}

impl AgentPane {
    pub(crate) fn position(&self) -> PanePosition<'_> {
        PanePosition {
            pane_id: &self.pane_id,
            folder: &self.folder,
            session: &self.session,
            window_index: self.window_index,
            pane_index: self.pane_index,
            started_at_ms: self.started_at_ms,
        }
    }
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
                session: projection.pane.session_name.clone(),
                window_index: projection.pane.window_index,
                pane_index: projection.pane.pane_index,
                started_at_ms: projection.pane.incarnation.agent_started_at_ms,
            })
            .collect(),
    }
}

/// The agent pane the writers type into for `folder`: for the chief's folder the chief
/// (`resolve_chief`), for any other folder its only agent (`lookup_in_folder`), or the
/// reason there is none.
fn target_pane<'a>(
    view: &'a AgentsView,
    folder: &str,
    chief: Chief<'_>,
) -> Result<&'a AgentPane, Reason> {
    let is_chief_folder = chief.folder.is_some_and(|entry| {
        folder == entry || same_folder(folder, entry) || same_folder(entry, folder)
    });
    let lookup = if is_chief_folder {
        resolve_chief(
            view.panes.iter().map(AgentPane::position),
            chief.folder,
            chief.session,
        )
    } else {
        lookup_in_folder(
            view.panes
                .iter()
                .map(|pane| (pane.pane_id.as_str(), pane.folder.as_str())),
            folder,
        )
    };
    let pane_id = match lookup {
        ChiefLookup::None => return Err(Reason::NoAgentInFolder),
        ChiefLookup::Ambiguous { count } => {
            return Err(Reason::SeveralAgentsInFolder { agents: count });
        }
        ChiefLookup::Found(pane_id) => pane_id,
    };
    view.panes
        .iter()
        .find(|pane| pane.pane_id == pane_id)
        .ok_or(Reason::NoAgentInFolder)
}

/// Checks 1 to 5: the pane a prompt for `folder` is typed into, or the reason of the
/// first check that fails. In the chief's folder (`chief`) several agents are fine: the
/// pane is the chief's. In any other folder several agents fail check 3. A pane in
/// `typed_into` counts as busy: Harold typed into it
/// earlier in this round or in an earlier round, and the snapshot still says idle for a
/// moment after Harold types. The round keeps a pane in `typed_into` until a round has
/// seen it not idle, for at most a minute. A Claude Code pane that is `Busy` is picked only
/// when `busy_ok`, which only a hand-over from a call sets (`Writer::HandOver`).
pub(crate) fn pick_pane<'a>(
    view: &'a AgentsView,
    folder: &str,
    chief: Chief<'_>,
    typed_into: &[String],
    busy_ok: bool,
) -> Result<&'a AgentPane, Reason> {
    if view.listing_failed {
        return Err(Reason::PaneListFailing);
    }
    let pane = target_pane(view, folder, chief)?;
    if !pane.reads_as_claude {
        return Err(Reason::InputBoxUnreadable);
    }
    match pane.state {
        EffectiveAgentState::Busy if busy_ok => Ok(pane),
        EffectiveAgentState::Busy => Err(Reason::AgentBusy),
        EffectiveAgentState::Unknown => Err(Reason::AgentStateUnknown),
        EffectiveAgentState::Idle if typed_into.contains(&pane.pane_id) => Err(Reason::AgentBusy),
        EffectiveAgentState::Idle => Ok(pane),
    }
}

/// Checks 6 and 7 on a fresh read of the pane's screen, where `None` is a read that
/// failed or timed out, and `vim` says whether Claude Code's editor is Vim. Returns the
/// reason of the check that fails, or `None` when the input box is there and empty and in
/// INSERT mode (always so when the editor is not Vim).
pub(crate) fn box_reason(screen: Option<&str>, vim: bool) -> Option<Reason> {
    match screen.map(|screen| read_input_box(screen, vim)) {
        None | Some(InputBox::NotFound) => Some(Reason::InputBoxNotFound),
        Some(InputBox::HoldsText) => Some(Reason::InputBoxHoldsText),
        Some(InputBox::NotInsert) => Some(Reason::InputBoxNotInsert),
        Some(InputBox::Empty) => None,
    }
}

// ---------------------------------------------------------------------------
// One writer at a time
// ---------------------------------------------------------------------------

/// Who types into a pane: timed prompts and alerts through `type_when_ready`, phone
/// replies through `inbound::tmux::relay_to_tmux_pane`, and a spoken turn of a call
/// with the chief through `type_when_ready`, a hand-over from such a call through
/// `type_when_ready`, and a tool (named, such as the question tool)
/// through `type_line_once`, which calls `type_when_ready`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Writer {
    TimedPrompt,
    Alert,
    PhoneReply,
    #[cfg_attr(not(feature = "calls"), allow(dead_code))]
    Voice,
    /// A hand-over: the call answerer's request for the chief. Like a spoken turn it is not
    /// held back by another writer's hold, and it is the one writer that may type into a
    /// Claude Code pane that is `Busy`.
    HandOver,
    /// A tool typing a line, by the tool's name: the question tool types its batch of
    /// answers into the chief's pane as `Tool("questions")`.
    Tool(&'static str),
}

/// One pane's lock and what the writers share about the pane.
pub(crate) type PaneLock = tokio::sync::Mutex<PaneRecord>;

/// What the writers share about one pane.
#[derive(Default)]
pub(crate) struct PaneRecord {
    /// The writer that last typed into the pane, and when its hold ends.
    typed: Option<(Writer, Instant)>,
    /// When Harold pressed `i` in this pane with no line typed after it. A lone `i` in the
    /// box soon after is Harold's own and Harold takes it out.
    insert_key_at: Option<Instant>,
}

impl PaneRecord {
    /// Whether the pane counts as busy for `writer` because another writer typed into it
    /// a moment ago. Each writer keeps its own hold on the panes it typed into. A spoken
    /// turn and a hand-over are never held back, as a phone reply is not: the caller is
    /// waiting on the line.
    fn held_against(&self, writer: Writer, now: Instant) -> bool {
        !matches!(writer, Writer::Voice | Writer::HandOver)
            && self
                .typed
                .is_some_and(|(by, until)| by != writer && now < until)
    }

    /// Records that `writer` has just typed into the pane: for a minute, while the pane
    /// still reads idle, it counts as busy for the other writers.
    pub(crate) fn typed_by(&mut self, writer: Writer) {
        self.typed = Some((writer, Instant::now() + TYPED_PANE_HOLD));
    }
}

/// One lock for each pane, held across the checks, the typing and Enter, so two writers
/// never put their keys into one pane at once. The map's own lock is never held across an
/// await.
#[derive(Default)]
pub(crate) struct PaneLocks {
    panes: std::sync::Mutex<HashMap<String, Arc<PaneLock>>>,
}

impl PaneLocks {
    pub(crate) fn for_pane(&self, pane_id: &str) -> Arc<PaneLock> {
        let mut panes = self.panes.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        // Forgets the panes no writer holds or waits for that have no hold or recent `i` left.
        panes.retain(|_, lock| {
            Arc::strong_count(lock) > 1
                || lock.try_lock().map_or(true, |record| {
                    record.typed.is_some_and(|(_, until)| now < until)
                        || record
                            .insert_key_at
                            .is_some_and(|at| now < at + STRAY_INSERT_KEY_AGE)
                })
        });
        Arc::clone(panes.entry(pane_id.to_string()).or_default())
    }

    /// Ends the hold on each pane `view` shows not idle or does not show: the agent took
    /// the text, or the pane is gone. A pane a writer holds now is left alone.
    fn observe(&self, view: &AgentsView) {
        if view.listing_failed {
            return;
        }
        let panes = self.panes.lock().unwrap_or_else(PoisonError::into_inner);
        for (pane_id, lock) in panes.iter() {
            if reads_idle(view, pane_id) {
                continue;
            }
            if let Ok(mut record) = lock.try_lock() {
                record.typed = None;
            }
        }
    }
}

fn reads_idle(view: &AgentsView, pane_id: &str) -> bool {
    view.panes
        .iter()
        .any(|pane| pane.pane_id == pane_id && pane.state == EffectiveAgentState::Idle)
}

/// The daemon's pane locks, shared by the timed prompts, the alerts and the phone relay.
static TMUX_PANE_LOCKS: LazyLock<PaneLocks> = LazyLock::new(PaneLocks::default);

/// The daemon's lock for one pane, for a writer that does not go through `Ports`: the
/// phone relay. `TmuxPorts` hands out the same map.
pub(crate) fn pane_lock(pane_id: &str) -> Arc<PaneLock> {
    TMUX_PANE_LOCKS.for_pane(pane_id)
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

    /// Sends one key to the pane by its tmux name, such as `i` or `BSpace`.
    fn press_key(
        &self,
        pane_id: &str,
        key: &str,
    ) -> impl Future<Output = Result<(), String>> + Send;

    /// Whether Claude Code's `editorMode` is `vim`. Read at each call, so a change shows at
    /// once. A setting that cannot be read counts as not `vim`.
    fn editor_is_vim(&self) -> bool {
        false
    }

    /// The pane locks every writer through these ports shares.
    fn pane_locks(&self) -> &PaneLocks;

    /// `[notify] chief` and `chief_session`: which folder holds the chief and, among its
    /// agents, which tmux session. Unset by default.
    fn chief(&self) -> Chief<'_> {
        Chief::default()
    }
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
        insert_pressed: bool,
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
    /// On a new store, the prompt's time today had passed, so it counts as handled today.
    MarkedOnNewStore {
        name: String,
    },
}

pub(crate) fn log_report(report: &Report) {
    match report {
        Report::Typed {
            name,
            pane,
            late_s,
            insert_pressed,
        } => {
            info!(name = %name, pane = %pane, late_s, insert_pressed, "timed prompt typed");
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
        Report::MarkedOnNewStore { name } => {
            info!(name = %name, "timed prompt marked handled (new store)")
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

/// How one try at typing a text ended.
pub(crate) enum Attempt {
    Wait(Reason),
    Typed {
        pane: String,
        /// Harold pressed `i` to put a Vim box in INSERT mode before it typed.
        insert_pressed: bool,
    },
    EnterFailed {
        pane: String,
        error: String,
    },
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

    /// On a store Harold has just created, counts each prompt whose time today has passed as
    /// handled today, so a restart onto a new store does not type it a second time. A prompt
    /// whose time is later today still runs. Without a readable clock nothing is marked.
    pub(crate) async fn mark_passed_on_new_store(&mut self) -> Vec<Report> {
        let mut reports = Vec::new();
        let Some(now) = self.ports.now().await else {
            return reports;
        };
        let store: &HaroldStore = &self.store;
        for (prompt, progress) in self.prompts.iter_mut() {
            if matches!(prompt.window.phase(now.seconds_of_day), Phase::Before)
                || progress.handled_day.as_deref() == Some(now.day.as_str())
            {
                continue;
            }
            reports.push(Report::MarkedOnNewStore {
                name: prompt.name.clone(),
            });
            let event = TimedPromptHandled {
                name: prompt.name.clone(),
                day: now.day.clone(),
                outcome: TimedPromptOutcome::Skipped,
                reason: Some(Reason::NewStore.as_str().to_string()),
                pane_id: None,
            };
            record(store, event, &mut reports).await;
            progress.handled_day = Some(now.day.clone());
        }
        reports
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
                    Attempt::Typed {
                        pane,
                        insert_pressed,
                    } => {
                        reports.push(Report::Typed {
                            insert_pressed,
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
    let attempt = type_when_ready(
        ports,
        Writer::TimedPrompt,
        &prompt.folder,
        &prompt.text,
        view,
        typed_into,
    )
    .await;
    if let Attempt::Typed { pane, .. } = &attempt {
        // The next turn end on the pane starts at this typing, unless the hook sends its
        // transcript time.
        crate::telemetry::prompt_typed(
            pane,
            crate::telemetry::StartedBy::TimedPrompt,
            None,
            time::OffsetDateTime::now_utc(),
        );
    }
    attempt
}

/// Checks 1 to 7 for `folder` and types `text` when they pass. Shared by timed prompts and
/// alerts. A pane in `typed_into` counts as busy (see `pick_pane`), and so does a pane
/// another writer typed into in the last minute while it still reads idle.
///
/// The pane's lock is held from the checks to Enter. A writer that waited for it checks
/// again on a fresh view, so it sees the pane the other writer just typed into as busy.
pub(crate) async fn type_when_ready<P: Ports>(
    ports: &P,
    writer: Writer,
    folder: &str,
    text: &str,
    view: &AgentsView,
    typed_into: &[String],
) -> Attempt {
    let locks = ports.pane_locks();
    locks.observe(view);
    // For the chief's folder `pick_pane` finds the chief with `resolve_chief`, which logs
    // each change of the chief's pane.
    let chief = ports.chief();
    let busy_ok = writer == Writer::HandOver;
    let pane = match pick_pane(view, folder, chief, typed_into, busy_ok) {
        Ok(pane) => pane.pane_id.clone(),
        Err(reason) => return Attempt::Wait(reason),
    };
    let lock = locks.for_pane(&pane);
    let mut record = lock.lock().await;
    let view = ports.agents();
    if !view.listing_failed && !reads_idle(&view, &pane) {
        record.typed = None;
    }
    match pick_pane(&view, folder, chief, typed_into, busy_ok) {
        Ok(picked) if picked.pane_id == pane => {}
        // The folder's pane changed while this writer waited; the next try looks again.
        Ok(_) => return Attempt::Wait(Reason::AgentStateUnknown),
        Err(reason) => return Attempt::Wait(reason),
    }
    if record.held_against(writer, Instant::now()) {
        return Attempt::Wait(Reason::AgentBusy);
    }
    let vim = ports.editor_is_vim();
    let mut screen = ports.read_screen(&pane).await;
    let mut insert_pressed = false;
    // A lone `i` soon after Harold pressed `i` is Harold's own: take it out and check the
    // box again. After a minute it is the user's text.
    if let Some(at) = record.insert_key_at
        && at.elapsed() >= STRAY_INSERT_KEY_AGE
    {
        record.insert_key_at = None;
    }
    if record.insert_key_at.is_some() && screen.as_deref().is_some_and(|s| box_holds_only(s, 'i')) {
        if remove_stray_insert_key(ports, &pane, screen.as_deref(), false)
            .await
            .is_err()
        {
            return Attempt::Wait(Reason::TypingFailed);
        }
        record.insert_key_at = None;
        tokio::time::sleep(INSERT_REDRAW).await;
        screen = ports.read_screen(&pane).await;
    }
    if let Some(InputBox::NotInsert) = screen.as_deref().map(|screen| read_input_box(screen, vim)) {
        // The box is empty and in Vim NORMAL mode. One `i` puts it in INSERT mode, and
        // the footer must show that before Harold types. At most one `i` per try: a
        // second one would type the letter into a box already in INSERT mode.
        if ports.press_key(&pane, "i").await.is_err() {
            return Attempt::Wait(Reason::TypingFailed);
        }
        record.insert_key_at = Some(Instant::now());
        let mut removed = false;
        let mut ready = false;
        for _ in 0..INSERT_READS {
            tokio::time::sleep(INSERT_REDRAW).await;
            let screen = ports.read_screen(&pane).await;
            match screen.as_deref().map(|screen| read_input_box(screen, vim)) {
                Some(InputBox::Empty) => {
                    ready = true;
                    break;
                }
                Some(InputBox::HoldsText)
                    if !removed && screen.as_deref().is_some_and(|s| box_holds_only(s, 'i')) =>
                {
                    // The box was in INSERT mode already, with no footer drawn, and the
                    // `i` went in as text. Take it out and read again.
                    if remove_stray_insert_key(ports, &pane, screen.as_deref(), true)
                        .await
                        .is_err()
                    {
                        return Attempt::Wait(Reason::TypingFailed);
                    }
                    record.insert_key_at = None;
                    removed = true;
                }
                _ if removed => break,
                _ => {}
            }
        }
        if !ready {
            return Attempt::Wait(Reason::InputBoxNotInsert);
        }
        insert_pressed = true;
    } else if let Some(reason) = box_reason(screen.as_deref(), vim) {
        return Attempt::Wait(reason);
    }
    // Nothing is in the box yet, so a failure here is safe to try again later.
    if ports.type_text(&pane, text).await.is_err() {
        return Attempt::Wait(Reason::TypingFailed);
    }
    // Typed, whether or not Enter goes through: the text is in the box either way.
    record.insert_key_at = None;
    record.typed_by(writer);
    match ports.press_enter(&pane).await {
        Ok(()) => Attempt::Typed {
            pane,
            insert_pressed,
        },
        Err(error) => Attempt::EnterFailed { pane, error },
    }
}

/// Takes Harold's own `i` out of the box: `BSpace` in INSERT mode, else `x`, which removes
/// the letter under the cursor in NORMAL mode. The box is in INSERT mode when the footer
/// shows it, or when `known_insert` is set: Harold's `i` showed as text in the try that
/// pressed it, so the box was in INSERT mode already.
async fn remove_stray_insert_key<P: Ports>(
    ports: &P,
    pane: &str,
    screen: Option<&str>,
    known_insert: bool,
) -> Result<(), String> {
    let insert = known_insert
        || screen
            .and_then(read_box)
            .is_some_and(|read| read.insert_footer);
    ports
        .press_key(pane, if insert { "BSpace" } else { "x" })
        .await?;
    info!(
        pane_id = %pane,
        state = if insert { "insert" } else { "normal" },
        "stray insert key removed"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Typing a line for a tool
// ---------------------------------------------------------------------------

/// How a `type_line_once` call ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LineOutcome {
    Typed {
        insert_pressed: bool,
    },
    /// The pane was busy, its input box held text, or the pane list could not be read;
    /// nothing was typed. Carries the name of the `Reason` that made the line wait.
    PaneBusy(&'static str),
    /// No agent pane has the id.
    NoPane,
    /// The text went into the box but Enter failed.
    Failed,
}

/// Tries once to type `text` into the agent pane `pane_id` as `writer` and press Enter.
/// It never waits for a busy pane: the caller tries again later. Logs nothing: the caller
/// logs what it needs to.
pub(crate) async fn type_line_once<P: Ports>(
    ports: &P,
    writer: Writer,
    pane_id: &str,
    text: &str,
) -> LineOutcome {
    let view = ports.agents();
    // A failed listing says nothing about the pane: try again later.
    if view.listing_failed {
        return LineOutcome::PaneBusy(Reason::PaneListFailing.as_str());
    }
    let Some(pane) = view.panes.iter().find(|pane| pane.pane_id == pane_id) else {
        return LineOutcome::NoPane;
    };
    // Only the pane the writers type into for the folder may be typed into: a second pane
    // in the chief's folder waits.
    if let Ok(target) = target_pane(&view, &pane.folder, ports.chief())
        && target.pane_id != pane_id
    {
        // A second agent in the chief's folder: the same reason `target_pane` gives.
        return LineOutcome::PaneBusy("several_agents_in_folder");
    }
    match type_when_ready(ports, writer, &pane.folder, text, &view, &[]).await {
        Attempt::Typed { insert_pressed, .. } => LineOutcome::Typed { insert_pressed },
        Attempt::EnterFailed { .. } => LineOutcome::Failed,
        Attempt::Wait(Reason::NoAgentInFolder) => LineOutcome::NoPane,
        Attempt::Wait(reason) => LineOutcome::PaneBusy(reason.as_str()),
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

/// The visible screen of one pane, with the escape codes that say which text is dim or in
/// reverse video (`-e`).
fn capture_args(pane_id: &str) -> [&str; 5] {
    ["capture-pane", "-e", "-p", "-t", pane_id]
}

/// Sends one key to the pane by its tmux name.
fn press_key_in_tmux_pane(pane_id: &str, key: &str) -> Result<(), String> {
    if pane_id.starts_with(crate::agent::rex::BLOCK_PREFIX) {
        return crate::agent::rex::press_key(pane_id, key);
    }
    let status = std::process::Command::new("tmux")
        .args(["send-keys", "-t", pane_id, key])
        .status()
        .map_err(|error| format!("failed to start tmux key {key} for {pane_id}: {error}"))?;
    if !status.success() {
        return Err(format!(
            "tmux send-keys {key} failed for {pane_id}: {status}"
        ));
    }
    Ok(())
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
    chief_folder: Option<String>,
    chief_session: Option<String>,
}

impl TmuxPorts {
    /// `providers` are the configured agent providers, which say whose input box Harold
    /// can read.
    pub(crate) fn new(snapshots: AgentSnapshotHub, providers: Vec<AgentProviderSettings>) -> Self {
        Self {
            snapshots,
            providers,
            chief_folder: None,
            chief_session: None,
        }
    }

    /// Sets the chief's folder and tmux session (`[notify] chief`, `chief_session`).
    pub(crate) fn with_chief(mut self, notify: &crate::settings::NotifySettings) -> Self {
        self.chief_folder = notify.chief.clone();
        self.chief_session = notify.chief_session.clone();
        self
    }
}

impl Ports for TmuxPorts {
    async fn now(&self) -> Option<LocalNow> {
        let text = bounded_stdout(DATE_PROGRAM, &DATE_ARGS, COMMAND_TIMEOUT).await?;
        parse_local_now(&text)
    }

    fn agents(&self) -> AgentsView {
        agents_view(&self.snapshots.current(), &self.providers)
    }

    fn chief(&self) -> Chief<'_> {
        Chief {
            folder: self.chief_folder.as_deref(),
            session: self.chief_session.as_deref(),
        }
    }

    async fn read_screen(&self, pane_id: &str) -> Option<String> {
        if pane_id.starts_with(crate::agent::rex::BLOCK_PREFIX) {
            let pane_id = pane_id.to_string();
            let read = tokio::task::spawn_blocking(move || {
                crate::agent::rex::read_screen_styled(&pane_id)
            });
            return tokio::time::timeout(COMMAND_TIMEOUT, read)
                .await
                .ok()?
                .ok()?;
        }
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

    async fn press_key(&self, pane_id: &str, key: &str) -> Result<(), String> {
        let (pane_id, key) = (pane_id.to_string(), key.to_string());
        typing_step(move || press_key_in_tmux_pane(&pane_id, &key)).await
    }

    fn editor_is_vim(&self) -> bool {
        claude_config_dir().is_some_and(|dir| editor_mode_is_vim(&dir))
    }

    fn pane_locks(&self) -> &PaneLocks {
        &TMUX_PANE_LOCKS
    }
}

// ---------------------------------------------------------------------------
// The task
// ---------------------------------------------------------------------------

/// Runs the first round `first_round_after` after it starts and then a round every
/// `ROUND_INTERVAL`, until shutdown. With no prompts it ends at once and logs nothing.
/// Shutdown ends the wait for the first round at once. After that it is looked at between
/// rounds only: a round is never dropped between typing a text and pressing Enter.
pub(crate) async fn run<P: Ports>(
    mut prompts: TimedPrompts<P>,
    first_round_after: Duration,
    mut shutdown: watch::Receiver<()>,
) {
    if prompts.is_empty() {
        return;
    }
    let mut ticker = tokio::time::interval_at(
        tokio::time::Instant::now() + first_round_after,
        ROUND_INTERVAL,
    );
    // A round that runs longer than `ROUND_INTERVAL` is not followed by a burst of rounds
    // to make up for the ones it ran over.
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

/// How long after Harold starts the first round waits. The snapshot Harold starts with is
/// from before it stopped, so the first round waits until the agent monitor has had time
/// for two listings of the tmux panes (a pane that is gone is removed after two) and one
/// pass over the screens.
pub(crate) fn startup_wait(monitor: &AgentMonitorSettings) -> Duration {
    let listing = Duration::from_millis(monitor.inventory_interval_ms)
        + Duration::from_millis(monitor.inventory_timeout_ms);
    listing * 2 + Duration::from_millis(monitor.screen_interval_ms) + COMMAND_TIMEOUT
}

/// On a new store, marks the prompts whose time today has passed. A store that already
/// exists is opened as it is: nothing is marked and the clock is not read.
pub(crate) async fn mark_passed_if_new_store<P: Ports>(
    prompts: &mut TimedPrompts<P>,
    is_new: bool,
) -> Vec<Report> {
    if !is_new {
        return Vec::new();
    }
    prompts.mark_passed_on_new_store().await
}

/// Starts the task for the prompts in `settings`. The caller has already read the whole
/// event stream into the state database, so the stored dates are current.
/// On a new store, prompts whose time today has passed are first marked handled.
pub(crate) async fn start(
    settings: &Settings,
    store: Arc<HaroldStore>,
    snapshots: AgentSnapshotHub,
    shutdown: watch::Receiver<()>,
) -> store::Result<JoinHandle<()>> {
    let last_days = if settings.timed_prompts.is_empty() {
        HashMap::new()
    } else {
        store.timed_prompt_last_days().await?
    };
    let ports = TmuxPorts::new(snapshots, settings.agents.0.clone()).with_chief(&settings.notify);
    let is_new = store.is_new();
    let mut prompts = TimedPrompts::new(&settings.timed_prompts, &last_days, ports, store);
    for report in mark_passed_if_new_store(&mut prompts, is_new).await {
        log_report(&report);
    }
    Ok(tokio::spawn(run(
        prompts,
        startup_wait(&settings.agent_monitor),
        shutdown,
    )))
}

#[cfg(test)]
#[path = "timed_prompts_tests.rs"]
pub(crate) mod tests;
