//! Pane id shapes and the choice of writer for a pane id.
//!
//! A pane id from another Rex host has the form `rex:<label>/block:<id>`.
//! No writer accepts that form, so a host pane id never reaches a terminal.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use super::rex::valid_rex_pane_id;
use super::runtime::valid_tmux_pane_id;

const HOST_PREFIX: &str = "rex:";
const MAX_LABEL_LEN: usize = 32;
/// The most pane id and writer pairs that `refuse_write` remembers for its log.
pub(super) const MAX_LOGGED_REFUSALS: usize = 256;

/// Whether a host label is 1 to 32 characters of letters, digits, `.` or `-`.
pub(crate) fn valid_host_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= MAX_LABEL_LEN
        && label
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
}

/// Builds the pane id of a block on another Rex host.
pub(crate) fn host_pane_id(label: &str, block_id: &str) -> String {
    format!("{HOST_PREFIX}{label}/block:{block_id}")
}

/// Splits a host pane id into the label and the `block:<id>` part.
///
/// Returns `None` unless the label and the block id are both valid.
pub(crate) fn parse_host_pane_id(id: &str) -> Option<(&str, &str)> {
    let rest = id.strip_prefix(HOST_PREFIX)?;
    let (label, block) = rest.split_once('/')?;
    (valid_host_label(label) && valid_rex_pane_id(block)).then_some((label, block))
}

/// The writer that owns a pane id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PaneRoute {
    Tmux,
    Rex,
}

/// Why no writer takes a pane id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PaneRefusal {
    /// The pane belongs to another Rex host.
    OnRexHost,
    /// The pane id has no known form.
    InvalidPaneId,
}

impl PaneRefusal {
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Self::OnRexHost => "on_rex_host",
            Self::InvalidPaneId => "invalid_pane_id",
        }
    }
}

/// Chooses the writer from the shape of the pane id alone.
///
/// It reads no map and no setting, takes no lock and logs nothing.
pub(crate) fn route_pane(pane_id: &str) -> Result<PaneRoute, PaneRefusal> {
    if valid_rex_pane_id(pane_id) {
        Ok(PaneRoute::Rex)
    } else if valid_tmux_pane_id(pane_id) {
        Ok(PaneRoute::Tmux)
    } else if parse_host_pane_id(pane_id).is_some() {
        Err(PaneRefusal::OnRexHost)
    } else {
        Err(PaneRefusal::InvalidPaneId)
    }
}

/// The text that stands for a pane id that has no known form.
const INVALID_PANE_ID_TEXT: &str = "<invalid-pane-id>";
/// The most bytes of a pane id that `refuse_write` logs or keeps.
const MAX_LOGGED_ID_BYTES: usize = 128;
/// The time in seconds between two logs for one writer when the dedupe set is full.
const FULL_SET_LOG_SECONDS: u64 = 60;

/// The text of a refused pane id that is safe to log and to keep.
///
/// An id with no known form stands for itself as a fixed text, so raw caller text never
/// reaches a log. An id on a Rex host has a checked shape, and the cap is a second limit.
pub(super) fn loggable_id(pane_id: &str, refusal: PaneRefusal) -> &str {
    match refusal {
        PaneRefusal::InvalidPaneId => INVALID_PANE_ID_TEXT,
        PaneRefusal::OnRexHost => {
            let mut end = pane_id.len().min(MAX_LOGGED_ID_BYTES);
            while !pane_id.is_char_boundary(end) {
                end -= 1;
            }
            &pane_id[..end]
        }
    }
}

/// Remembers which refusals Harold logged, with a fixed upper size.
#[derive(Default)]
pub(super) struct RefusalLog {
    seen: HashSet<(String, &'static str)>,
    /// For each writer: the second of the last log made while `seen` was full.
    full_logged: HashMap<&'static str, u64>,
}

impl RefusalLog {
    /// Whether to log the refusal of `id` by `writer` at the time `now_secs`.
    ///
    /// Each pair logs once. When `seen` is full, a new pair logs at most once a minute for
    /// each writer, so the line never goes silent.
    pub(super) fn should_log(&mut self, id: &str, writer: &'static str, now_secs: u64) -> bool {
        if self.seen.len() < MAX_LOGGED_REFUSALS {
            return self.seen.insert((id.to_string(), writer));
        }
        if self.seen.contains(&(id.to_string(), writer)) {
            return false;
        }
        let due = self
            .full_logged
            .get(writer)
            .is_none_or(|last| now_secs.saturating_sub(*last) >= FULL_SET_LOG_SECONDS);
        if due {
            self.full_logged.insert(writer, now_secs);
        }
        due
    }
}

/// The one log of refusals for the process.
fn refusals() -> std::sync::MutexGuard<'static, RefusalLog> {
    static REFUSALS: OnceLock<Mutex<RefusalLog>> = OnceLock::new();
    REFUSALS
        .get_or_init(|| Mutex::new(RefusalLog::default()))
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// The pane id texts that the process keeps for the dedupe.
#[cfg(test)]
pub(crate) fn kept_ids() -> Vec<String> {
    refusals().seen.iter().map(|(id, _)| id.clone()).collect()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

/// Logs a refused write once for each pane id and writer. Returns the error text.
///
/// The log and the error text hold no raw caller text: see `loggable_id`.
pub(crate) fn refuse_write(pane_id: &str, writer: &'static str, refusal: PaneRefusal) -> String {
    let id = loggable_id(pane_id, refusal);
    let first = refusals().should_log(id, writer, now_secs());
    if first {
        tracing::warn!(
            pane_id = id,
            writer,
            reason = refusal.reason(),
            "pane write refused"
        );
    }
    format!(
        "pane write refused: {writer} cannot write to {id} ({})",
        refusal.reason()
    )
}
