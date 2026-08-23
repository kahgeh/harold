use crate::text::normalize_search;

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
pub struct App {
    pub connection: ConnectionState,
    pub snapshot: Snapshot,
    pub search: SearchState,
    pub selected: Option<AgentIncarnation>,
}

impl App {
    pub fn new(
        connection: ConnectionState,
        snapshot: Snapshot,
        search: SearchState,
        selected: Option<AgentIncarnation>,
    ) -> Self {
        Self {
            connection,
            snapshot,
            search,
            selected,
        }
    }

    pub fn visible_rows(&self) -> Vec<&AgentRow> {
        let query = normalize_search(&self.search.query);
        self.snapshot
            .rows
            .iter()
            .filter(|row| query.is_empty() || row_matches(row, &query))
            .collect()
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

    pub fn degraded_health(&self) -> impl Iterator<Item = &MonitorHealth> {
        self.snapshot
            .monitor_health
            .iter()
            .filter(|health| health.state == MonitorHealthState::Degraded)
    }
}

fn row_matches(row: &AgentRow, normalized_query: &str) -> bool {
    [
        row.provider_display_name.as_str(),
        row.work_summary.as_deref().unwrap_or_default(),
        row.tmux_target.as_str(),
        row.working_directory.as_str(),
    ]
    .into_iter()
    .any(|field| normalize_search(field).contains(normalized_query))
}
