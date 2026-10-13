use std::process::Command;

use tracing::{info, warn};

use crate::agent::domain::AgentPaneObservation;
use crate::agent::inventory::{AgentInventoryPort, TmuxAgentInventory};
use crate::agent::summary::sanitize_bounded_metadata;
use crate::settings::get_settings;

const INBOUND_PANE_LABEL_MAX_SCALARS: usize = 256;

// ---------------------------------------------------------------------------
// Live pane discovery
// ---------------------------------------------------------------------------

pub(crate) fn scan_live_panes() -> Vec<super::directory::AgentAddress> {
    let inventory = TmuxAgentInventory::new(get_settings().agents.clone());
    match inventory.scan() {
        Ok(observations) => observations
            .into_iter()
            .map(observation_to_address)
            .collect(),
        Err(error) => {
            warn!(?error, "agent inventory scan failed");
            Vec::new()
        }
    }
}

pub(crate) fn is_pane_alive(pane_id: &str) -> bool {
    let inventory = TmuxAgentInventory::new(get_settings().agents.clone());
    match inventory.resolve(pane_id) {
        Ok(observation) => observation.is_some(),
        Err(error) => {
            warn!(?error, pane_id, "agent inventory resolve failed");
            false
        }
    }
}

/// The label Harold shows for a pane and matches a reply's tag against:
/// `<session_name>:<window_index>.<pane_index>`.
pub(crate) fn pane_label(observation: &AgentPaneObservation) -> String {
    sanitize_bounded_metadata(&observation.tmux_target, INBOUND_PANE_LABEL_MAX_SCALARS)
}

fn observation_to_address(observation: AgentPaneObservation) -> super::directory::AgentAddress {
    super::directory::AgentAddress::TmuxPane {
        label: pane_label(&observation),
        pane_id: observation.incarnation.pane_id,
    }
}

// ---------------------------------------------------------------------------
// Control character stripping
// ---------------------------------------------------------------------------

pub(crate) fn strip_control(text: &str) -> String {
    // Remove ANSI escape sequences and control characters before sending to tmux.
    // The '-l' flag prevents shell interpretation but raw bytes still reach the pane.
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for c2 in chars.by_ref() {
                    if c2.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else if c.is_control() && c != '\n' {
            // drop
        } else {
            out.push(c);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// tmux relay
// ---------------------------------------------------------------------------

const TMUX: &str = "tmux";

fn literal_text_args<'a>(pane_id: &'a str, text: &'a str) -> [&'a str; 5] {
    ["send-keys", "-t", pane_id, "-l", text]
}

fn enter_args(pane_id: &str) -> [&str; 4] {
    ["send-keys", "-t", pane_id, "Enter"]
}

fn type_text_with(program: &str, pane_id: &str, text: &str) -> Result<(), String> {
    let safe = strip_control(text);
    let status = Command::new(program)
        .args(literal_text_args(pane_id, &safe))
        .status()
        .map_err(|error| format!("failed to start tmux send-keys for {pane_id}: {error}"))?;
    if !status.success() {
        return Err(format!(
            "tmux send-keys text failed for {pane_id}: {status}"
        ));
    }
    Ok(())
}

fn press_enter_with(program: &str, pane_id: &str) -> Result<(), String> {
    let status = Command::new(program)
        .args(enter_args(pane_id))
        .status()
        .map_err(|error| format!("failed to start tmux Enter for {pane_id}: {error}"))?;
    if !status.success() {
        return Err(format!(
            "tmux send-keys Enter failed for {pane_id}: {status}"
        ));
    }
    Ok(())
}

/// Types `text` into the pane as literal keys, without Enter. Writes no log line: the
/// caller decides whether the text may be logged.
pub(crate) fn type_text_into_tmux_pane(pane_id: &str, text: &str) -> Result<(), String> {
    type_text_with(TMUX, pane_id, text)
}

/// Sends Enter to the pane.
pub(crate) fn press_enter_in_tmux_pane(pane_id: &str) -> Result<(), String> {
    press_enter_with(TMUX, pane_id)
}

pub(crate) fn relay_to_tmux_pane(pane_id: &str, text: &str) -> Result<(), String> {
    info!(pane_id, text, "relay_to_tmux_pane");
    type_text_into_tmux_pane(pane_id, text)?;
    press_enter_in_tmux_pane(pane_id)
}

#[cfg(test)]
#[path = "tmux_tests.rs"]
mod tests;
