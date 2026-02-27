use std::process::Command;

use tracing::info;

// ---------------------------------------------------------------------------
// Process detection
// ---------------------------------------------------------------------------

/// Matches process names that are purely digits separated by dots (semver-like),
/// e.g. "16.20.1" — the node version that Claude Code runs as.
pub(crate) fn node_semver_process(cmd: &str) -> bool {
    let parts: Vec<&str> = cmd.split('.').collect();
    parts.len() >= 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

// ---------------------------------------------------------------------------
// Live pane discovery
// ---------------------------------------------------------------------------

pub(crate) fn scan_live_panes() -> Vec<super::directory::AgentAddress> {
    let out = match Command::new("tmux")
        .args([
            "list-panes",
            "-a",
            "-F",
            "#{pane_id}|#{session_name}:#{window_index}.#{pane_index}|#{pane_current_command}",
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
            let pane_id = parts[0].to_string();
            let label = parts[1]
                .chars()
                .filter(|c| c.is_ascii_graphic() || *c == ' ')
                .collect::<String>()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            if node_semver_process(parts[2].trim()) {
                Some(super::directory::AgentAddress::TmuxPane { pane_id, label })
            } else {
                None
            }
        })
        .collect()
}

pub(crate) fn is_pane_alive(pane_id: &str) -> bool {
    Command::new("tmux")
        .args([
            "display-message",
            "-t",
            pane_id,
            "-p",
            "#{pane_current_command}",
        ])
        .output()
        .is_ok_and(|o| node_semver_process(String::from_utf8_lossy(&o.stdout).trim()))
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

pub(crate) fn relay_to_tmux_pane(pane_id: &str, text: &str) {
    info!(pane_id, text, "relay_to_tmux_pane");
    let safe = strip_control(text);
    let _ = Command::new("tmux")
        .args(["send-keys", "-t", pane_id, "-l", &safe])
        .status();
    let _ = Command::new("tmux")
        .args(["send-keys", "-t", pane_id, "Enter"])
        .status();
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_control_removes_ansi_and_controls() {
        let input = "\x1b[31mred\x1b[0m normal\x01hidden";
        let output = strip_control(input);
        assert_eq!(output, "red normalhidden");
    }

    #[test]
    fn strip_control_removes_lone_control_chars() {
        let input = "clean\x01";
        let output = strip_control(input);
        assert_eq!(output, "clean");
    }

    #[test]
    fn node_semver_process_matches_node_version() {
        assert!(node_semver_process("16.20.1"));
        assert!(node_semver_process("20.11.0"));
        assert!(!node_semver_process("python3.11"));
        assert!(!node_semver_process("bash"));
        assert!(!node_semver_process("node"));
    }
}
