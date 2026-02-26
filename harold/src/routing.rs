use std::process::Command;
use std::sync::Mutex;

use tracing::info;

use crate::settings::get_settings;
use crate::util::{ai_cli_env, sanitise_for_applescript};

// ---------------------------------------------------------------------------
// State — last_notified_pane (in-memory; Harold owns all routing state)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct PaneInfo {
    pub pane_id: String,
    pub label: String,
}

static LAST_NOTIFIED_PANE: Mutex<Option<PaneInfo>> = Mutex::new(None);

pub fn set_last_notified_pane(pane: PaneInfo) {
    *LAST_NOTIFIED_PANE.lock().unwrap() = Some(pane);
}

fn get_last_notified_pane() -> Option<PaneInfo> {
    LAST_NOTIFIED_PANE.lock().unwrap().clone()
}

// ---------------------------------------------------------------------------
// Tag parsing
// ---------------------------------------------------------------------------

pub(crate) fn parse_tag(text: &str) -> (Option<&str>, &str) {
    if let Some(rest) = text.strip_prefix('[')
        && let Some(end) = rest.find(']')
    {
        let tag = &rest[..end];
        let body = rest[end + 1..].trim();
        return (Some(tag), body);
    }
    (None, text)
}

// ---------------------------------------------------------------------------
// Live pane discovery
// ---------------------------------------------------------------------------

pub(crate) fn is_claude_code_process(cmd: &str) -> bool {
    // Claude Code runs as a node process named like "16.20.1" (the node version).
    // We match process names that are purely digits separated by dots (semver-like).
    // TODO: replace with explicit pane registration via the TurnComplete RPC.
    let parts: Vec<&str> = cmd.split('.').collect();
    parts.len() >= 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

fn live_claude_panes() -> Vec<PaneInfo> {
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
            if is_claude_code_process(parts[2].trim()) {
                Some(PaneInfo { pane_id, label })
            } else {
                None
            }
        })
        .collect()
}

fn is_pane_alive(pane_id: &str) -> bool {
    Command::new("tmux")
        .args([
            "display-message",
            "-t",
            pane_id,
            "-p",
            "#{pane_current_command}",
        ])
        .output()
        .is_ok_and(|o| is_claude_code_process(String::from_utf8_lossy(&o.stdout).trim()))
}

// ---------------------------------------------------------------------------
// Semantic routing via AI CLI
// ---------------------------------------------------------------------------

fn semantic_resolve(body: &str, panes: &[PaneInfo]) -> Option<(usize, String)> {
    if panes.len() <= 1 {
        return None;
    }
    let cfg = get_settings();
    let cli = cfg.ai.cli_path.as_deref()?;

    let labels_list = panes
        .iter()
        .map(|p| format!("- {}", p.label))
        .collect::<Vec<_>>()
        .join("\n");

    let prompt = format!(
        "Given this message: \"{body}\"\n\n\
         And these active tmux panes:\n{labels_list}\n\n\
         Does the message contain EXPLICIT routing intent to a specific pane? \
         (direct address like 'To X,', 'ask X', '[X]' — NOT just thematic association)\n\
         If yes, reply on two lines:\n\
         LINE1: exact pane label\n\
         LINE2: message with routing prefix removed\n\
         If no explicit routing intent, reply: none"
    );

    let out = Command::new(cli)
        .args([
            "-p",
            &prompt,
            "--model",
            "haiku",
            "--max-turns",
            "1",
            "--settings",
            r#"{"disableAllHooks":true}"#,
        ])
        .envs(ai_cli_env())
        .output()
        .ok()?;

    if !out.status.success() {
        return None;
    }

    let output = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if output.to_lowercase() == "none" || output.is_empty() {
        return None;
    }

    let mut lines = output.lines();
    let answer = lines
        .next()?
        .trim()
        .trim_start_matches("LINE1:")
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .to_string();
    let cleaned = lines
        .next()
        .map(|l| l.trim().trim_start_matches("LINE2:").trim().to_string())
        .unwrap_or_else(|| body.to_string());

    let idx = panes.iter().position(|p| {
        p.label == answer
            || answer.to_lowercase().contains(&p.label.to_lowercase())
            || p.label.to_lowercase().contains(&answer.to_lowercase())
    })?;

    Some((idx, cleaned))
}

