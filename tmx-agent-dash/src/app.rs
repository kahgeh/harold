use crate::questions::{BlockEffect, Mode, Outgoing, QuestionBlock, Queue, SendResult};
use crate::text::normalize_search;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::HashSet;
use std::fmt;

/// Release tag compiled in by the release workflow; "source" for any other build.
pub const DASH_VERSION: &str = version_or_source(option_env!("HAROLD_RELEASE_VERSION"));

const fn version_or_source(value: Option<&'static str>) -> &'static str {
    match value {
        Some(version) if !version.is_empty() => version,
        _ => "source",
    }
}

/// Whether `key` acts the same in every mode, before any other routing: Ctrl+Q and
/// Ctrl+C quit and Ctrl+L retries. Shift held as well still counts.
pub fn is_global_key(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('q' | 'c' | 'l'))
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AgentIncarnation {
    pub pane_id: String,
    pub pane_pid: u32,
    pub agent_pid: u32,
    pub agent_started_at_ms: i64,
    pub provider_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    Busy,
    Idle,
    Unknown,
}

impl AgentState {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Busy => "BUSY",
            Self::Idle => "IDLE",
            Self::Unknown => "UNKNOWN",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRow {
    pub incarnation: AgentIncarnation,
    pub provider_display_name: String,
    pub tmux_target: String,
    pub session_name: String,
    pub window_index: u32,
    pub pane_index: u32,
    pub working_directory: String,
    pub work_summary: Option<String>,
    pub state: AgentState,
    pub last_transition_at_ms: i64,
}

impl AgentRow {
    /// Project the agent belongs to: the first word of its tmux session name.
    pub fn group(&self) -> &str {
        self.session_name
            .split_whitespace()
            .next()
            .unwrap_or(&self.session_name)
    }

    /// What the user calls this agent: the last component of its working directory.
    pub fn name(&self) -> &str {
        if self.working_directory.is_empty() {
            return &self.tmux_target;
        }
        match self
            .working_directory
            .trim_end_matches('/')
            .rsplit('/')
            .next()
        {
            Some(last) if !last.is_empty() => last,
            _ => "/",
        }
    }

    pub fn provider_tag(&self) -> String {
        let id = self.incarnation.provider_id.to_lowercase();
        let display = self.provider_display_name.to_lowercase();
        if id.contains("claude") || display.contains("claude") {
            return "cc".into();
        }
        if id.contains("codex") || display.contains("codex") {
            return "cx".into();
        }
        display
            .chars()
            .filter(|character| character.is_alphanumeric())
            .take(2)
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorHealthState {
    Healthy,
    Degraded,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorHealth {
    pub component: String,
    pub state: MonitorHealthState,
    pub reason_code: String,
    pub observed_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub through_event_version: u64,
    pub server_time_ms: i64,
    pub monitor_health: Vec<MonitorHealth>,
    pub rows: Vec<AgentRow>,
    pub tts_muted: bool,
    pub messaging_paused: bool,
    /// The release Harold reports, `None` while disconnected or when it sent none.
    pub harold_version: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceState {
    Unknown,
    On,
    Muted,
}

impl VoiceState {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Unknown => "○ VOICE —",
            Self::On => "● VOICE ON",
            Self::Muted => "○ VOICE MUTED",
        }
    }
}

/// The call with the chief, as Harold last reported it. Harold's transcribing
/// and waiting both show as `Thinking`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallState {
    NotSetUp,
    Off,
    Listening,
    Thinking,
    Speaking,
}

impl CallState {
    /// Whether a call is open: listening, thinking or speaking.
    pub const fn in_call(self) -> bool {
        matches!(self, Self::Listening | Self::Thinking | Self::Speaking)
    }
}

/// What the dashboard asks of Harold's call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallAction {
    Start,
    Stop,
    TalkNow,
    Status,
}

/// Harold's last answer about the call. `chief_pane_id` is empty when no chief
/// runs, and `last_transcript` is empty until a turn is typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallView {
    pub state: CallState,
    pub chief_pane_id: String,
    pub last_transcript: String,
    pub last_result: Option<&'static str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Connecting,
    Live,
    Unavailable,
    Stale,
}

impl ConnectionState {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Connecting => "CONNECTING",
            Self::Live => "LIVE",
            Self::Unavailable => "UNAVAILABLE",
            Self::Stale => "STALE",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchState {
    pub query: String,
    pub editing: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RuntimeStatus {
    Retrying {
        endpoint: String,
        detail: String,
        delay_ms: u64,
    },
    NavigationUnavailable,
    NavigationFailed(String),
    VoiceFailed(String),
    MessagingFailed(String),
    CallFailed(String),
    SourceError(String),
}

/// Where the keys go: the agent list, or the question block above it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Focus {
    #[default]
    List,
    Block,
}

/// What the tmux focus probe saw: the user's active pane, or nothing usable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FocusReading {
    Active(String),
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct App {
    pub connection: ConnectionState,
    /// This dash's own release, fixed at compile time.
    pub dash_version: String,
    pub snapshot: Snapshot,
    pub search: SearchState,
    pub palette: Option<PaletteState>,
    pub selected: Option<AgentIncarnation>,
    /// The call, unknown until Harold answers and again after a failed poll or a disconnect.
    pub call: Option<CallView>,
    /// The chief's questions, hidden while none waits.
    pub questions: QuestionBlock,
    pub focus: Focus,
    last_snapshot_received_at_ms: Option<i64>,
    runtime_status: Option<RuntimeStatus>,
    has_snapshot: bool,
    normalized_query: String,
    searchable_rows: Vec<SearchableRow>,
    row_labels: Vec<RowLabel>,
    waiting: Vec<AgentIncarnation>,
    active_pane: Option<String>,
    focus_tracking: bool,
    /// The block hid while it had the focus: keys are swallowed until Tab or Esc, so a
    /// note being typed never turns into list commands.
    swallowing_keys: bool,
    /// The question under the focus changed while a note or the summary was in view, or
    /// came back changed: block keys are dropped until Esc (stay in the block) or Tab
    /// (go to the list), so letters typed for the old question never act on the new one.
    holding_block_keys: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SearchableRow {
    fields: [String; 4],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    None,
    Navigate { pane_id: String },
    Retry,
    Quit,
    SetTtsMuted(bool),
    SetMessagingPaused(bool),
    Call(CallAction),
    Answer(Outgoing),
    Defer(String),
    OpenLink(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteState {
    pub query: String,
    pub selected: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteEntry {
    pub label: &'static str,
    pub effect: Effect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotError {
    RegressedRevision { current: u64, received: u64 },
    DuplicatePaneId,
    DuplicateIncarnation,
}

impl fmt::Display for SnapshotError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RegressedRevision { current, received } => write!(
                formatter,
                "snapshot revision regressed from {current} to {received}"
            ),
            Self::DuplicatePaneId => formatter.write_str("snapshot contains a duplicate pane ID"),
            Self::DuplicateIncarnation => {
                formatter.write_str("snapshot contains a duplicate agent incarnation")
            }
        }
    }
}

impl std::error::Error for SnapshotError {}

impl App {
    pub fn new(
        connection: ConnectionState,
        snapshot: Snapshot,
        search: SearchState,
        selected: Option<AgentIncarnation>,
    ) -> Self {
        let normalized_query = normalize_search(&search.query);
        let searchable_rows = snapshot.rows.iter().map(SearchableRow::from).collect();
        let row_labels = build_labels(&snapshot.rows);
        Self {
            connection,
            dash_version: DASH_VERSION.to_owned(),
            snapshot,
            search,
            palette: None,
            selected,
            call: None,
            questions: QuestionBlock::default(),
            focus: Focus::List,
            last_snapshot_received_at_ms: None,
            runtime_status: None,
            has_snapshot: matches!(connection, ConnectionState::Live | ConnectionState::Stale),
            normalized_query,
            searchable_rows,
            row_labels,
            waiting: Vec::new(),
            active_pane: None,
            focus_tracking: false,
            swallowing_keys: false,
            holding_block_keys: false,
        }
    }

    pub fn label<'a>(&'a self, row: &'a AgentRow) -> &'a str {
        self.row_label(row)
            .map_or_else(|| row.name(), |label| label.full.as_str())
    }

    /// The row label split into its name and the disambiguating suffix (empty
    /// unless the name collides). Renderers truncate the name, never the suffix.
    pub fn label_parts<'a>(&'a self, row: &'a AgentRow) -> (&'a str, &'a str) {
        self.row_label(row).map_or_else(
            || (row.name(), ""),
            |label| label.full.split_at(label.full.len() - label.suffix_len),
        )
    }

    fn row_label(&self, row: &AgentRow) -> Option<&RowLabel> {
        self.snapshot
            .rows
            .iter()
            .position(|candidate| candidate.incarnation == row.incarnation)
            .and_then(|index| self.row_labels.get(index))
    }

    pub fn visible_groups(&self) -> Vec<(&str, Vec<&AgentRow>)> {
        let mut groups: Vec<(&str, Vec<&AgentRow>)> = Vec::new();
        for row in self.visible_rows() {
            match groups.last_mut() {
                Some((group, rows)) if *group == row.group() => rows.push(row),
                _ => groups.push((row.group(), vec![row])),
            }
        }
        groups
    }

    pub fn observe_focus(&mut self, reading: FocusReading) {
        match reading {
            FocusReading::Active(pane_id) => {
                self.waiting.retain(|waiting| waiting.pane_id != pane_id);
                self.active_pane = Some(pane_id);
                self.focus_tracking = true;
            }
            FocusReading::Unavailable => {
                self.active_pane = None;
                self.focus_tracking = false;
            }
        }
    }

    pub const fn focus_tracking(&self) -> bool {
        self.focus_tracking
    }

    pub fn is_waiting(&self, incarnation: &AgentIncarnation) -> bool {
        self.waiting.contains(incarnation)
    }

    /// Agents that finished while the user was elsewhere, oldest first.
    pub fn waiting_rows(&self) -> Vec<&AgentRow> {
        self.waiting
            .iter()
            .filter_map(|waiting| {
                self.snapshot
                    .rows
                    .iter()
                    .find(|row| &row.incarnation == waiting)
            })
            .collect()
    }

    fn next_waiting(&self, rows: &[AgentRow]) -> Vec<AgentIncarnation> {
        let mut waiting: Vec<AgentIncarnation> = self
            .waiting
            .iter()
            .filter(|waiting| {
                rows.iter()
                    .any(|row| &row.incarnation == *waiting && row.state != AgentState::Busy)
            })
            .cloned()
            .collect();
        for row in rows {
            if row.state != AgentState::Idle || waiting.contains(&row.incarnation) {
                continue;
            }
            let was_busy = self.snapshot.rows.iter().any(|prior| {
                prior.incarnation == row.incarnation && prior.state == AgentState::Busy
            });
            let in_view = self.active_pane.as_deref() == Some(row.incarnation.pane_id.as_str());
            if was_busy && !in_view {
                waiting.push(row.incarnation.clone());
            }
        }
        waiting
    }

    fn select_waiting(&mut self, forward: bool) {
        let waiting: Vec<AgentIncarnation> = self
            .waiting_rows()
            .into_iter()
            .map(|row| row.incarnation.clone())
            .collect();
        if waiting.is_empty() {
            return;
        }
        let current = self
            .selected
            .as_ref()
            .and_then(|selected| waiting.iter().position(|waiting| waiting == selected));
        let index = match (current, forward) {
            (Some(index), true) => (index + 1) % waiting.len(),
            (Some(index), false) => (index + waiting.len() - 1) % waiting.len(),
            (None, true) => 0,
            (None, false) => waiting.len() - 1,
        };
        let target = waiting[index].clone();
        if !self
            .visible_rows()
            .iter()
            .any(|row| row.incarnation == target)
        {
            self.search.query.clear();
            self.normalized_query.clear();
        }
        self.selected = Some(target);
    }

    pub fn voice(&self) -> VoiceState {
        if !self.has_snapshot {
            VoiceState::Unknown
        } else if self.snapshot.tts_muted {
            VoiceState::Muted
        } else {
            VoiceState::On
        }
    }

    pub fn observe_call(&mut self, view: Option<CallView>) {
        self.call = view;
    }

    /// The row whose pane Harold named as the chief's in its last call answer.
    pub fn chief_row(&self) -> Option<&AgentRow> {
        let pane_id = self
            .call
            .as_ref()
            .map(|call| call.chief_pane_id.as_str())
            .filter(|pane_id| !pane_id.is_empty())?;
        self.snapshot
            .rows
            .iter()
            .find(|row| row.incarnation.pane_id == pane_id)
    }

    fn chief_row_selected(&self) -> bool {
        match (self.selected_row(), self.chief_row()) {
            (Some(selected), Some(chief)) => selected.incarnation == chief.incarnation,
            _ => false,
        }
    }

    /// What the call button does now: start a call when none is on and the voice
    /// is not muted, end one during a call, and nothing otherwise.
    fn call_press(&self) -> Option<CallAction> {
        match self.call.as_ref()?.state {
            CallState::Off if self.voice() != VoiceState::Muted => Some(CallAction::Start),
            state if state.in_call() => Some(CallAction::Stop),
            _ => None,
        }
    }

    /// Takes the latest question queue. A new question never takes the focus; the
    /// focus returns to the list when the block hides.
    ///
    /// When the block has the focus and the question in view changes while a note or the
    /// summary was showing, block keys are held until Esc or Tab. A block that hid under
    /// the focus and comes back takes the focus back: as it was, typing resumes; changed,
    /// its keys are held.
    pub fn observe_questions(&mut self, queue: Queue) {
        let before = self.questions.shown();
        let kept = self.questions.kept();
        self.questions.reconcile(queue);
        let after = self.questions.shown();
        if self.swallowing_keys && self.questions.is_visible() {
            self.swallowing_keys = false;
            self.focus = Focus::Block;
            self.holding_block_keys = after != kept;
        } else if self.focus == Focus::Block
            && after != before
            && before
                .as_ref()
                .is_some_and(|(_, _, mode)| !matches!(mode, Mode::Choosing))
        {
            self.holding_block_keys = true;
        }
        self.settle_focus();
    }

    /// Whether block keys are held after the question in view changed under them.
    pub fn holding_block_keys(&self) -> bool {
        self.holding_block_keys
    }

    /// Hides the block without a notice, for when the queue cannot be read or the stream
    /// is lost. The block keeps its picks, mode and note for the next listing. If it had
    /// the focus, keys are swallowed until Tab or Esc.
    pub fn hide_questions(&mut self) {
        self.questions.suspend();
        if self.focus == Focus::Block {
            self.swallowing_keys = true;
        }
        self.holding_block_keys = false;
        self.settle_focus();
    }

    /// Whether keys are being swallowed after the block hid under the focus.
    pub fn swallowing_keys(&self) -> bool {
        self.swallowing_keys
    }

    /// Applies Harold's answer to a send.
    pub fn question_sent(&mut self, id: &str, result: SendResult) {
        self.questions.sent(id, result);
        self.settle_focus();
    }

    /// Whether `key` should meet a fresh queue: it goes to the block, or it is `Tab`.
    /// A global key never does: it acts the same whatever the queue holds.
    pub fn wants_fresh_questions(&self, key: &KeyEvent) -> bool {
        !is_global_key(key)
            && !self.swallowing_keys
            && !self.search.editing
            && self.palette.is_none()
            && (self.focus == Focus::Block || key.code == KeyCode::Tab)
    }

    /// Whether `key` goes to the question block.
    pub fn key_reaches_block(&self, key: &KeyEvent) -> bool {
        self.wants_fresh_questions(key) && self.focus == Focus::Block && key.code != KeyCode::Tab
    }

    fn settle_focus(&mut self) {
        if !self.questions.is_visible() {
            self.focus = Focus::List;
            self.holding_block_keys = false;
        }
    }

    fn handle_block_key(&mut self, code: KeyCode) -> Effect {
        let effect = match self.questions.handle_key(code) {
            BlockEffect::None => Effect::None,
            BlockEffect::Leave => {
                self.focus = Focus::List;
                Effect::None
            }
            BlockEffect::OpenLink(link) => Effect::OpenLink(link),
            BlockEffect::Defer(id) => Effect::Defer(id),
            BlockEffect::Send(out) => Effect::Answer(out),
        };
        self.settle_focus();
        effect
    }

    pub fn begin_connection(&mut self) {
        self.connection = ConnectionState::Connecting;
    }

    pub fn apply_first_snapshot(&mut self, snapshot: Snapshot) -> Result<(), SnapshotError> {
        self.replace_snapshot(snapshot, true)?;
        self.has_snapshot = true;
        self.connection = ConnectionState::Live;
        Ok(())
    }

    pub fn apply_later_snapshot(&mut self, snapshot: Snapshot) -> Result<bool, SnapshotError> {
        if snapshot.through_event_version == self.snapshot.through_event_version {
            return Ok(false);
        }
        if snapshot.through_event_version < self.snapshot.through_event_version {
            return Err(SnapshotError::RegressedRevision {
                current: self.snapshot.through_event_version,
                received: snapshot.through_event_version,
            });
        }

        self.replace_snapshot(snapshot, false)?;
        self.has_snapshot = true;
        self.connection = ConnectionState::Live;
        Ok(true)
    }

    pub fn mark_disconnected(&mut self) {
        self.call = None;
        self.snapshot.harold_version = None;
        self.connection = if self.has_snapshot {
            ConnectionState::Stale
        } else {
            ConnectionState::Unavailable
        };
    }

    pub(crate) fn record_snapshot_received_at(&mut self, received_at_ms: i64) {
        self.last_snapshot_received_at_ms = Some(received_at_ms);
    }

    pub(crate) const fn last_snapshot_received_at_ms(&self) -> Option<i64> {
        self.last_snapshot_received_at_ms
    }

    pub(crate) fn set_runtime_status(&mut self, status: RuntimeStatus) {
        self.runtime_status = Some(status);
    }

    pub(crate) fn clear_runtime_status(&mut self) {
        self.runtime_status = None;
    }

    pub(crate) const fn runtime_status(&self) -> Option<&RuntimeStatus> {
        self.runtime_status.as_ref()
    }

    pub fn visible_rows(&self) -> Vec<&AgentRow> {
        self.snapshot
            .rows
            .iter()
            .zip(&self.searchable_rows)
            .filter_map(|(row, searchable)| {
                (self.normalized_query.is_empty()
                    || searchable.matches(self.normalized_query.as_str()))
                .then_some(row)
            })
            .collect()
    }

    pub fn visible_counts(&self) -> (usize, usize) {
        (self.visible_rows().len(), self.snapshot.rows.len())
    }

    pub fn selected_row(&self) -> Option<&AgentRow> {
        let selected = self.selected.as_ref()?;
        self.visible_rows()
            .into_iter()
            .find(|row| &row.incarnation == selected)
    }

    pub fn state_counts(&self) -> (usize, usize, usize) {
        self.snapshot
            .rows
            .iter()
            .fold((0, 0, 0), |(busy, idle, unknown), row| match row.state {
                AgentState::Busy => (busy + 1, idle, unknown),
                AgentState::Idle => (busy, idle + 1, unknown),
                AgentState::Unknown => (busy, idle, unknown + 1),
            })
    }

    /// Whether Harold has confirmed that away-channel messaging is paused.
    pub fn messaging_paused(&self) -> bool {
        self.has_snapshot && self.snapshot.messaging_paused
    }

    pub fn degraded_health(&self) -> impl Iterator<Item = &MonitorHealth> {
        self.snapshot
            .monitor_health
            .iter()
            .filter(|health| health.state == MonitorHealthState::Degraded)
    }

    pub fn palette_entries(&self) -> Vec<PaletteEntry> {
        let Some(palette) = &self.palette else {
            return Vec::new();
        };
        if self.connection != ConnectionState::Live {
            return Vec::new();
        }
        let query = normalize_search(&palette.query);
        let mut all = match self.voice() {
            VoiceState::On => vec![PaletteEntry {
                label: "Voice: mute",
                effect: Effect::SetTtsMuted(true),
            }],
            VoiceState::Muted => vec![PaletteEntry {
                label: "Voice: unmute",
                effect: Effect::SetTtsMuted(false),
            }],
            VoiceState::Unknown => Vec::new(),
        };
        if self.has_snapshot {
            all.push(if self.snapshot.messaging_paused {
                PaletteEntry {
                    label: "Messaging: resume",
                    effect: Effect::SetMessagingPaused(false),
                }
            } else {
                PaletteEntry {
                    label: "Messaging: pause",
                    effect: Effect::SetMessagingPaused(true),
                }
            });
        }
        match self.call_press() {
            Some(CallAction::Start) => all.push(PaletteEntry {
                label: "Call: start",
                effect: Effect::Call(CallAction::Start),
            }),
            Some(CallAction::Stop) => all.push(PaletteEntry {
                label: "Call: end",
                effect: Effect::Call(CallAction::Stop),
            }),
            Some(CallAction::TalkNow | CallAction::Status) | None => {}
        }
        all.into_iter()
            .filter(|entry| normalize_search(entry.label).contains(&query))
            .collect()
    }

    fn handle_palette_key(&mut self, key: KeyCode) -> Effect {
        let entries = self.palette_entries();
        let Some(palette) = self.palette.as_mut() else {
            return Effect::None;
        };
        match key {
            KeyCode::Esc => self.palette = None,
            KeyCode::Enter => {
                if let Some(entry) = entries.get(palette.selected) {
                    let effect = entry.effect.clone();
                    self.palette = None;
                    return effect;
                }
            }
            KeyCode::Up => palette.selected = palette.selected.saturating_sub(1),
            KeyCode::Down => {
                palette.selected = (palette.selected + 1).min(entries.len().saturating_sub(1));
            }
            KeyCode::Backspace => {
                palette.query.pop();
                palette.selected = 0;
            }
            KeyCode::Char(character) if !character.is_control() => {
                palette.query.push(character);
                palette.selected = 0;
            }
            _ => {}
        }
        Effect::None
    }

    /// Selects the first visible row, or nothing when no row shows.
    pub fn select_first_row(&mut self) {
        self.select_boundary(false);
    }

    /// Handles one key press. Ctrl+Q and Ctrl+C quit and Ctrl+L retries from every
    /// mode, changing nothing else. Any other Control or Alt chord does nothing: it is
    /// never typed and never taken as the bare letter.
    pub fn handle_key(&mut self, key: impl Into<KeyEvent>) -> Effect {
        let key = key.into();
        // The global keys act first, even while the question block holds or swallows keys.
        if is_global_key(&key) {
            return match key.code {
                KeyCode::Char('l') => Effect::Retry,
                _ => Effect::Quit,
            };
        }
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return Effect::None;
        }
        if self.swallowing_keys {
            // Tab or Esc returns the keys to the list, which already has the focus.
            if matches!(key.code, KeyCode::Tab | KeyCode::Esc) {
                self.swallowing_keys = false;
            }
            return Effect::None;
        }
        if self.search.editing {
            return self.handle_search_key(key.code);
        }
        if self.palette.is_some() {
            return self.handle_palette_key(key.code);
        }
        if self.holding_block_keys && self.focus == Focus::Block {
            match key.code {
                // Esc reads the change: the notice goes and the shown question is chosen
                // from, from its first decision.
                KeyCode::Esc => {
                    self.holding_block_keys = false;
                    self.questions.clear_notice();
                    self.settle_focus();
                    return Effect::None;
                }
                KeyCode::Tab => self.holding_block_keys = false,
                _ => return Effect::None,
            }
        }
        // A notice on a block with no question goes at the next key, and the block hides;
        // the key still acts on the list. In the block, the block clears it itself.
        if self.focus == Focus::List && self.questions.shows_notice_only() {
            self.questions.clear_notice();
        }
        if key.code == KeyCode::Tab && self.questions.is_visible() {
            self.focus = match self.focus {
                Focus::List => Focus::Block,
                Focus::Block => Focus::List,
            };
            return Effect::None;
        }
        if self.focus == Focus::Block {
            return self.handle_block_key(key.code);
        }

        match key.code {
            KeyCode::Char('f') => self.search.editing = true,
            KeyCode::Char('/') => {
                self.palette = Some(PaletteState {
                    query: String::new(),
                    selected: 0,
                });
            }
            KeyCode::Char('q') => return Effect::Quit,
            KeyCode::Esc if !self.search.query.is_empty() => {
                self.search.query.clear();
                self.search_changed();
            }
            KeyCode::Char('j') | KeyCode::Down => self.select_relative(1),
            KeyCode::Char('k') | KeyCode::Up => self.select_relative(-1),
            KeyCode::Char('g') => self.select_boundary(false),
            KeyCode::Char('G') => self.select_boundary(true),
            KeyCode::Char('c') if self.chief_row_selected() => {
                if let Some(action) = self.call_press() {
                    return Effect::Call(action);
                }
            }
            KeyCode::Char('t')
                if self.chief_row_selected()
                    && self
                        .call
                        .as_ref()
                        .is_some_and(|call| call.state == CallState::Speaking) =>
            {
                return Effect::Call(CallAction::TalkNow);
            }
            KeyCode::Char('n') => self.select_waiting(true),
            KeyCode::Char('N') => self.select_waiting(false),
            KeyCode::Enter => {
                let Some(incarnation) = self.selected_row().map(|row| row.incarnation.clone())
                else {
                    return Effect::None;
                };
                if !self.focus_tracking {
                    // Without a focus probe, jumping from the dash is the only visit we can see.
                    self.waiting.retain(|waiting| *waiting != incarnation);
                }
                return Effect::Navigate {
                    pane_id: incarnation.pane_id,
                };
            }
            _ => {}
        }
        Effect::None
    }

    fn replace_snapshot(
        &mut self,
        mut snapshot: Snapshot,
        authoritative: bool,
    ) -> Result<(), SnapshotError> {
        validate_rows(&snapshot.rows)?;
        sort_rows(&mut snapshot.rows);
        let waiting = self.next_waiting(&snapshot.rows);

        let prior_selection = self.selected.clone();
        let prior_index = prior_selection.as_ref().and_then(|selected| {
            self.snapshot
                .rows
                .iter()
                .position(|row| &row.incarnation == selected)
        });
        let replacement_in_same_pane = prior_selection.as_ref().is_some_and(|selected| {
            snapshot.rows.iter().any(|row| {
                row.incarnation.pane_id == selected.pane_id && row.incarnation != *selected
            })
        });
        let stable_selection = prior_selection.as_ref().and_then(|selected| {
            snapshot
                .rows
                .iter()
                .find(|row| &row.incarnation == selected)
                .map(|row| row.incarnation.clone())
        });
        let preserved_stable_selection = stable_selection.is_some();

        self.selected = if stable_selection.is_some() {
            stable_selection
        } else if authoritative || replacement_in_same_pane {
            None
        } else {
            prior_index.and_then(|index| {
                snapshot
                    .rows
                    .get(index.min(snapshot.rows.len().saturating_sub(1)))
                    .map(|row| row.incarnation.clone())
            })
        };
        self.snapshot = snapshot;
        self.waiting = waiting;
        self.searchable_rows = self.snapshot.rows.iter().map(SearchableRow::from).collect();
        self.row_labels = build_labels(&self.snapshot.rows);
        if !self.normalized_query.is_empty()
            || preserved_stable_selection
            || (!authoritative && !replacement_in_same_pane && prior_selection.is_some())
        {
            self.select_first_if_hidden();
        }
        Ok(())
    }

    fn handle_search_key(&mut self, key: KeyCode) -> Effect {
        match key {
            KeyCode::Esc => {
                self.search.editing = false;
                self.search.query.clear();
                self.search_changed();
            }
            KeyCode::Enter => self.search.editing = false,
            KeyCode::Backspace => {
                self.search.query.pop();
                self.search_changed();
            }
            KeyCode::Char(character) if !character.is_control() => {
                self.search.query.push(character);
                self.search_changed();
            }
            _ => {}
        }
        Effect::None
    }

    fn search_changed(&mut self) {
        self.normalized_query = normalize_search(&self.search.query);
        self.select_first_if_hidden();
    }

    fn select_first_if_hidden(&mut self) {
        let selected_is_visible = self.selected.as_ref().is_some_and(|selected| {
            self.visible_rows()
                .iter()
                .any(|row| &row.incarnation == selected)
        });
        if selected_is_visible {
            return;
        }
        self.selected = self
            .visible_rows()
            .first()
            .map(|row| row.incarnation.clone());
    }

    fn select_relative(&mut self, delta: isize) {
        let visible = self.visible_rows();
        let Some(current) = self.selected.as_ref() else {
            self.selected = visible.first().map(|row| row.incarnation.clone());
            return;
        };
        let Some(index) = visible.iter().position(|row| &row.incarnation == current) else {
            self.selected = visible.first().map(|row| row.incarnation.clone());
            return;
        };
        let next = index
            .saturating_add_signed(delta)
            .min(visible.len().saturating_sub(1));
        self.selected = visible.get(next).map(|row| row.incarnation.clone());
    }

    fn select_boundary(&mut self, last: bool) {
        let visible = self.visible_rows();
        self.selected = if last {
            visible.last()
        } else {
            visible.first()
        }
        .map(|row| row.incarnation.clone());
    }
}

impl From<&AgentRow> for SearchableRow {
    fn from(row: &AgentRow) -> Self {
        Self {
            fields: [
                normalize_search(&row.provider_display_name),
                normalize_search(row.work_summary.as_deref().unwrap_or_default()),
                normalize_search(&row.tmux_target),
                normalize_search(&row.working_directory),
            ],
        }
    }
}

impl SearchableRow {
    fn matches(&self, query: &str) -> bool {
        self.fields.iter().any(|field| field.contains(query))
    }
}

fn validate_rows(rows: &[AgentRow]) -> Result<(), SnapshotError> {
    let mut pane_ids = HashSet::with_capacity(rows.len());
    let mut incarnations = HashSet::with_capacity(rows.len());
    for row in rows {
        if !incarnations.insert(&row.incarnation) {
            return Err(SnapshotError::DuplicateIncarnation);
        }
        if !pane_ids.insert(row.incarnation.pane_id.as_str()) {
            return Err(SnapshotError::DuplicatePaneId);
        }
    }
    Ok(())
}

fn sort_rows(rows: &mut [AgentRow]) {
    rows.sort_by(|left, right| {
        left.group()
            .cmp(right.group())
            .then_with(|| left.name().cmp(right.name()))
            .then_with(|| left.window_index.cmp(&right.window_index))
            .then_with(|| left.pane_index.cmp(&right.pane_index))
            .then_with(|| left.incarnation.pane_id.cmp(&right.incarnation.pane_id))
            .then_with(|| left.incarnation.pane_pid.cmp(&right.incarnation.pane_pid))
            .then_with(|| left.incarnation.agent_pid.cmp(&right.incarnation.agent_pid))
            .then_with(|| {
                left.incarnation
                    .agent_started_at_ms
                    .cmp(&right.incarnation.agent_started_at_ms)
            })
            .then_with(|| {
                left.incarnation
                    .provider_id
                    .cmp(&right.incarnation.provider_id)
            })
    });
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RowLabel {
    full: String,
    suffix_len: usize,
}

/// Rows sharing a group and name get a suffix so they stay distinguishable:
/// ` :window.pane` when they share a session, otherwise the session's second
/// word (or, lacking one, the whole tmux target) is added so same-address panes
/// of different sessions in one group still differ.
fn build_labels(rows: &[AgentRow]) -> Vec<RowLabel> {
    rows.iter()
        .map(|row| {
            let mut colliders = rows
                .iter()
                .filter(|other| other.group() == row.group() && other.name() == row.name());
            let suffix = if colliders.clone().count() < 2 {
                String::new()
            } else if colliders.any(|other| other.session_name != row.session_name) {
                match row.session_name.split_whitespace().nth(1) {
                    Some(word) => format!(" {word}:{}.{}", row.window_index, row.pane_index),
                    None => format!(" {}", row.tmux_target),
                }
            } else {
                format!(" :{}.{}", row.window_index, row.pane_index)
            };
            RowLabel {
                full: format!("{}{suffix}", row.name()),
                suffix_len: suffix.len(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;
    use crate::questions::{AnswerKind, Mode};
    use AgentState::{Busy, Idle, Unknown};

    #[test]
    fn voice_is_unknown_until_a_snapshot_then_follows_the_snapshot() {
        let empty = Snapshot {
            harold_version: None,
            through_event_version: 0,
            server_time_ms: 0,
            monitor_health: Vec::new(),
            rows: Vec::new(),
            tts_muted: false,
            messaging_paused: false,
        };
        let search = SearchState {
            query: String::new(),
            editing: false,
        };
        let mut app = App::new(ConnectionState::Connecting, empty.clone(), search, None);
        assert_eq!(app.voice(), VoiceState::Unknown);

        app.apply_first_snapshot(Snapshot {
            through_event_version: 1,
            ..empty.clone()
        })
        .unwrap();
        assert_eq!(app.voice(), VoiceState::On);

        app.apply_later_snapshot(Snapshot {
            through_event_version: 2,
            tts_muted: true,
            messaging_paused: false,
            ..empty
        })
        .unwrap();
        assert_eq!(app.voice(), VoiceState::Muted);
    }

    #[test]
    fn snapshot_first_message_is_authoritative_at_any_revision() {
        let selected = incarnation("%old", 10, 20, 30, "codex");
        let mut app = App::new(
            ConnectionState::Stale,
            snapshot(
                90,
                900,
                vec![row(selected.clone(), AgentState::Busy, "z", 9, 9)],
            ),
            empty_search(),
            Some(selected),
        );
        let replacement = incarnation("%new", 11, 21, 31, "claude");

        app.begin_connection();
        assert_eq!(app.connection, ConnectionState::Connecting);
        app.apply_first_snapshot(Snapshot {
            harold_version: None,
            tts_muted: false,
            messaging_paused: false,
            through_event_version: 2,
            server_time_ms: 222,
            monitor_health: vec![health(MonitorHealthState::Degraded, "tmux_unavailable")],
            rows: vec![row(replacement.clone(), AgentState::Idle, "a", 1, 2)],
        })
        .unwrap();

        assert_eq!(app.connection, ConnectionState::Live);
        assert_eq!(app.snapshot.through_event_version, 2);
        assert_eq!(app.snapshot.server_time_ms, 222);
        assert_eq!(
            app.snapshot.monitor_health[0].state,
            MonitorHealthState::Degraded
        );
        assert_eq!(app.snapshot.rows[0].incarnation, replacement);
        assert_eq!(app.selected, None);
    }

    #[test]
    fn snapshot_later_duplicate_is_ignored_and_regression_is_rejected() {
        let mut app = empty_app();
        app.apply_first_snapshot(snapshot(
            7,
            700,
            vec![row(
                incarnation("%7", 7, 70, 700, "codex"),
                AgentState::Busy,
                "s",
                0,
                0,
            )],
        ))
        .unwrap();
        let accepted = app.snapshot.clone();

        assert!(
            !app.apply_later_snapshot(snapshot(7, 701, Vec::new()))
                .unwrap()
        );
        assert_eq!(app.snapshot, accepted);
        assert!(
            app.apply_later_snapshot(snapshot(6, 600, Vec::new()))
                .is_err()
        );
        assert_eq!(app.snapshot, accepted);
    }

    #[test]
    fn snapshot_monitor_health_is_retained_and_recovers_only_on_accepted_snapshot() {
        let pane = incarnation("%1", 1, 2, 3, "codex");
        let mut app = empty_app();
        app.apply_first_snapshot(Snapshot {
            harold_version: None,
            tts_muted: false,
            messaging_paused: false,
            through_event_version: 10,
            server_time_ms: 1_000,
            monitor_health: vec![
                health(MonitorHealthState::Healthy, "ok"),
                health(MonitorHealthState::Degraded, "capture_failed"),
                health(MonitorHealthState::Unknown, "not_observed"),
            ],
            rows: vec![row(pane.clone(), AgentState::Busy, "s", 0, 0)],
        })
        .unwrap();

        assert_eq!(app.connection, ConnectionState::Live);
        assert_eq!(app.snapshot.rows.len(), 1);
        assert_eq!(app.snapshot.monitor_health.len(), 3);
        assert_eq!(app.degraded_health().count(), 1);

        assert!(
            !app.apply_later_snapshot(Snapshot {
                harold_version: None,
                tts_muted: false,
                messaging_paused: false,
                through_event_version: 10,
                server_time_ms: 1_001,
                monitor_health: vec![health(MonitorHealthState::Healthy, "ok")],
                rows: vec![],
            })
            .unwrap()
        );
        assert_eq!(app.degraded_health().count(), 1);

        assert!(
            app.apply_later_snapshot(Snapshot {
                harold_version: None,
                tts_muted: false,
                messaging_paused: false,
                through_event_version: 11,
                server_time_ms: 1_100,
                monitor_health: vec![health(MonitorHealthState::Healthy, "ok")],
                rows: vec![row(pane, AgentState::Idle, "s", 0, 0)],
            })
            .unwrap()
        );
        assert_eq!(app.snapshot.server_time_ms, 1_100);
        assert_eq!(app.degraded_health().count(), 0);
        assert_eq!(app.snapshot.rows.len(), 1);
    }

    #[test]
    fn snapshot_rows_are_sorted_and_duplicate_identity_is_rejected() {
        let busy_b = row(
            incarnation("%4", 4, 40, 400, "codex"),
            AgentState::Busy,
            "b",
            0,
            0,
        );
        let busy_a_2 = row(
            incarnation("%3", 3, 30, 300, "codex"),
            AgentState::Busy,
            "a",
            1,
            2,
        );
        let busy_a_1 = row(
            incarnation("%2", 2, 20, 200, "codex"),
            AgentState::Busy,
            "a",
            1,
            1,
        );
        let idle = row(
            incarnation("%1", 1, 10, 100, "codex"),
            AgentState::Idle,
            "a",
            0,
            0,
        );
        let unknown = row(
            incarnation("%5", 5, 50, 500, "codex"),
            AgentState::Unknown,
            "a",
            0,
            0,
        );
        let mut app = empty_app();

        app.apply_first_snapshot(snapshot(
            1,
            1,
            vec![unknown, idle, busy_b, busy_a_2, busy_a_1],
        ))
        .unwrap();
        assert_eq!(
            app.snapshot
                .rows
                .iter()
                .map(|agent| agent.incarnation.pane_id.as_str())
                .collect::<Vec<_>>(),
            vec!["%1", "%5", "%2", "%3", "%4"]
        );

        let duplicate = row(
            incarnation("%8", 8, 80, 800, "codex"),
            AgentState::Idle,
            "x",
            0,
            0,
        );
        assert!(
            app.apply_later_snapshot(snapshot(2, 2, vec![duplicate.clone(), duplicate]))
                .is_err()
        );

        let same_pane_a = row(
            incarnation("%9", 9, 90, 900, "codex"),
            AgentState::Idle,
            "x",
            0,
            0,
        );
        let same_pane_b = row(
            incarnation("%9", 10, 91, 901, "claude"),
            AgentState::Idle,
            "x",
            0,
            1,
        );
        assert!(
            app.apply_later_snapshot(snapshot(2, 2, vec![same_pane_a, same_pane_b]))
                .is_err()
        );
    }

    #[test]
    fn snapshot_preserves_stable_selection_but_clears_same_pane_replacement() {
        let selected = incarnation("%2", 2, 20, 200, "codex");
        let mut app = App::new(
            ConnectionState::Live,
            snapshot(
                1,
                1,
                vec![row(selected.clone(), AgentState::Idle, "s", 0, 1)],
            ),
            empty_search(),
            Some(selected.clone()),
        );

        app.apply_later_snapshot(snapshot(
            2,
            2,
            vec![
                row(
                    incarnation("%1", 1, 10, 100, "codex"),
                    AgentState::Busy,
                    "s",
                    0,
                    0,
                ),
                row(selected.clone(), AgentState::Unknown, "s", 0, 1),
            ],
        ))
        .unwrap();
        assert_eq!(app.selected, Some(selected));

        let replacement = incarnation("%2", 2, 21, 201, "codex");
        app.apply_later_snapshot(snapshot(
            3,
            3,
            vec![row(replacement, AgentState::Idle, "s", 0, 1)],
        ))
        .unwrap();
        assert_eq!(app.selected, None);
    }

    #[test]
    fn snapshot_removed_selection_uses_nearest_row_then_handles_empty_snapshot() {
        let first = incarnation("%1", 1, 10, 100, "codex");
        let middle = incarnation("%2", 2, 20, 200, "codex");
        let last = incarnation("%3", 3, 30, 300, "codex");
        let mut app = App::new(
            ConnectionState::Live,
            snapshot(
                1,
                1,
                vec![
                    row(first.clone(), AgentState::Idle, "s", 0, 0),
                    row(middle.clone(), AgentState::Idle, "s", 0, 1),
                    row(last.clone(), AgentState::Idle, "s", 0, 2),
                ],
            ),
            empty_search(),
            Some(middle),
        );

        app.apply_later_snapshot(snapshot(
            2,
            2,
            vec![
                row(first, AgentState::Idle, "s", 0, 0),
                row(last.clone(), AgentState::Idle, "s", 0, 2),
            ],
        ))
        .unwrap();
        assert_eq!(app.selected, Some(last));

        app.apply_later_snapshot(snapshot(3, 3, Vec::new()))
            .unwrap();
        assert_eq!(app.selected, None);
        assert!(app.snapshot.rows.is_empty());
    }

    #[test]
    fn snapshot_disconnect_distinguishes_unavailable_from_stale() {
        let mut unavailable = empty_app();
        unavailable.mark_disconnected();
        assert_eq!(unavailable.connection, ConnectionState::Unavailable);

        let mut stale = empty_app();
        stale
            .apply_first_snapshot(snapshot(1, 1, Vec::new()))
            .unwrap();
        stale.mark_disconnected();
        assert_eq!(stale.connection, ConnectionState::Stale);
    }

    #[test]
    fn search_edit_mode_handles_q_unicode_backspace_accept_and_clear() {
        let mut app = empty_app();

        assert_eq!(app.handle_key(KeyCode::Char('f')), Effect::None);
        assert!(app.search.editing);
        assert_eq!(app.handle_key(KeyCode::Char('q')), Effect::None);
        assert_eq!(app.handle_key(KeyCode::Char('界')), Effect::None);
        assert_eq!(app.search.query, "q界");

        assert_eq!(app.handle_key(KeyCode::Backspace), Effect::None);
        assert_eq!(app.search.query, "q");
        assert_eq!(app.handle_key(KeyCode::Enter), Effect::None);
        assert!(!app.search.editing);
        assert_eq!(app.search.query, "q");
        assert_eq!(app.handle_key(KeyCode::Esc), Effect::None);
        assert_eq!(app.search, empty_search());

        app.handle_key(KeyCode::Char('f'));
        assert_eq!(app.handle_key(KeyCode::Esc), Effect::None);
        assert_eq!(app.search, empty_search());
    }

    #[test]
    fn search_keys_outside_editing_produce_quit_retry_and_navigation_effects() {
        let selected = incarnation("%4", 4, 40, 400, "codex");
        let mut app = App::new(
            ConnectionState::Live,
            snapshot(
                1,
                1,
                vec![row(selected.clone(), AgentState::Busy, "s", 0, 0)],
            ),
            empty_search(),
            Some(selected),
        );

        assert_eq!(
            app.handle_key(KeyCode::Enter),
            Effect::Navigate {
                pane_id: "%4".into()
            }
        );
        assert_eq!(app.handle_key(ctrl('l')), Effect::Retry);
        assert_eq!(app.handle_key(KeyCode::Char('q')), Effect::Quit);
        assert_eq!(app.handle_key(KeyCode::Esc), Effect::None);

        app.selected = None;
        assert_eq!(app.handle_key(KeyCode::Enter), Effect::None);
    }

    #[test]
    fn search_matches_each_supported_field_case_insensitively() {
        let matched = AgentRow {
            incarnation: incarnation("%17", 17, 170, 1_700, "provider-id"),
            provider_display_name: "Codex Prime".into(),
            tmux_target: "Agents:2.7".into(),
            session_name: "Agents".into(),
            window_index: 2,
            pane_index: 7,
            working_directory: "/Users/example/Project-Zeta".into(),
            work_summary: Some("Map Harold snapshots".into()),
            state: AgentState::Busy,
            last_transition_at_ms: 1,
        };
        let other = row(
            incarnation("%18", 18, 180, 1_800, "claude"),
            AgentState::Idle,
            "review",
            0,
            0,
        );

        for query in ["CODEX", "hArOlD", "AGENTS:2.7", "project-ZETA"] {
            let app = App::new(
                ConnectionState::Live,
                snapshot(1, 1, vec![matched.clone(), other.clone()]),
                SearchState {
                    query: query.into(),
                    editing: false,
                },
                None,
            );
            assert_eq!(
                app.visible_rows()
                    .iter()
                    .map(|agent| agent.incarnation.pane_id.as_str())
                    .collect::<Vec<_>>(),
                vec!["%17"],
                "query {query:?} should match"
            );
        }
    }

    #[test]
    fn search_streaming_snapshot_keeps_query_and_selects_first_visible_match() {
        let hidden = incarnation("%1", 1, 10, 100, "codex");
        let first_match = incarnation("%2", 2, 20, 200, "claude");
        let later_match = incarnation("%3", 3, 30, 300, "claude");
        let mut app = App::new(
            ConnectionState::Live,
            snapshot(
                1,
                1,
                vec![row(hidden.clone(), AgentState::Busy, "one", 0, 0)],
            ),
            SearchState {
                query: "needle".into(),
                editing: false,
            },
            Some(hidden),
        );

        let mut first = row(first_match.clone(), AgentState::Idle, "two", 0, 0);
        first.work_summary = Some("Needle first".into());
        let mut later = row(later_match, AgentState::Unknown, "zthree", 0, 0);
        later.work_summary = Some("another NEEDLE".into());
        app.apply_later_snapshot(snapshot(2, 2, vec![later, first]))
            .unwrap();

        assert_eq!(app.search.query, "needle");
        assert_eq!(app.visible_counts(), (2, 2));
        assert_eq!(app.selected, Some(first_match));
    }

    #[test]
    fn search_authoritative_snapshot_reselects_when_stable_incarnation_is_hidden() {
        let selected = incarnation("%1", 1, 10, 100, "codex");
        let visible = incarnation("%2", 2, 20, 200, "claude");
        let mut old_selected = row(selected.clone(), AgentState::Busy, "one", 0, 0);
        old_selected.work_summary = Some("needle before reconnect".into());
        let mut app = App::new(
            ConnectionState::Stale,
            snapshot(8, 8, vec![old_selected]),
            SearchState {
                query: "needle".into(),
                editing: false,
            },
            Some(selected.clone()),
        );
        let hidden_selected = row(selected, AgentState::Idle, "one", 0, 0);
        let mut new_visible = row(visible.clone(), AgentState::Idle, "two", 0, 0);
        new_visible.work_summary = Some("new NEEDLE result".into());

        app.begin_connection();
        app.apply_first_snapshot(snapshot(1, 1, vec![hidden_selected, new_visible]))
            .unwrap();

        assert_eq!(app.selected, Some(visible));
    }

    #[test]
    fn search_authoritative_snapshot_selects_first_match_without_prior_selection() {
        let first_match = incarnation("%1", 1, 10, 100, "codex");
        let later_match = incarnation("%2", 2, 20, 200, "claude");
        let mut app = App::new(
            ConnectionState::Unavailable,
            snapshot(0, 0, Vec::new()),
            SearchState {
                query: "needle".into(),
                editing: false,
            },
            None,
        );
        let mut first = row(first_match.clone(), AgentState::Busy, "one", 0, 0);
        first.work_summary = Some("first needle".into());
        let mut later = row(later_match, AgentState::Idle, "two", 0, 0);
        later.work_summary = Some("later NEEDLE".into());

        app.begin_connection();
        app.apply_first_snapshot(snapshot(1, 1, vec![later, first]))
            .unwrap();

        assert_eq!(app.selected, Some(first_match));
    }

    #[test]
    fn search_authoritative_snapshot_selects_first_match_after_selected_departed() {
        let departed = incarnation("%9", 9, 90, 900, "codex");
        let first_match = incarnation("%1", 1, 10, 100, "claude");
        let mut app = App::new(
            ConnectionState::Stale,
            snapshot(
                8,
                8,
                vec![row(departed.clone(), AgentState::Busy, "old", 0, 0)],
            ),
            SearchState {
                query: "needle".into(),
                editing: false,
            },
            Some(departed),
        );
        let mut matching = row(first_match.clone(), AgentState::Idle, "new", 0, 0);
        matching.work_summary = Some("replacement inventory needle".into());

        app.begin_connection();
        app.apply_first_snapshot(snapshot(1, 1, vec![matching]))
            .unwrap();

        assert_eq!(app.selected, Some(first_match));
    }

    #[test]
    fn search_authoritative_same_pane_replacement_selects_first_match_not_old_incarnation() {
        let departed = incarnation("%2", 2, 20, 200, "codex");
        let first_match = incarnation("%1", 1, 10, 100, "claude");
        let replacement = incarnation("%2", 2, 21, 201, "codex");
        let mut old = row(departed.clone(), AgentState::Busy, "old", 0, 0);
        old.work_summary = Some("needle before restart".into());
        let mut app = App::new(
            ConnectionState::Stale,
            snapshot(8, 8, vec![old]),
            SearchState {
                query: "needle".into(),
                editing: false,
            },
            Some(departed.clone()),
        );
        let mut first = row(first_match.clone(), AgentState::Busy, "new", 0, 0);
        first.work_summary = Some("first needle".into());
        let mut restarted = row(replacement.clone(), AgentState::Idle, "old", 0, 0);
        restarted.work_summary = Some("restarted needle".into());

        app.begin_connection();
        app.apply_first_snapshot(snapshot(1, 1, vec![restarted, first]))
            .unwrap();

        assert_eq!(app.selected, Some(first_match));
        assert_ne!(app.selected, Some(departed));
        assert_ne!(app.selected, Some(replacement));
    }

    #[test]
    fn search_selection_and_navigation_are_restricted_to_visible_rows() {
        let one = incarnation("%1", 1, 10, 100, "codex");
        let two = incarnation("%2", 2, 20, 200, "codex");
        let three = incarnation("%3", 3, 30, 300, "codex");
        let mut first = row(one, AgentState::Busy, "s", 0, 0);
        first.work_summary = Some("match one".into());
        let hidden = row(two, AgentState::Idle, "s", 0, 1);
        let mut last = row(three.clone(), AgentState::Unknown, "s", 0, 2);
        last.work_summary = Some("match three".into());
        let mut app = App::new(
            ConnectionState::Live,
            snapshot(1, 1, vec![first, hidden, last]),
            SearchState {
                query: "match".into(),
                editing: false,
            },
            None,
        );

        app.handle_key(KeyCode::Char('g'));
        assert_eq!(app.selected.as_ref().unwrap().pane_id, "%1");
        app.handle_key(KeyCode::Down);
        assert_eq!(app.selected, Some(three.clone()));
        app.handle_key(KeyCode::Char('j'));
        assert_eq!(app.selected, Some(three.clone()));
        app.handle_key(KeyCode::Up);
        assert_eq!(app.selected.as_ref().unwrap().pane_id, "%1");
        app.handle_key(KeyCode::Char('G'));
        assert_eq!(app.selected, Some(three));
        app.handle_key(KeyCode::Char('k'));
        assert_eq!(app.selected.as_ref().unwrap().pane_id, "%1");
        assert_eq!(app.visible_counts(), (2, 3));
    }

    #[test]
    fn search_no_matches_has_no_selection_and_clearing_does_not_resurrect_it() {
        let first = incarnation("%1", 1, 10, 100, "codex");
        let selected = incarnation("%2", 2, 20, 200, "codex");
        let mut app = App::new(
            ConnectionState::Live,
            snapshot(
                1,
                1,
                vec![
                    row(first.clone(), AgentState::Busy, "s", 0, 0),
                    row(selected.clone(), AgentState::Idle, "s", 0, 1),
                ],
            ),
            empty_search(),
            Some(selected),
        );

        app.handle_key(KeyCode::Char('f'));
        for character in "absent".chars() {
            app.handle_key(KeyCode::Char(character));
        }
        assert_eq!(app.visible_counts(), (0, 2));
        assert_eq!(app.selected, None);

        app.handle_key(KeyCode::Esc);
        assert_eq!(app.visible_counts(), (2, 2));
        assert_eq!(app.selected, Some(first));
    }

    fn empty_app() -> App {
        App::new(
            ConnectionState::Connecting,
            snapshot(0, 0, Vec::new()),
            empty_search(),
            None,
        )
    }

    fn empty_search() -> SearchState {
        SearchState {
            query: String::new(),
            editing: false,
        }
    }

    fn snapshot(through_event_version: u64, server_time_ms: i64, rows: Vec<AgentRow>) -> Snapshot {
        Snapshot {
            harold_version: None,
            tts_muted: false,
            messaging_paused: false,
            through_event_version,
            server_time_ms,
            monitor_health: Vec::new(),
            rows,
        }
    }

    #[test]
    fn a_disconnect_clears_the_harold_version_and_a_reconnect_shows_the_new_one() {
        let search = SearchState {
            query: String::new(),
            editing: false,
        };
        let mut app = App::new(
            ConnectionState::Connecting,
            snapshot(0, 0, Vec::new()),
            search,
            None,
        );
        let with_version = |version: &str| Snapshot {
            harold_version: Some(version.to_owned()),
            ..snapshot(1, 1_000, Vec::new())
        };

        app.apply_first_snapshot(with_version("v0.8.0")).unwrap();
        assert_eq!(app.snapshot.harold_version.as_deref(), Some("v0.8.0"));

        app.mark_disconnected();
        assert_eq!(app.snapshot.harold_version, None);

        app.apply_first_snapshot(with_version("v0.9.0")).unwrap();
        assert_eq!(app.snapshot.harold_version.as_deref(), Some("v0.9.0"));
    }

    #[test]
    fn the_dash_version_defaults_to_source_in_tests() {
        let search = SearchState {
            query: String::new(),
            editing: false,
        };
        let app = App::new(
            ConnectionState::Connecting,
            snapshot(0, 0, Vec::new()),
            search,
            None,
        );
        assert_eq!(app.dash_version, "source");
    }

    #[test]
    fn an_unset_or_empty_release_version_is_source() {
        assert_eq!(version_or_source(None), "source");
        assert_eq!(version_or_source(Some("")), "source");
        assert_eq!(version_or_source(Some("v0.9.0")), "v0.9.0");
    }

    fn health(state: MonitorHealthState, reason_code: &str) -> MonitorHealth {
        MonitorHealth {
            component: "inventory".into(),
            state,
            reason_code: reason_code.into(),
            observed_at_ms: 123,
        }
    }

    fn incarnation(
        pane_id: &str,
        pane_pid: u32,
        agent_pid: u32,
        agent_started_at_ms: i64,
        provider_id: &str,
    ) -> AgentIncarnation {
        AgentIncarnation {
            pane_id: pane_id.into(),
            pane_pid,
            agent_pid,
            agent_started_at_ms,
            provider_id: provider_id.into(),
        }
    }

    fn row(
        incarnation: AgentIncarnation,
        state: AgentState,
        session_name: &str,
        window_index: u32,
        pane_index: u32,
    ) -> AgentRow {
        AgentRow {
            tmux_target: format!("{session_name}:{window_index}.{pane_index}"),
            session_name: session_name.into(),
            window_index,
            pane_index,
            provider_display_name: incarnation.provider_id.clone(),
            working_directory: format!("/work/{session_name}"),
            work_summary: Some(format!("work in {session_name}")),
            incarnation,
            state,
            last_transition_at_ms: 10,
        }
    }

    fn live_voice_app(muted: bool) -> App {
        let snapshot = Snapshot {
            harold_version: None,
            through_event_version: 1,
            server_time_ms: 0,
            monitor_health: Vec::new(),
            rows: Vec::new(),
            tts_muted: muted,
            messaging_paused: false,
        };
        let mut app = App::new(
            ConnectionState::Connecting,
            snapshot.clone(),
            SearchState {
                query: String::new(),
                editing: false,
            },
            None,
        );
        app.apply_first_snapshot(snapshot).unwrap();
        app
    }

    fn type_text(app: &mut App, text: &str) {
        for character in text.chars() {
            app.handle_key(KeyCode::Char(character));
        }
    }

    #[test]
    fn f_opens_agent_search_and_slash_opens_the_palette() {
        let mut app = live_voice_app(false);
        assert_eq!(app.handle_key(KeyCode::Char('f')), Effect::None);
        assert!(app.search.editing);
        assert!(app.palette.is_none());

        let mut app = live_voice_app(false);
        assert_eq!(app.handle_key(KeyCode::Char('/')), Effect::None);
        assert!(app.palette.is_some());
        assert!(!app.search.editing);
    }

    #[test]
    fn slash_and_f_are_plain_characters_inside_either_input() {
        let mut app = live_voice_app(false);
        app.handle_key(KeyCode::Char('f'));
        type_text(&mut app, "a/f");
        assert_eq!(app.search.query, "a/f");
        assert!(app.palette.is_none());

        let mut app = live_voice_app(false);
        app.handle_key(KeyCode::Char('/'));
        type_text(&mut app, "f/");
        assert_eq!(app.palette.as_ref().unwrap().query, "f/");
        assert!(!app.search.editing);
    }

    #[test]
    fn palette_offers_only_the_applicable_voice_action_and_runs_it() {
        let mut app = live_voice_app(false);
        app.handle_key(KeyCode::Char('/'));
        type_text(&mut app, "voi");
        let labels: Vec<_> = app.palette_entries().iter().map(|e| e.label).collect();
        assert_eq!(labels, ["Voice: mute"]);
        assert_eq!(app.handle_key(KeyCode::Enter), Effect::SetTtsMuted(true));
        assert!(app.palette.is_none());

        let mut app = live_voice_app(true);
        app.handle_key(KeyCode::Char('/'));
        type_text(&mut app, "UNMUTE");
        assert_eq!(app.handle_key(KeyCode::Enter), Effect::SetTtsMuted(false));
    }

    #[test]
    fn palette_filter_with_no_match_does_nothing_on_enter_and_esc_closes() {
        let mut app = live_voice_app(false);
        app.handle_key(KeyCode::Char('/'));
        type_text(&mut app, "zzz");
        assert!(app.palette_entries().is_empty());
        assert_eq!(app.handle_key(KeyCode::Enter), Effect::None);
        assert!(app.palette.is_some());
        assert_eq!(app.handle_key(KeyCode::Esc), Effect::None);
        assert!(app.palette.is_none());
    }

    #[test]
    fn palette_backspace_edits_and_resets_selection() {
        let mut app = live_voice_app(false);
        app.handle_key(KeyCode::Char('/'));
        type_text(&mut app, "voz");
        assert!(app.palette_entries().is_empty());
        app.handle_key(KeyCode::Backspace);
        assert_eq!(app.palette_entries().len(), 1);
        assert_eq!(app.palette.as_ref().unwrap().selected, 0);
    }

    #[test]
    fn voice_actions_are_hidden_before_a_snapshot_and_when_not_live() {
        let mut app = App::new(
            ConnectionState::Connecting,
            Snapshot {
                harold_version: None,
                through_event_version: 0,
                server_time_ms: 0,
                monitor_health: Vec::new(),
                rows: Vec::new(),
                tts_muted: false,
                messaging_paused: false,
            },
            SearchState {
                query: String::new(),
                editing: false,
            },
            None,
        );
        app.handle_key(KeyCode::Char('/'));
        assert!(app.palette_entries().is_empty());

        let mut stale = live_voice_app(false);
        stale.mark_disconnected();
        stale.handle_key(KeyCode::Char('/'));
        assert!(stale.palette_entries().is_empty());
    }

    fn live_messaging_app(paused: bool) -> App {
        let mut app = live_voice_app(false);
        app.snapshot.messaging_paused = paused;
        app
    }

    #[test]
    fn palette_offers_only_the_applicable_messaging_action_and_runs_it() {
        let mut app = live_messaging_app(false);
        assert!(!app.messaging_paused());
        app.handle_key(KeyCode::Char('/'));
        type_text(&mut app, "messaging");
        let labels: Vec<_> = app.palette_entries().iter().map(|e| e.label).collect();
        assert_eq!(labels, ["Messaging: pause"]);
        assert_eq!(
            app.handle_key(KeyCode::Enter),
            Effect::SetMessagingPaused(true)
        );
        assert!(app.palette.is_none());

        let mut app = live_messaging_app(true);
        assert!(app.messaging_paused());
        app.handle_key(KeyCode::Char('/'));
        type_text(&mut app, "MESSAGING");
        let labels: Vec<_> = app.palette_entries().iter().map(|e| e.label).collect();
        assert_eq!(labels, ["Messaging: resume"]);
        assert_eq!(
            app.handle_key(KeyCode::Enter),
            Effect::SetMessagingPaused(false)
        );
    }

    #[test]
    fn unfiltered_palette_lists_voice_then_messaging() {
        let mut app = live_messaging_app(false);
        app.handle_key(KeyCode::Char('/'));
        let labels: Vec<_> = app.palette_entries().iter().map(|e| e.label).collect();
        assert_eq!(labels, ["Voice: mute", "Messaging: pause"]);
    }

    #[test]
    fn messaging_actions_are_hidden_before_a_snapshot_and_when_not_live() {
        let mut app = App::new(
            ConnectionState::Connecting,
            Snapshot {
                harold_version: None,
                through_event_version: 0,
                server_time_ms: 0,
                monitor_health: Vec::new(),
                rows: Vec::new(),
                tts_muted: false,
                messaging_paused: true,
            },
            SearchState {
                query: String::new(),
                editing: false,
            },
            None,
        );
        assert!(!app.messaging_paused());
        app.handle_key(KeyCode::Char('/'));
        assert!(app.palette_entries().is_empty());

        let mut stale = live_messaging_app(true);
        stale.mark_disconnected();
        stale.handle_key(KeyCode::Char('/'));
        assert!(stale.palette_entries().is_empty());
        // The last confirmed state stays on show while the stream is down.
        assert!(stale.messaging_paused());
    }

    #[test]
    fn quit_and_navigation_keys_are_inert_while_the_palette_is_open() {
        let mut app = live_voice_app(false);
        app.handle_key(KeyCode::Char('/'));
        assert_eq!(app.handle_key(KeyCode::Char('q')), Effect::None);
        assert_eq!(app.palette.as_ref().unwrap().query, "q");
    }

    fn placed(
        pane_id: &str,
        state: AgentState,
        session: &str,
        directory: &str,
        pane_index: u32,
    ) -> AgentRow {
        let mut placed = row(
            incarnation(pane_id, 1, 2, 3, "claude"),
            state,
            session,
            0,
            pane_index,
        );
        placed.working_directory = directory.into();
        placed
    }

    #[test]
    fn group_is_the_first_word_of_the_session_and_name_is_the_last_directory() {
        let main = placed(
            "%1",
            AgentState::Idle,
            "harold  main",
            "/Users/k/Dev/p/harold/main",
            3,
        );
        assert_eq!(main.group(), "harold");
        assert_eq!(main.name(), "main");

        let drifted = placed(
            "%2",
            AgentState::Idle,
            "harold  voice-mute-palette1",
            "/Users/k/Dev/p/harold/voice-mute-palette/",
            3,
        );
        assert_eq!(drifted.group(), "harold");
        assert_eq!(drifted.name(), "voice-mute-palette");

        let single = placed(
            "%3",
            AgentState::Idle,
            "kahgeh-com",
            "/Users/k/Dev/p/kahgeh-com",
            3,
        );
        assert_eq!(
            (single.group(), single.name()),
            ("kahgeh-com", "kahgeh-com")
        );

        let dotted = placed("%4", AgentState::Idle, " home", "/Users/k/.claude", 3);
        assert_eq!((dotted.group(), dotted.name()), ("home", ".claude"));
    }

    #[test]
    fn name_is_never_empty() {
        assert_eq!(placed("%1", AgentState::Idle, "s", "/", 0).name(), "/");
        let mut blank = placed("%2", AgentState::Idle, "s", "", 4);
        blank.tmux_target = "s:0.4".into();
        assert_eq!(blank.name(), "s:0.4");
        let mut nameless = placed("%3", AgentState::Idle, "   ", "/w/x", 0);
        nameless.session_name = "   ".into();
        assert_eq!(nameless.group(), "   ");
    }

    #[test]
    fn provider_tag_is_two_letters() {
        let mut agent = placed("%1", AgentState::Idle, "s", "/w", 0);
        agent.incarnation.provider_id = "claude".into();
        agent.provider_display_name = "Claude Code".into();
        assert_eq!(agent.provider_tag(), "cc");
        agent.incarnation.provider_id = "codex".into();
        agent.provider_display_name = "Codex".into();
        assert_eq!(agent.provider_tag(), "cx");
        agent.incarnation.provider_id = "opencode".into();
        agent.provider_display_name = "OpenCode".into();
        assert_eq!(agent.provider_tag(), "op");
    }

    #[test]
    fn rows_sort_by_group_then_name_and_ignore_state() {
        let mut app = empty_app();
        app.apply_first_snapshot(snapshot(
            1,
            0,
            vec![
                placed("%4", AgentState::Busy, "home", "/Users/k/Dev/xn", 2),
                placed(
                    "%2",
                    AgentState::Idle,
                    "harold  voice",
                    "/p/harold/voice",
                    3,
                ),
                placed("%3", AgentState::Unknown, "home", "/Users/k/Dev/p/sre", 1),
                placed("%1", AgentState::Busy, "harold  main", "/p/harold/main", 3),
            ],
        ))
        .unwrap();
        let order = |app: &App| -> Vec<String> {
            app.snapshot
                .rows
                .iter()
                .map(|row| row.incarnation.pane_id.clone())
                .collect()
        };
        assert_eq!(order(&app), ["%1", "%2", "%3", "%4"]);

        // Every state flips; positions must not move.
        app.apply_later_snapshot(snapshot(
            2,
            0,
            vec![
                placed("%4", AgentState::Idle, "home", "/Users/k/Dev/xn", 2),
                placed(
                    "%2",
                    AgentState::Busy,
                    "harold  voice",
                    "/p/harold/voice",
                    3,
                ),
                placed("%3", AgentState::Busy, "home", "/Users/k/Dev/p/sre", 1),
                placed(
                    "%1",
                    AgentState::Unknown,
                    "harold  main",
                    "/p/harold/main",
                    3,
                ),
            ],
        ))
        .unwrap();
        assert_eq!(order(&app), ["%1", "%2", "%3", "%4"]);
    }

    #[test]
    fn labels_append_the_pane_address_only_when_a_name_is_shared_in_a_group() {
        let mut app = empty_app();
        app.apply_first_snapshot(snapshot(
            1,
            0,
            vec![
                placed("%1", AgentState::Idle, "home", "/Users/k/.claude/skills", 3),
                placed("%2", AgentState::Idle, "home", "/Users/k/other/skills", 4),
                placed("%3", AgentState::Idle, "home", "/Users/k/Dev/p/sre", 1),
                placed("%4", AgentState::Idle, "work", "/w/skills", 0),
            ],
        ))
        .unwrap();
        let labels: Vec<String> = app
            .snapshot
            .rows
            .iter()
            .map(|row| app.label(row).to_owned())
            .collect();
        assert_eq!(labels, ["skills :0.3", "skills :0.4", "sre", "skills"]);
    }

    #[test]
    fn cross_session_collisions_in_one_group_still_get_distinct_labels() {
        let mut app = empty_app();
        let mut first = placed("%1", AgentState::Idle, "harold", "/w/main", 0);
        first.session_name = "harold  a".into();
        first.tmux_target = "harold  a:0.0".into();
        first.pane_index = 0;
        let mut second = placed("%2", AgentState::Idle, "harold", "/w/main", 0);
        second.session_name = "harold b".into();
        second.tmux_target = "harold b:0.0".into();
        second.pane_index = 0;
        let mut third = placed("%3", AgentState::Idle, "harold", "/w/main", 0);
        third.session_name = "harold".into();
        third.tmux_target = "harold:0.0".into();
        third.pane_index = 0;
        app.apply_first_snapshot(snapshot(1, 0, vec![first, second, third]))
            .unwrap();
        let mut labels: Vec<String> = app
            .snapshot
            .rows
            .iter()
            .map(|row| app.label(row).to_owned())
            .collect();
        labels.sort();
        labels.dedup();
        assert_eq!(labels.len(), 3, "{labels:?}");
        for row in &app.snapshot.rows {
            let (name, suffix) = app.label_parts(row);
            assert_eq!(name, "main");
            assert!(!suffix.is_empty());
        }
    }

    #[test]
    fn visible_groups_follow_the_search_filter_and_drop_empty_groups() {
        let mut app = empty_app();
        app.apply_first_snapshot(snapshot(
            1,
            0,
            vec![
                placed("%1", AgentState::Idle, "harold  main", "/p/harold/main", 3),
                placed("%2", AgentState::Idle, "home", "/Users/k/Dev/p/sre", 1),
                placed("%3", AgentState::Idle, "home", "/Users/k/Dev/xn", 2),
            ],
        ))
        .unwrap();
        let groups: Vec<(&str, usize)> = app
            .visible_groups()
            .into_iter()
            .map(|(group, rows)| (group, rows.len()))
            .collect();
        assert_eq!(groups, [("harold", 1), ("home", 2)]);

        app.handle_key(KeyCode::Char('f'));
        for character in "sre".chars() {
            app.handle_key(KeyCode::Char(character));
        }
        let groups: Vec<(&str, usize)> = app
            .visible_groups()
            .into_iter()
            .map(|(group, rows)| (group, rows.len()))
            .collect();
        assert_eq!(groups, [("home", 1)]);
    }

    fn fleet(states: [AgentState; 3], revision: u64) -> Snapshot {
        snapshot(
            revision,
            0,
            vec![
                placed("%1", states[0], "harold  main", "/p/harold/main", 3),
                placed("%2", states[1], "harold  voice", "/p/harold/voice", 3),
                placed("%3", states[2], "home", "/Users/k/Dev/p/sre", 1),
            ],
        )
    }

    fn waiting_panes(app: &App) -> Vec<String> {
        app.waiting_rows()
            .iter()
            .map(|row| row.incarnation.pane_id.clone())
            .collect()
    }

    #[test]
    fn busy_to_idle_joins_in_order_and_first_seen_idle_does_not() {
        let mut app = empty_app();
        app.apply_first_snapshot(fleet([Busy, Busy, Idle], 1))
            .unwrap();
        assert!(waiting_panes(&app).is_empty());

        app.apply_later_snapshot(fleet([Busy, Idle, Idle], 2))
            .unwrap();
        assert_eq!(waiting_panes(&app), ["%2"]);
        app.apply_later_snapshot(fleet([Idle, Idle, Idle], 3))
            .unwrap();
        assert_eq!(waiting_panes(&app), ["%2", "%1"]);
        assert!(app.is_waiting(&app.snapshot.rows[0].incarnation));
    }

    #[test]
    fn going_busy_or_disappearing_leaves_and_unknown_stays() {
        let mut app = empty_app();
        app.apply_first_snapshot(fleet([Busy, Busy, Busy], 1))
            .unwrap();
        app.apply_later_snapshot(fleet([Idle, Idle, Idle], 2))
            .unwrap();
        assert_eq!(waiting_panes(&app), ["%1", "%2", "%3"]);

        app.apply_later_snapshot(fleet([Busy, Unknown, Idle], 3))
            .unwrap();
        assert_eq!(waiting_panes(&app), ["%2", "%3"]);

        app.apply_later_snapshot(snapshot(
            4,
            0,
            vec![placed("%3", Idle, "home", "/Users/k/Dev/p/sre", 1)],
        ))
        .unwrap();
        assert_eq!(waiting_panes(&app), ["%3"]);
    }

    #[test]
    fn a_new_incarnation_in_the_same_pane_does_not_inherit_waiting() {
        let mut app = empty_app();
        app.apply_first_snapshot(fleet([Busy, Busy, Busy], 1))
            .unwrap();
        app.apply_later_snapshot(fleet([Idle, Busy, Busy], 2))
            .unwrap();
        let mut replacement = placed("%1", Idle, "harold  main", "/p/harold/main", 3);
        replacement.incarnation.agent_pid = 999;
        app.apply_later_snapshot(snapshot(3, 0, vec![replacement]))
            .unwrap();
        assert!(waiting_panes(&app).is_empty());
    }

    #[test]
    fn focus_clears_waiting_and_suppresses_joining_for_the_pane_in_view() {
        let mut app = empty_app();
        assert!(!app.focus_tracking());
        app.apply_first_snapshot(fleet([Busy, Busy, Busy], 1))
            .unwrap();
        app.apply_later_snapshot(fleet([Idle, Busy, Busy], 2))
            .unwrap();
        assert_eq!(waiting_panes(&app), ["%1"]);

        app.observe_focus(FocusReading::Active("%1".into()));
        assert!(app.focus_tracking());
        assert!(waiting_panes(&app).is_empty());

        // The user is now looking at %2 when it finishes: it never joins.
        app.observe_focus(FocusReading::Active("%2".into()));
        app.apply_later_snapshot(fleet([Idle, Idle, Busy], 3))
            .unwrap();
        assert!(waiting_panes(&app).is_empty());

        app.observe_focus(FocusReading::Unavailable);
        assert!(!app.focus_tracking());
        app.apply_later_snapshot(fleet([Idle, Idle, Idle], 4))
            .unwrap();
        assert_eq!(waiting_panes(&app), ["%3"]);
    }

    #[test]
    fn n_and_shift_n_cycle_waiting_agents_and_wrap() {
        let mut app = empty_app();
        app.apply_first_snapshot(fleet([Busy, Busy, Busy], 1))
            .unwrap();
        app.apply_later_snapshot(fleet([Busy, Busy, Idle], 2))
            .unwrap();
        app.apply_later_snapshot(fleet([Idle, Busy, Idle], 3))
            .unwrap();
        assert_eq!(waiting_panes(&app), ["%3", "%1"]);
        let selected = |app: &App| app.selected.as_ref().map(|s| s.pane_id.clone());

        app.selected = Some(app.snapshot.rows[1].incarnation.clone()); // %2, not waiting
        assert_eq!(app.handle_key(KeyCode::Char('n')), Effect::None);
        assert_eq!(selected(&app).as_deref(), Some("%3"));
        app.handle_key(KeyCode::Char('n'));
        assert_eq!(selected(&app).as_deref(), Some("%1"));
        app.handle_key(KeyCode::Char('n'));
        assert_eq!(selected(&app).as_deref(), Some("%3"));
        assert_eq!(app.handle_key(shift_n()), Effect::None);
        assert_eq!(selected(&app).as_deref(), Some("%1"));

        app.selected = Some(app.snapshot.rows[1].incarnation.clone());
        app.handle_key(shift_n());
        assert_eq!(selected(&app).as_deref(), Some("%1"));
    }

    #[test]
    fn n_and_shift_n_do_nothing_when_nothing_waits() {
        let mut app = empty_app();
        app.apply_first_snapshot(fleet([Busy, Busy, Busy], 1))
            .unwrap();
        let before = app.selected.clone();
        assert_eq!(app.handle_key(KeyCode::Char('n')), Effect::None);
        assert_eq!(app.handle_key(shift_n()), Effect::None);
        assert_eq!(app.selected, before);
    }

    #[test]
    fn stepping_to_a_waiting_agent_hidden_by_search_clears_the_search() {
        let mut app = empty_app();
        app.apply_first_snapshot(fleet([Busy, Busy, Busy], 1))
            .unwrap();
        app.apply_later_snapshot(fleet([Busy, Busy, Idle], 2))
            .unwrap();
        app.handle_key(KeyCode::Char('f'));
        for character in "main".chars() {
            app.handle_key(KeyCode::Char(character));
        }
        app.handle_key(KeyCode::Enter);
        assert_eq!(app.visible_rows().len(), 1);

        app.handle_key(KeyCode::Char('n'));
        assert!(app.search.query.is_empty());
        assert_eq!(app.selected.as_ref().unwrap().pane_id, "%3");
        assert_eq!(app.visible_rows().len(), 3);
    }

    fn call_view(state: CallState, chief_pane_id: &str) -> CallView {
        CallView {
            state,
            chief_pane_id: chief_pane_id.into(),
            last_transcript: String::new(),
            last_result: None,
        }
    }

    /// The fleet with the call known and the chief (%3) selected.
    fn call_app(state: CallState) -> App {
        let mut app = empty_app();
        app.apply_first_snapshot(fleet([Idle, Idle, Idle], 1))
            .unwrap();
        app.observe_call(Some(call_view(state, "%3")));
        app.selected = Some(app.chief_row().unwrap().incarnation.clone());
        app
    }

    fn select_pane(app: &mut App, pane_id: &str) {
        app.selected = app
            .snapshot
            .rows
            .iter()
            .find(|row| row.incarnation.pane_id == pane_id)
            .map(|row| row.incarnation.clone());
    }

    fn palette_labels(app: &mut App, query: &str) -> Vec<&'static str> {
        app.handle_key(KeyCode::Char('/'));
        type_text(app, query);
        app.palette_entries().iter().map(|e| e.label).collect()
    }

    #[test]
    fn c_on_the_chief_row_starts_and_ends_a_call() {
        let mut app = call_app(CallState::Off);
        assert_eq!(
            app.handle_key(KeyCode::Char('c')),
            Effect::Call(CallAction::Start)
        );
        for state in [
            CallState::Listening,
            CallState::Thinking,
            CallState::Speaking,
        ] {
            let mut app = call_app(state);
            assert_eq!(
                app.handle_key(KeyCode::Char('c')),
                Effect::Call(CallAction::Stop),
                "{state:?}"
            );
        }
    }

    #[test]
    fn ctrl_c_on_the_chief_row_quits_and_alt_c_does_nothing() {
        for state in [
            CallState::Off,
            CallState::Listening,
            CallState::Thinking,
            CallState::Speaking,
        ] {
            let mut app = call_app(state);
            assert_eq!(app.handle_key(ctrl('c')), Effect::Quit, "{state:?}");
            assert_eq!(
                app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::ALT)),
                Effect::None,
                "{state:?}"
            );
        }
        let mut app = call_app(CallState::Off);
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)),
            Effect::Call(CallAction::Start)
        );
        let mut app = call_app(CallState::Listening);
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)),
            Effect::Call(CallAction::Stop)
        );
    }

    #[test]
    fn control_or_alt_t_does_not_talk_now() {
        for modifiers in [KeyModifiers::CONTROL, KeyModifiers::ALT] {
            let mut app = call_app(CallState::Speaking);
            assert_eq!(
                app.handle_key(KeyEvent::new(KeyCode::Char('t'), modifiers)),
                Effect::None,
                "{modifiers:?}"
            );
        }
        let mut app = call_app(CallState::Speaking);
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE)),
            Effect::Call(CallAction::TalkNow)
        );
    }

    #[test]
    fn ctrl_c_and_ctrl_q_quit_off_the_chief_row() {
        let mut app = call_app(CallState::Off);
        select_pane(&mut app, "%1");
        assert_eq!(app.handle_key(ctrl('c')), Effect::Quit);
        assert_eq!(app.handle_key(ctrl('q')), Effect::Quit);
    }

    #[test]
    fn c_and_t_do_nothing_on_other_rows() {
        for state in [
            CallState::Off,
            CallState::Listening,
            CallState::Thinking,
            CallState::Speaking,
        ] {
            for pane in ["%1", "%2"] {
                let mut app = call_app(state);
                select_pane(&mut app, pane);
                assert_eq!(app.handle_key(KeyCode::Char('c')), Effect::None);
                assert_eq!(app.handle_key(KeyCode::Char('t')), Effect::None);
            }
        }
    }

    #[test]
    fn t_talks_now_only_while_the_chief_speaks() {
        let mut app = call_app(CallState::Speaking);
        assert_eq!(
            app.handle_key(KeyCode::Char('t')),
            Effect::Call(CallAction::TalkNow)
        );
        for state in [
            CallState::NotSetUp,
            CallState::Off,
            CallState::Listening,
            CallState::Thinking,
        ] {
            let mut app = call_app(state);
            assert_eq!(
                app.handle_key(KeyCode::Char('t')),
                Effect::None,
                "{state:?}"
            );
        }
        let mut unknown = call_app(CallState::Speaking);
        unknown.observe_call(None);
        assert_eq!(unknown.handle_key(KeyCode::Char('t')), Effect::None);
    }

    #[test]
    fn c_does_nothing_when_not_set_up() {
        let mut app = call_app(CallState::NotSetUp);
        assert_eq!(app.handle_key(KeyCode::Char('c')), Effect::None);
        assert!(palette_labels(&mut app, "call").is_empty());

        let mut unknown = call_app(CallState::Off);
        unknown.observe_call(None);
        assert_eq!(unknown.handle_key(KeyCode::Char('c')), Effect::None);
        assert!(palette_labels(&mut unknown, "call").is_empty());
    }

    #[test]
    fn c_does_nothing_while_the_voice_is_muted_and_works_once_unmuted() {
        let mut app = call_app(CallState::Off);
        app.apply_later_snapshot(Snapshot {
            tts_muted: true,
            ..fleet([Idle, Idle, Idle], 2)
        })
        .unwrap();
        assert_eq!(app.voice(), VoiceState::Muted);
        assert_eq!(app.handle_key(KeyCode::Char('c')), Effect::None);
        assert!(!palette_labels(&mut app, "").contains(&"Call: start"));
        app.handle_key(KeyCode::Esc);

        app.apply_later_snapshot(fleet([Idle, Idle, Idle], 3))
            .unwrap();
        assert_eq!(
            app.handle_key(KeyCode::Char('c')),
            Effect::Call(CallAction::Start)
        );
    }

    #[test]
    fn palette_call_runs_the_same_action_from_any_row() {
        for (state, label, action) in [
            (CallState::Off, "Call: start", CallAction::Start),
            (CallState::Listening, "Call: end", CallAction::Stop),
            (CallState::Thinking, "Call: end", CallAction::Stop),
            (CallState::Speaking, "Call: end", CallAction::Stop),
        ] {
            for pane in ["%1", "%3"] {
                let mut app = call_app(state);
                select_pane(&mut app, pane);
                assert_eq!(palette_labels(&mut app, "call"), [label], "{state:?}");
                assert_eq!(app.handle_key(KeyCode::Enter), Effect::Call(action));
                assert!(app.palette.is_none());
            }
        }
    }

    #[test]
    fn the_chief_row_follows_chief_pane_id() {
        let mut app = call_app(CallState::Off);
        assert_eq!(app.chief_row().unwrap().incarnation.pane_id, "%3");

        app.observe_call(Some(call_view(CallState::Off, "%1")));
        assert_eq!(app.chief_row().unwrap().incarnation.pane_id, "%1");
        select_pane(&mut app, "%1");
        assert_eq!(
            app.handle_key(KeyCode::Char('c')),
            Effect::Call(CallAction::Start)
        );
        select_pane(&mut app, "%3");
        assert_eq!(app.handle_key(KeyCode::Char('c')), Effect::None);

        for missing in ["", "%9"] {
            app.observe_call(Some(call_view(CallState::Off, missing)));
            assert!(app.chief_row().is_none(), "{missing:?}");
        }
        app.observe_call(None);
        assert!(app.chief_row().is_none());

        // A disconnect forgets the call until the next answer.
        app.observe_call(Some(call_view(CallState::Off, "%3")));
        app.mark_disconnected();
        assert_eq!(app.call, None);
    }

    #[test]
    fn enter_clears_waiting_only_when_focus_tracking_is_off() {
        let mut app = empty_app();
        app.apply_first_snapshot(fleet([Busy, Busy, Busy], 1))
            .unwrap();
        app.apply_later_snapshot(fleet([Idle, Busy, Busy], 2))
            .unwrap();
        app.handle_key(KeyCode::Char('n'));
        assert_eq!(
            app.handle_key(KeyCode::Enter),
            Effect::Navigate {
                pane_id: "%1".into()
            }
        );
        assert!(waiting_panes(&app).is_empty());

        app.observe_focus(FocusReading::Active("%9".into()));
        app.apply_later_snapshot(fleet([Idle, Idle, Busy], 3))
            .unwrap();
        app.handle_key(KeyCode::Char('n'));
        app.handle_key(KeyCode::Enter);
        assert_eq!(
            waiting_panes(&app),
            ["%2"],
            "tracking on: only a real visit clears it"
        );
    }

    fn question_view(id: &str, version: u32) -> crate::questions::QuestionView {
        use crate::questions::{DecisionView, OptionView, QuestionView};
        let option = |key: &str, suggested: bool| OptionView {
            key: key.into(),
            label: format!("label {key}"),
            impact: format!("impact {key}"),
            suggested,
            free: false,
        };
        QuestionView {
            id: id.into(),
            product: "harold".into(),
            spec: "spec".into(),
            sha: "abc".into(),
            link: "https://example.com/spec".into(),
            intro: "intro".into(),
            version,
            deferred: false,
            decisions: vec![
                DecisionView {
                    ask: "Ask 1?".into(),
                    options: vec![option("a", true), option("b", false)],
                },
                DecisionView {
                    ask: "Ask 2?".into(),
                    options: vec![option("a", true), option("b", false)],
                },
            ],
        }
    }

    /// Opens a revise note from the block's first decision, through its action row.
    fn open_revise_note(app: &mut App) {
        for code in [
            KeyCode::Char('j'),
            KeyCode::Char('j'),
            KeyCode::Char('j'),
            KeyCode::Enter,
        ] {
            app.handle_key(code);
        }
    }

    fn question_queue(ids: &[&str]) -> crate::questions::Queue {
        crate::questions::Queue {
            questions: ids.iter().map(|id| question_view(id, 1)).collect(),
            waiting: ids.len() as u32,
            dismissed: Vec::new(),
        }
    }

    #[test]
    fn tab_moves_the_focus() {
        let mut app = call_app(CallState::Off);
        assert_eq!(app.handle_key(KeyCode::Tab), Effect::None);
        assert_eq!(app.focus, Focus::List, "no block, no focus to move");

        app.observe_questions(question_queue(&["Q1"]));
        assert_eq!(app.handle_key(KeyCode::Tab), Effect::None);
        assert_eq!(app.focus, Focus::Block);
        assert_eq!(app.handle_key(KeyCode::Tab), Effect::None);
        assert_eq!(app.focus, Focus::List);

        // Search and palette editing keep their keys.
        app.handle_key(KeyCode::Char('f'));
        app.handle_key(KeyCode::Tab);
        assert_eq!(app.focus, Focus::List);
        app.handle_key(KeyCode::Enter);
        app.handle_key(KeyCode::Char('/'));
        app.handle_key(KeyCode::Tab);
        assert_eq!(app.focus, Focus::List);
        app.handle_key(KeyCode::Esc);

        // Esc in the block gives the focus back.
        app.handle_key(KeyCode::Tab);
        assert_eq!(app.focus, Focus::Block);
        assert_eq!(app.handle_key(KeyCode::Esc), Effect::None);
        assert_eq!(app.focus, Focus::List);
    }

    #[test]
    fn r_and_l_do_nothing_and_c_calls_in_the_list() {
        let mut app = call_app(CallState::Off);
        app.observe_questions(question_queue(&["Q1"]));
        assert_eq!(app.focus, Focus::List);
        assert_eq!(app.handle_key(KeyCode::Char('r')), Effect::None);
        assert_eq!(
            app.handle_key(KeyCode::Char('c')),
            Effect::Call(CallAction::Start)
        );
        let before = app.selected.clone();
        assert_eq!(app.handle_key(KeyCode::Char('l')), Effect::None);
        assert_eq!(app.selected, before, "nothing waits, so l stays put");
        assert_eq!(app.questions.view().unwrap().mode, Mode::Choosing);
        assert!(app.questions.view().unwrap().picks.is_empty());
    }

    #[test]
    fn block_keys_apply_only_with_focus() {
        let mut app = call_app(CallState::Off);
        app.observe_questions(question_queue(&["Q1", "Q2"]));
        app.handle_key(KeyCode::Enter);
        assert!(app.questions.view().unwrap().picks.is_empty());

        app.handle_key(KeyCode::Tab);
        assert_eq!(app.handle_key(KeyCode::Char('c')), Effect::None);
        assert_eq!(app.handle_key(KeyCode::Char('r')), Effect::None);
        assert_eq!(app.questions.view().unwrap().mode, Mode::Choosing);
        assert_eq!(app.handle_key(KeyCode::Enter), Effect::None);
        assert_eq!(app.questions.view().unwrap().picks.len(), 1);
        app.handle_key(KeyCode::Enter);
        match app.handle_key(KeyCode::Enter) {
            Effect::Answer(out) => {
                assert_eq!((out.id.as_str(), out.version), ("Q1", 1));
                assert_eq!(out.kind, AnswerKind::Answer);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            app.handle_key(KeyCode::Char('l')),
            Effect::None,
            "the summary ignores l"
        );
        // Esc goes back to the last decision; its later row defers.
        app.handle_key(KeyCode::Esc);
        app.handle_key(KeyCode::Char('j'));
        app.handle_key(KeyCode::Char('j'));
        assert_eq!(app.handle_key(KeyCode::Enter), Effect::Defer("Q1".into()));
        assert_eq!(
            app.handle_key(KeyCode::Char('o')),
            Effect::OpenLink("https://example.com/spec".into())
        );
        assert_eq!(
            app.handle_key(KeyCode::Char('q')),
            Effect::None,
            "q is not quit in the block"
        );
    }

    #[test]
    fn a_notice_on_an_empty_block_goes_at_the_next_key_and_the_key_still_acts() {
        let mut app = call_app(CallState::Off);
        app.observe_questions(question_queue(&["Q1"]));
        app.observe_questions(question_queue(&[]));
        assert!(app.questions.shows_notice_only());
        assert_eq!(app.focus, Focus::List);

        // On the list: the notice goes and the key does what it does on the list.
        assert_eq!(app.handle_key(KeyCode::Char('q')), Effect::Quit);
        assert!(!app.questions.is_visible());

        // In the block: the key only clears it, and the focus returns to the list.
        app.observe_questions(question_queue(&["Q1"]));
        app.handle_key(KeyCode::Tab);
        app.observe_questions(question_queue(&[]));
        assert_eq!(app.focus, Focus::Block);
        assert_eq!(app.handle_key(KeyCode::Char('q')), Effect::None);
        assert!(!app.questions.is_visible());
        assert_eq!(app.focus, Focus::List);
    }

    #[test]
    fn a_notice_on_an_empty_block_goes_after_the_next_empty_list() {
        let mut app = call_app(CallState::Off);
        app.observe_questions(question_queue(&["Q1"]));
        app.handle_key(KeyCode::Tab);
        app.observe_questions(question_queue(&[]));
        assert!(app.questions.is_visible());
        app.observe_questions(question_queue(&[]));
        assert!(!app.questions.is_visible());
        assert_eq!(app.focus, Focus::List);
    }

    #[test]
    fn a_new_question_does_not_take_the_focus() {
        let mut app = call_app(CallState::Off);
        app.observe_questions(question_queue(&["Q1"]));
        assert_eq!(app.focus, Focus::List);
        app.observe_questions(question_queue(&["Q1", "Q2"]));
        assert_eq!(app.focus, Focus::List);

        // When the block hides, the focus returns to the list.
        app.handle_key(KeyCode::Tab);
        assert_eq!(app.focus, Focus::Block);
        app.hide_questions();
        assert!(!app.questions.is_visible());
        assert_eq!(app.focus, Focus::List);
    }

    #[test]
    fn keys_after_the_block_hides_under_focus_do_not_reach_the_list() {
        let mut app = call_app(CallState::Off);
        app.observe_questions(question_queue(&["Q1"]));
        app.handle_key(KeyCode::Tab);
        open_revise_note(&mut app);
        app.handle_key(KeyCode::Char('a'));
        // The list fails, or the stream ends, while a note is being typed.
        app.hide_questions();
        assert_eq!(app.focus, Focus::List);
        assert!(app.swallowing_keys());
        for code in [
            KeyCode::Char('q'),
            KeyCode::Enter,
            KeyCode::Char('c'),
            KeyCode::Char('r'),
            KeyCode::Char('f'),
            KeyCode::Char('/'),
        ] {
            assert_eq!(app.handle_key(code), Effect::None, "{code:?}");
        }
        assert!(!app.search.editing && app.palette.is_none());
        assert_eq!(app.handle_key(KeyCode::Esc), Effect::None);
        assert!(!app.swallowing_keys());
        assert_eq!(
            app.handle_key(KeyCode::Char('c')),
            Effect::Call(CallAction::Start)
        );

        // Tab ends it too, and the list keeps the focus.
        app.observe_questions(question_queue(&["Q1"]));
        app.handle_key(KeyCode::Tab);
        app.hide_questions();
        assert_eq!(app.handle_key(KeyCode::Char('q')), Effect::None);
        assert_eq!(app.handle_key(KeyCode::Tab), Effect::None);
        assert_eq!(app.focus, Focus::List);
        assert_eq!(app.handle_key(KeyCode::Char('q')), Effect::Quit);

        // Without the focus, a hidden block swallows nothing.
        let mut app = call_app(CallState::Off);
        app.observe_questions(question_queue(&["Q1"]));
        app.hide_questions();
        assert!(!app.swallowing_keys());
        assert_eq!(app.handle_key(KeyCode::Char('q')), Effect::Quit);
    }

    #[test]
    fn a_returning_block_takes_the_focus_back_and_typing_resumes() {
        let mut app = call_app(CallState::Off);
        app.observe_questions(question_queue(&["Q1"]));
        app.handle_key(KeyCode::Tab);
        open_revise_note(&mut app);
        app.handle_key(KeyCode::Char('a'));
        app.hide_questions();
        assert!(app.swallowing_keys());
        app.observe_questions(question_queue(&["Q1"]));
        assert_eq!(app.focus, Focus::Block);
        assert!(!app.swallowing_keys());
        assert!(!app.holding_block_keys());
        app.handle_key(KeyCode::Char('b'));
        assert_eq!(
            app.questions.view().unwrap().mode,
            Mode::Note {
                kind: crate::questions::NoteKind::Revise,
                text: "ab".into(),
            }
        );
    }

    #[test]
    fn a_returning_block_that_changed_swallows_until_esc() {
        let mut app = call_app(CallState::Off);
        app.observe_questions(question_queue(&["Q1"]));
        app.handle_key(KeyCode::Tab);
        open_revise_note(&mut app);
        app.handle_key(KeyCode::Char('a'));
        app.hide_questions();
        let mut changed = question_queue(&["Q1"]);
        changed.questions[0].version = 2;
        app.observe_questions(changed);
        assert_eq!(app.focus, Focus::Block);
        assert!(!app.swallowing_keys());
        assert!(app.holding_block_keys());
        for code in [
            KeyCode::Char('l'),
            KeyCode::Char('o'),
            KeyCode::Char('r'),
            KeyCode::Enter,
            KeyCode::Char('q'),
        ] {
            assert_eq!(app.handle_key(code), Effect::None, "{code:?}");
        }
        let view = app.questions.view().unwrap();
        assert_eq!(view.mode, Mode::Choosing);
        assert!(view.picks.is_empty());
        assert_eq!(
            view.notice.as_deref(),
            Some("Q1 changed, your picks were cleared")
        );
        // Esc reads the change: the notice goes, and the question is chosen from.
        assert_eq!(app.handle_key(KeyCode::Esc), Effect::None);
        assert!(!app.holding_block_keys());
        assert_eq!(app.focus, Focus::Block);
        assert_eq!(app.questions.view().unwrap().notice, None);
        app.handle_key(KeyCode::Char('j'));
        app.handle_key(KeyCode::Char('j'));
        assert_eq!(app.handle_key(KeyCode::Enter), Effect::Defer("Q1".into()));

        // Tab also ends the hold, moving to the list as usual.
        let mut app = call_app(CallState::Off);
        app.observe_questions(question_queue(&["Q1"]));
        app.handle_key(KeyCode::Tab);
        open_revise_note(&mut app);
        let mut changed = question_queue(&["Q1"]);
        changed.questions[0].version = 2;
        app.observe_questions(changed);
        assert!(app.holding_block_keys(), "a change under a note holds too");
        assert_eq!(app.handle_key(KeyCode::Tab), Effect::None);
        assert_eq!(app.focus, Focus::List);
        assert!(!app.holding_block_keys());

        // A change while choosing does not hold.
        let mut app = call_app(CallState::Off);
        app.observe_questions(question_queue(&["Q1"]));
        app.handle_key(KeyCode::Tab);
        let mut changed = question_queue(&["Q1"]);
        changed.questions[0].version = 2;
        app.observe_questions(changed);
        assert!(!app.holding_block_keys());
    }

    fn ctrl(character: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(character), KeyModifiers::CONTROL)
    }

    /// `N` as the terminal sends it: the capital letter with Shift held.
    fn shift_n() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('N'), KeyModifiers::SHIFT)
    }

    /// Everything a key could change: the focus, the selection, search, the palette,
    /// the swallow and hold flags, and the block.
    type KeyState = (
        Focus,
        Option<AgentIncarnation>,
        SearchState,
        Option<PaletteState>,
        bool,
        bool,
        Option<crate::questions::BlockView>,
    );

    fn key_state(app: &App) -> KeyState {
        (
            app.focus,
            app.selected.clone(),
            app.search.clone(),
            app.palette.clone(),
            app.swallowing_keys,
            app.holding_block_keys,
            app.questions.view(),
        )
    }

    /// The fleet with %3 and %1 waiting, the call off and %2 selected.
    fn waiting_app() -> App {
        let mut app = empty_app();
        app.apply_first_snapshot(fleet([Busy, Busy, Busy], 1))
            .unwrap();
        app.apply_later_snapshot(fleet([Busy, Busy, Idle], 2))
            .unwrap();
        app.apply_later_snapshot(fleet([Idle, Busy, Idle], 3))
            .unwrap();
        app.observe_call(Some(call_view(CallState::Off, "%3")));
        select_pane(&mut app, "%2");
        app
    }

    /// One app in each mode a key can meet, with two questions listed and text typed
    /// where the mode takes text.
    fn apps_in_every_mode() -> Vec<(&'static str, App)> {
        let list = || {
            let mut app = waiting_app();
            app.observe_questions(question_queue(&["Q1", "Q2"]));
            app
        };
        let mut block = list();
        block.handle_key(KeyCode::Tab);
        let mut summary = block.clone();
        summary.handle_key(KeyCode::Enter);
        summary.handle_key(KeyCode::Enter);
        assert_eq!(summary.questions.view().unwrap().mode, Mode::Summary);
        let mut note = block.clone();
        open_revise_note(&mut note);
        type_text(&mut note, "ab");
        let mut search = list();
        type_text(&mut search, "fma");
        let mut palette = list();
        type_text(&mut palette, "/ca");
        let mut holding = note.clone();
        let mut changed = question_queue(&["Q1", "Q2"]);
        changed.questions[0].version = 2;
        holding.observe_questions(changed);
        assert!(holding.holding_block_keys());
        let mut swallowing = note.clone();
        swallowing.hide_questions();
        assert!(swallowing.swallowing_keys());
        vec![
            ("list", list()),
            ("block", block),
            ("summary", summary),
            ("note", note),
            ("search", search),
            ("palette", palette),
            ("holding", holding),
            ("swallowing", swallowing),
        ]
    }

    #[test]
    fn quit_keys_quit_from_every_mode() {
        for character in ['q', 'c'] {
            for modifiers in [
                KeyModifiers::CONTROL,
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ] {
                for (mode, mut app) in apps_in_every_mode() {
                    let before = key_state(&app);
                    assert_eq!(
                        app.handle_key(KeyEvent::new(KeyCode::Char(character), modifiers)),
                        Effect::Quit,
                        "{mode} {character} {modifiers:?}"
                    );
                    assert_eq!(key_state(&app), before, "{mode} {character}");
                }
            }
        }
    }

    #[test]
    fn ctrl_l_retries_from_every_mode_and_types_nothing() {
        for (mode, mut app) in apps_in_every_mode() {
            let before = key_state(&app);
            assert_eq!(app.handle_key(ctrl('l')), Effect::Retry, "{mode}");
            assert_eq!(key_state(&app), before, "{mode}");
        }
    }

    #[test]
    fn chords_do_nothing_in_any_mode() {
        let alt = |character| KeyEvent::new(KeyCode::Char(character), KeyModifiers::ALT);
        for (mode, app) in apps_in_every_mode() {
            for chord in [
                ctrl('r'),
                alt('q'),
                ctrl('j'),
                ctrl('g'),
                alt('f'),
                alt('a'),
            ] {
                let mut app = app.clone();
                let before = key_state(&app);
                assert_eq!(app.handle_key(chord), Effect::None, "{mode} {chord:?}");
                assert_eq!(key_state(&app), before, "{mode} {chord:?}");
            }
        }
    }

    #[test]
    fn r_h_l_and_x_do_nothing_in_the_list() {
        let mut app = waiting_app();
        assert_eq!(waiting_panes(&app), ["%3", "%1"]);
        let before = key_state(&app);
        for code in [
            KeyCode::Char('r'),
            KeyCode::Char('h'),
            KeyCode::Char('l'),
            KeyCode::Char('x'),
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Char('o'),
        ] {
            assert_eq!(app.handle_key(code), Effect::None, "{code:?}");
            assert_eq!(key_state(&app), before, "{code:?}");
        }
    }

    #[test]
    fn letters_type_in_search_and_the_palette() {
        let letters = "hjklnNrxoq";
        let typed = |app: &mut App| {
            for character in letters.chars() {
                let modifiers = if character.is_uppercase() {
                    KeyModifiers::SHIFT
                } else {
                    KeyModifiers::NONE
                };
                assert_eq!(
                    app.handle_key(KeyEvent::new(KeyCode::Char(character), modifiers)),
                    Effect::None,
                    "{character}"
                );
            }
        };
        let mut app = waiting_app();
        app.handle_key(KeyCode::Char('f'));
        typed(&mut app);
        assert_eq!(app.search.query, letters);

        let mut app = waiting_app();
        app.handle_key(KeyCode::Char('/'));
        typed(&mut app);
        assert_eq!(app.palette.as_ref().unwrap().query, letters);
    }

    #[test]
    fn n_in_the_block_moves_questions_not_agents() {
        let mut app = waiting_app();
        app.observe_questions(question_queue(&["Q1", "Q2"]));
        app.handle_key(KeyCode::Tab);
        let selected = app.selected.clone();
        assert_eq!(app.questions.view().unwrap().id, "Q1");
        assert_eq!(app.handle_key(KeyCode::Char('n')), Effect::None);
        assert_eq!(app.questions.view().unwrap().id, "Q2");
        assert_eq!(app.selected, selected);
        assert_eq!(app.handle_key(shift_n()), Effect::None);
        assert_eq!(app.questions.view().unwrap().id, "Q1");
        assert_eq!(app.selected, selected);
        assert_eq!(app.focus, Focus::Block);
    }

    #[test]
    fn list_keys_do_nothing_in_the_block() {
        for (mode, app) in apps_in_every_mode() {
            if !matches!(mode, "block" | "summary") {
                continue;
            }
            for character in ['f', '/', 'g', 'G', 'c', 't', 'q'] {
                let mut app = app.clone();
                app.observe_call(Some(call_view(CallState::Speaking, "%3")));
                select_pane(&mut app, "%3");
                let before = key_state(&app);
                assert_eq!(
                    app.handle_key(KeyCode::Char(character)),
                    Effect::None,
                    "{mode} {character}"
                );
                assert_eq!(key_state(&app), before, "{mode} {character}");
            }
        }
    }

    #[test]
    fn tab_does_nothing_in_search_and_the_palette() {
        for (mode, app) in apps_in_every_mode() {
            if !matches!(mode, "search" | "palette") {
                continue;
            }
            let mut app = app.clone();
            let before = key_state(&app);
            assert_eq!(app.handle_key(KeyCode::Tab), Effect::None, "{mode}");
            assert_eq!(key_state(&app), before, "{mode}");
        }
    }

    #[test]
    fn global_keys_are_the_quit_keys_and_ctrl_l() {
        for character in ['q', 'c', 'l'] {
            assert!(is_global_key(&ctrl(character)), "{character}");
            assert!(is_global_key(&KeyEvent::new(
                KeyCode::Char(character),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            )));
            assert!(!is_global_key(&KeyEvent::from(KeyCode::Char(character))));
            assert!(!is_global_key(&KeyEvent::new(
                KeyCode::Char(character),
                KeyModifiers::ALT
            )));
        }
        assert!(!is_global_key(&ctrl('r')));
    }

    #[test]
    fn global_keys_never_want_fresh_questions() {
        for (mode, app) in apps_in_every_mode() {
            for character in ['q', 'c', 'l'] {
                assert!(!app.wants_fresh_questions(&ctrl(character)), "{mode}");
            }
        }
    }

    #[test]
    fn select_first_row_selects_the_first_visible_row() {
        let mut app = waiting_app();
        app.select_first_row();
        assert_eq!(app.selected.as_ref().unwrap().pane_id, "%1");
        type_text(&mut app, "fvoice");
        app.handle_key(KeyCode::Enter);
        app.selected = None;
        app.select_first_row();
        assert_eq!(app.selected.as_ref().unwrap().pane_id, "%2");
    }
}
