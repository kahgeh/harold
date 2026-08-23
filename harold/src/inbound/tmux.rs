use std::process::Command;

use tracing::info;

use crate::settings::{AgentSettings, get_settings};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProcessInfo {
    pub(crate) pid: u32,
    pub(crate) ppid: u32,
    pub(crate) command: String,
}

// ---------------------------------------------------------------------------
// Process detection
// ---------------------------------------------------------------------------

fn command_matches_agent(command: &str, settings: &AgentSettings) -> bool {
    let command = command.trim().to_lowercase();
    settings.command_contains.iter().any(|needle| {
        let needle = needle.trim().to_lowercase();
        !needle.is_empty() && command.contains(&needle)
    })
}

fn process_tree_contains_agent(
    pane_pid: u32,
    processes: &[ProcessInfo],
    settings: &AgentSettings,
) -> bool {
    let mut pending = vec![pane_pid];

    while let Some(pid) = pending.pop() {
        for process in processes.iter().filter(|process| process.ppid == pid) {
            if command_matches_agent(&process.command, settings) {
                return true;
            }
            pending.push(process.pid);
        }

        if let Some(process) = processes.iter().find(|process| process.pid == pid)
            && command_matches_agent(&process.command, settings)
        {
            return true;
        }
    }

    false
}

fn read_process_table() -> Vec<ProcessInfo> {
    let out = match Command::new("ps")
        .args(["-axo", "pid=,ppid=,comm="])
        .output()
    {
        Ok(o) => o,
        Err(_) => return vec![],
    };

    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let pid = parts.next()?.parse().ok()?;
            let ppid = parts.next()?.parse().ok()?;
            let command = parts.next()?.to_string();
            Some(ProcessInfo { pid, ppid, command })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Live pane discovery
// ---------------------------------------------------------------------------

pub(crate) fn scan_live_panes() -> Vec<super::directory::AgentAddress> {
    let settings = get_settings();
    let processes = read_process_table();
    let out = match Command::new("tmux")
        .args([
            "list-panes",
            "-a",
            "-F",
            "#{pane_id}|#{session_name}:#{window_index}.#{pane_index}|#{pane_pid}",
        ])
        .output()
    {
        Ok(o) => o,
        Err(_) => return vec![],
    };

    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.splitn(3, '|').collect();
            if parts.len() != 3 {
                return None;
            }
            let pane_pid = parts[2].parse().ok()?;
            let pane_id = parts[0].to_string();
            let label = parts[1]
                .chars()
                .filter(|c| c.is_ascii_graphic() || *c == ' ')
                .collect::<String>()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            if process_tree_contains_agent(pane_pid, &processes, &settings.agents) {
                Some(super::directory::AgentAddress::TmuxPane { pane_id, label })
            } else {
                None
            }
        })
        .collect()
}

pub(crate) fn is_pane_alive(pane_id: &str) -> bool {
    let pane_pid = match Command::new("tmux")
        .args(["display-message", "-t", pane_id, "-p", "#{pane_pid}"])
        .output()
    {
        Ok(o) => String::from_utf8_lossy(&o.stdout).trim().parse().ok(),
        Err(_) => None,
    };

    pane_pid.is_some_and(|pane_pid| {
        process_tree_contains_agent(pane_pid, &read_process_table(), &get_settings().agents)
    })
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
