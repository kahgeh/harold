use std::process::Command;

use tracing::{info, warn};

use crate::agent::domain::AgentPaneObservation;
use crate::agent::inventory::{AgentInventoryPort, TmuxAgentInventory};
use crate::settings::get_settings;

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

fn observation_to_address(observation: AgentPaneObservation) -> super::directory::AgentAddress {
    super::directory::AgentAddress::TmuxPane {
        pane_id: observation.incarnation.pane_id,
        label: observation.tmux_target,
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

pub(crate) fn relay_to_tmux_pane(pane_id: &str, text: &str) -> Result<(), String> {
    info!(pane_id, text, "relay_to_tmux_pane");
    let safe = strip_control(text);
    let literal_status = Command::new("tmux")
        .args(["send-keys", "-t", pane_id, "-l", &safe])
        .status()
        .map_err(|error| format!("failed to start tmux send-keys for {pane_id}: {error}"))?;
    if !literal_status.success() {
        return Err(format!(
            "tmux send-keys text failed for {pane_id}: {literal_status}"
        ));
    }

    let enter_status = Command::new("tmux")
        .args(["send-keys", "-t", pane_id, "Enter"])
        .status()
        .map_err(|error| format!("failed to start tmux Enter for {pane_id}: {error}"))?;
    if !enter_status.success() {
        return Err(format!(
            "tmux send-keys Enter failed for {pane_id}: {enter_status}"
        ));
    }

    Ok(())
}

#[cfg(test)]
#[path = "tmux_tests.rs"]
mod tests;
