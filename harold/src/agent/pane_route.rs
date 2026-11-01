//! Pane id shapes and the choice of writer for a pane id.
//!
//! A pane id from another Rex host has the form `rex:<label>/block:<id>`.
//! No writer accepts that form, so a host pane id never reaches a terminal.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock, PoisonError};

use super::rex::valid_rex_pane_id;
use super::runtime::valid_tmux_pane_id;

const HOST_PREFIX: &str = "rex:";
const MAX_LABEL_LEN: usize = 32;
/// The most pane id and writer pairs that `refuse_write` remembers for its log.
const MAX_LOGGED_REFUSALS: usize = 256;

/// Whether a host label is 1 to 32 characters of letters, digits, `.` or `-`.
pub(crate) fn valid_host_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= MAX_LABEL_LEN
        && label
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
}

/// Builds the pane id of a block on another Rex host. Only the tests call it until Part B.
#[cfg_attr(not(test), allow(dead_code))]
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

/// Logs a refused write once for each pane id and writer. Returns the error text.
pub(crate) fn refuse_write(pane_id: &str, writer: &'static str, refusal: PaneRefusal) -> String {
    static SEEN: OnceLock<Mutex<HashSet<(String, &'static str)>>> = OnceLock::new();
    let seen = SEEN.get_or_init(|| Mutex::new(HashSet::new()));
    let first = {
        let mut seen = seen.lock().unwrap_or_else(PoisonError::into_inner);
        seen.len() < MAX_LOGGED_REFUSALS && seen.insert((pane_id.to_string(), writer))
    };
    if first {
        tracing::warn!(
            pane_id,
            writer,
            reason = refusal.reason(),
            "pane write refused"
        );
    }
    format!(
        "pane write refused: {writer} cannot write to {pane_id} ({})",
        refusal.reason()
    )
}
