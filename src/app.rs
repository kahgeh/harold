use crate::text::normalize_search;
use crossterm::event::KeyCode;
use std::collections::HashSet;
use std::fmt;

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
pub(crate) enum RuntimeStatus {
    Retrying {
        endpoint: String,
        detail: String,
        delay_ms: u64,
    },
    NavigationUnavailable,
    NavigationFailed(String),
    SourceError(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct App {
    pub connection: ConnectionState,
    pub snapshot: Snapshot,
    pub search: SearchState,
    pub selected: Option<AgentIncarnation>,
    last_snapshot_received_at_ms: Option<i64>,
    runtime_status: Option<RuntimeStatus>,
    has_snapshot: bool,
    normalized_query: String,
    searchable_rows: Vec<SearchableRow>,
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
        Self {
            connection,
            snapshot,
            search,
            selected,
            last_snapshot_received_at_ms: None,
            runtime_status: None,
            has_snapshot: matches!(connection, ConnectionState::Live | ConnectionState::Stale),
            normalized_query,
            searchable_rows,
        }
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

    pub fn degraded_health(&self) -> impl Iterator<Item = &MonitorHealth> {
        self.snapshot
            .monitor_health
            .iter()
            .filter(|health| health.state == MonitorHealthState::Degraded)
    }

    pub fn handle_key(&mut self, key: KeyCode) -> Effect {
        if self.search.editing {
            return self.handle_search_key(key);
        }

        match key {
            KeyCode::Char('/') => self.search.editing = true,
            KeyCode::Char('q') | KeyCode::Esc => return Effect::Quit,
            KeyCode::Char('r') => return Effect::Retry,
            KeyCode::Char('j') | KeyCode::Down => self.select_relative(1),
            KeyCode::Char('k') | KeyCode::Up => self.select_relative(-1),
            KeyCode::Char('g') => self.select_boundary(false),
            KeyCode::Char('G') => self.select_boundary(true),
            KeyCode::Enter => {
                return self
                    .selected_row()
                    .map_or(Effect::None, |row| Effect::Navigate {
                        pane_id: row.incarnation.pane_id.clone(),
                    });
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
        self.searchable_rows = self.snapshot.rows.iter().map(SearchableRow::from).collect();
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
        state_rank(left.state)
            .cmp(&state_rank(right.state))
            .then_with(|| left.session_name.cmp(&right.session_name))
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

const fn state_rank(state: AgentState) -> u8 {
    match state {
        AgentState::Busy => 0,
        AgentState::Idle => 1,
        AgentState::Unknown => 2,
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyCode;

    use super::*;

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
            vec!["%2", "%3", "%4", "%1", "%5"]
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

        assert_eq!(app.handle_key(KeyCode::Char('/')), Effect::None);
        assert!(app.search.editing);
        assert_eq!(app.handle_key(KeyCode::Char('q')), Effect::None);
        assert_eq!(app.handle_key(KeyCode::Char('界')), Effect::None);
        assert_eq!(app.search.query, "q界");

        assert_eq!(app.handle_key(KeyCode::Backspace), Effect::None);
        assert_eq!(app.search.query, "q");
        assert_eq!(app.handle_key(KeyCode::Enter), Effect::None);
        assert!(!app.search.editing);
        assert_eq!(app.search.query, "q");

        app.handle_key(KeyCode::Char('/'));
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
        assert_eq!(app.handle_key(KeyCode::Char('r')), Effect::Retry);
        assert_eq!(app.handle_key(KeyCode::Char('q')), Effect::Quit);
        assert_eq!(app.handle_key(KeyCode::Esc), Effect::Quit);

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
        let mut later = row(later_match, AgentState::Unknown, "three", 0, 0);
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

        app.handle_key(KeyCode::Char('/'));
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
            through_event_version,
            server_time_ms,
            monitor_health: Vec::new(),
            rows,
        }
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
}