// ---------------------------------------------------------------------------
// Pane resolution
// ---------------------------------------------------------------------------

pub(crate) fn resolve_pane<'a>(
    tag: Option<&str>,
    body: &str,
    panes: &'a [PaneInfo],
) -> Option<(&'a PaneInfo, String)> {
    if let Some(tag) = tag {
        if let Some(p) = panes.iter().find(|p| p.label == tag) {
            return Some((p, body.to_string()));
        }
        let tag_lc = tag.to_lowercase();
        return panes
            .iter()
            .find(|p| p.label.to_lowercase().contains(&tag_lc))
            .map(|p| (p, body.to_string()));
    }

    if let Some((idx, cleaned)) = semantic_resolve(body, panes) {
        return Some((&panes[idx], cleaned));
    }

    if let Some(last) = get_last_notified_pane()
        && let Some(p) = panes.iter().find(|p| p.pane_id == last.pane_id)
    {
        return Some((p, body.to_string()));
    }

    panes
        .iter()
        .find(|p| p.label.to_lowercase().contains("my-agent"))
        .map(|p| (p, body.to_string()))
}

// ---------------------------------------------------------------------------
// tmux relay
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

fn relay_to_pane(pane_id: &str, text: &str) {
    let safe = strip_control(text);
    let _ = Command::new("tmux")
        .args(["send-keys", "-t", pane_id, "-l", &safe])
        .status();
    let _ = Command::new("tmux")
        .args(["send-keys", "-t", pane_id, "Enter"])
        .status();
}

// ---------------------------------------------------------------------------
// iMessage send (for error / confirmation replies)
// ---------------------------------------------------------------------------

pub(crate) fn send_imessage(msg: &str) {
    let cfg = get_settings();
    let Some(recipient) = cfg.imessage.recipient.as_deref() else {
        return;
    };
    let escaped_msg = sanitise_for_applescript(msg)
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    let escaped_rec = sanitise_for_applescript(recipient)
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    let script = format!(
        "tell application \"Messages\" to send \"{escaped_msg}\" to buddy \"{escaped_rec}\""
    );
    let _ = Command::new("osascript").args(["-e", &script]).status();
}

// ---------------------------------------------------------------------------
// Route a received reply — called from projector
// ---------------------------------------------------------------------------

pub fn route_reply(text: &str) {
    let (tag, body) = parse_tag(text);
    let panes = live_claude_panes();

    if panes.is_empty() {
        send_imessage("No Claude Code sessions active.");
        return;
    }

    match resolve_pane(tag, body, &panes) {
        None => {
            let available = panes
                .iter()
                .map(|p| p.label.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            let msg = match tag {
                Some(t) => format!("No pane matching '{t}'. Available: {available}"),
                None => format!("No active pane found. Available: {available}"),
            };
            send_imessage(&msg);
        }
        Some((pane, cleaned_body)) => {
            if !is_pane_alive(&pane.pane_id) {
                let available = panes
                    .iter()
                    .filter(|p| p.pane_id != pane.pane_id)
                    .map(|p| p.label.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                send_imessage(&format!(
                    "Pane {} is no longer active. Available: {}",
                    pane.label, available
                ));
                return;
            }
            info!(pane_id = %pane.pane_id, label = %pane.label, "routing reply");
            relay_to_pane(&pane.pane_id, &format!("📱 {cleaned_body}"));
            send_imessage(&format!("✓ Delivered to [{}]", pane.label));
        }
    }
}

#[cfg(test)]
#[path = "routing_test.rs"]
mod tests;
