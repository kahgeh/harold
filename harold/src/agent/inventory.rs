use std::collections::{HashMap, HashSet};
use std::io;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use time::{Date, Month, PrimitiveDateTime, Time};

use crate::settings::{AgentProviderSettings, AgentSettings};

use super::domain::{
    AgentIncarnation, AgentPaneObservation, UNKNOWN_PROVIDER_DISPLAY_NAME, UNKNOWN_PROVIDER_ID,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ProcessInfo {
    pub(super) pid: u32,
    pub(super) ppid: u32,
    pub(super) pgid: i32,
    pub(super) tpgid: i32,
    pub(super) tty: String,
    pub(super) started_at_ms: Option<i64>,
    pub(super) command: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct TmuxPaneInfo {
    pub(super) pane_id: String,
    pub(super) session_name: String,
    pub(super) window_index: u32,
    pub(super) pane_index: u32,
    pub(super) pane_pid: u32,
    pub(super) tty: String,
    pub(super) working_directory: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InventoryError {
    CommandUnavailable,
    CommandFailed,
    MalformedOutput,
    MissingProcessStartTime,
}

pub(crate) trait AgentInventoryPort: Send + Sync {
    fn scan(&self) -> Result<Vec<AgentPaneObservation>, InventoryError>;
    fn resolve(&self, pane_id: &str) -> Result<Option<AgentPaneObservation>, InventoryError>;
    #[allow(
        dead_code,
        reason = "exact-incarnation revalidation is consumed by the monitor runtime slice"
    )]
    fn is_current(&self, incarnation: &AgentIncarnation) -> Result<bool, InventoryError>;
}

pub(crate) struct TmuxAgentInventory {
    settings: AgentSettings,
}

impl TmuxAgentInventory {
    pub(crate) fn new(settings: AgentSettings) -> Self {
        Self { settings }
    }
}

impl AgentInventoryPort for TmuxAgentInventory {
    fn scan(&self) -> Result<Vec<AgentPaneObservation>, InventoryError> {
        let processes = read_process_table()?;
        let panes = read_tmux_panes()?;
        let observed_at_ms = now_ms()?;
        panes
            .iter()
            .filter_map(|pane| {
                observe_pane(pane, &processes, &self.settings, observed_at_ms).transpose()
            })
            .collect()
    }

    fn resolve(&self, pane_id: &str) -> Result<Option<AgentPaneObservation>, InventoryError> {
        Ok(self
            .scan()?
            .into_iter()
            .find(|pane| pane.incarnation.pane_id == pane_id))
    }

    fn is_current(&self, incarnation: &AgentIncarnation) -> Result<bool, InventoryError> {
        Ok(self
            .resolve(&incarnation.pane_id)?
            .is_some_and(|pane| pane.incarnation == *incarnation))
    }
}

pub(super) fn observe_pane(
    pane: &TmuxPaneInfo,
    processes: &[ProcessInfo],
    settings: &AgentSettings,
    observed_at_ms: i64,
) -> Result<Option<AgentPaneObservation>, InventoryError> {
    let by_pid: HashMap<u32, &ProcessInfo> = processes
        .iter()
        .map(|process| (process.pid, process))
        .collect();
    let foreground_pgid = by_pid
        .get(&pane.pane_pid)
        .map(|process| process.tpgid)
        .filter(|pgid| *pgid > 0);

    let selected = processes
        .iter()
        .filter(|process| settings.matches_command(&process.command))
        .filter_map(|process| {
            descendant_depth(process.pid, pane.pane_pid, &by_pid).map(|depth| {
                let foreground = foreground_pgid.is_some_and(|pgid| {
                    normalize_tty(&process.tty) == normalize_tty(&pane.tty) && process.pgid == pgid
                });
                (process, foreground, depth)
            })
        })
        .min_by_key(|(process, foreground, depth)| (!*foreground, *depth, process.pid));

    let Some((process, _, _)) = selected else {
        return Ok(None);
    };
    let Some(agent_started_at_ms) = process.started_at_ms else {
        return Err(InventoryError::MissingProcessStartTime);
    };
    let (provider_id, provider_display_name) = resolve_provider(settings, &process.command);

    Ok(Some(AgentPaneObservation {
        incarnation: AgentIncarnation {
            pane_id: pane.pane_id.clone(),
            pane_pid: pane.pane_pid,
            agent_pid: process.pid,
            agent_started_at_ms,
            provider_id,
        },
        tmux_target: format!(
            "{}:{}.{}",
            pane.session_name, pane.window_index, pane.pane_index
        ),
        session_name: pane.session_name.clone(),
        window_index: pane.window_index,
        pane_index: pane.pane_index,
        working_directory: pane.working_directory.clone(),
        provider_display_name,
        observed_at_ms,
    }))
}

fn descendant_depth(
    pid: u32,
    pane_pid: u32,
    processes: &HashMap<u32, &ProcessInfo>,
) -> Option<usize> {
    if pid == pane_pid {
        return Some(0);
    }
    let mut current = pid;
    let mut depth = 0;
    let mut visited = HashSet::new();
    while visited.insert(current) {
        let process = processes.get(&current)?;
        depth += 1;
        if process.ppid == pane_pid {
            return Some(depth);
        }
        current = process.ppid;
    }
    None
}

fn resolve_provider(settings: &AgentSettings, command: &str) -> (String, String) {
    let AgentSettings::Named(providers) = settings else {
        return unknown_provider();
    };
    let matching: Vec<&AgentProviderSettings> = providers
        .iter()
        .filter(|provider| command_matches_any(command, &provider.command_contains))
        .collect();
    match matching.as_slice() {
        [provider] => (provider.id.clone(), provider.display_name.clone()),
        _ => unknown_provider(),
    }
}

fn unknown_provider() -> (String, String) {
    (
        UNKNOWN_PROVIDER_ID.to_string(),
        UNKNOWN_PROVIDER_DISPLAY_NAME.to_string(),
    )
}

fn command_matches_any(command: &str, fragments: &[String]) -> bool {
    let command = command.to_lowercase();
    fragments.iter().any(|fragment| {
        let fragment = fragment.trim().to_lowercase();
        !fragment.is_empty() && command.contains(&fragment)
    })
}

fn normalize_tty(tty: &str) -> &str {
    tty.strip_prefix("/dev/").unwrap_or(tty)
}

pub(super) fn parse_process_table(output: &str) -> Result<Vec<ProcessInfo>, InventoryError> {
    output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 11 {
                return Err(InventoryError::MalformedOutput);
            }
            Ok(ProcessInfo {
                pid: parse_field(fields[0])?,
                ppid: parse_field(fields[1])?,
                pgid: parse_field(fields[2])?,
                tpgid: parse_field(fields[3])?,
                tty: fields[4].to_string(),
                started_at_ms: parse_start_time(fields[6], fields[7], fields[8], fields[9]),
                command: fields[10..].join(" "),
            })
        })
        .collect()
}

pub(super) fn parse_tmux_panes(output: &str) -> Result<Vec<TmuxPaneInfo>, InventoryError> {
    output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let fields: Vec<&str> = line.split('\u{1f}').collect();
            if fields.len() != 7 {
                return Err(InventoryError::MalformedOutput);
            }
            Ok(TmuxPaneInfo {
                pane_id: fields[0].to_string(),
                session_name: fields[1].to_string(),
                window_index: parse_field(fields[2])?,
                pane_index: parse_field(fields[3])?,
                pane_pid: parse_field(fields[4])?,
                tty: normalize_tty(fields[5]).to_string(),
                working_directory: fields[6].to_string(),
            })
        })
        .collect()
}

fn parse_field<T: std::str::FromStr>(field: &str) -> Result<T, InventoryError> {
    field.parse().map_err(|_| InventoryError::MalformedOutput)
}

fn parse_start_time(month: &str, day: &str, time: &str, year: &str) -> Option<i64> {
    let month = match month {
        "Jan" => Month::January,
        "Feb" => Month::February,
        "Mar" => Month::March,
        "Apr" => Month::April,
        "May" => Month::May,
        "Jun" => Month::June,
        "Jul" => Month::July,
        "Aug" => Month::August,
        "Sep" => Month::September,
        "Oct" => Month::October,
        "Nov" => Month::November,
        "Dec" => Month::December,
        _ => return None,
    };
    let mut clock = time.split(':');
    let hour = clock.next()?.parse().ok()?;
    let minute = clock.next()?.parse().ok()?;
    let second = clock.next()?.parse().ok()?;
    if clock.next().is_some() {
        return None;
    }
    let date = Date::from_calendar_date(year.parse().ok()?, month, day.parse().ok()?).ok()?;
    let time = Time::from_hms(hour, minute, second).ok()?;
    Some(
        PrimitiveDateTime::new(date, time)
            .assume_utc()
            .unix_timestamp()
            * 1_000,
    )
}

fn read_process_table() -> Result<Vec<ProcessInfo>, InventoryError> {
    let output = Command::new("ps")
        .env("LC_ALL", "C")
        .env("TZ", "UTC")
        .args(["-axo", "pid=,ppid=,pgid=,tpgid=,tty=,lstart=,command="])
        .output()
        .map_err(command_error)?;
    if !output.status.success() {
        return Err(InventoryError::CommandFailed);
    }
    let stdout = String::from_utf8(output.stdout).map_err(|_| InventoryError::MalformedOutput)?;
    parse_process_table(&stdout)
}

fn read_tmux_panes() -> Result<Vec<TmuxPaneInfo>, InventoryError> {
    let output = Command::new("tmux")
        .args([
            "list-panes",
            "-a",
            "-F",
            "#{pane_id}\x1f#{session_name}\x1f#{window_index}\x1f#{pane_index}\x1f#{pane_pid}\x1f#{pane_tty}\x1f#{pane_current_path}",
        ])
        .output()
        .map_err(command_error)?;
    if !output.status.success() {
        return Err(InventoryError::CommandFailed);
    }
    let stdout = String::from_utf8(output.stdout).map_err(|_| InventoryError::MalformedOutput)?;
    parse_tmux_panes(&stdout)
}

fn command_error(error: io::Error) -> InventoryError {
    if error.kind() == io::ErrorKind::NotFound {
        InventoryError::CommandUnavailable
    } else {
        InventoryError::CommandFailed
    }
}

fn now_ms() -> Result<i64, InventoryError> {
    let milliseconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| InventoryError::CommandFailed)?
        .as_millis();
    i64::try_from(milliseconds).map_err(|_| InventoryError::CommandFailed)
}
