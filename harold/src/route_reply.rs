use std::process::Command;
use std::sync::Mutex;

use tracing::info;

use crate::settings::get_settings;
use crate::util::{ai_cli_env, sanitise_for_applescript};

// ---------------------------------------------------------------------------
// State — agent routing (in-memory; Harold owns all routing state)
// ---------------------------------------------------------------------------

/// How to reach an agent session. Currently only tmux panes, but extensible.
#[derive(Debug, Clone)]
pub enum AgentAddress {
    TmuxPane { pane_id: String, label: String },
}

impl AgentAddress {
    pub fn label(&self) -> &str {
        match self {
            AgentAddress::TmuxPane { label, .. } => label,
        }
    }

    pub(crate) fn tmux_pane_id(&self) -> &str {
        match self {
            AgentAddress::TmuxPane { pane_id, .. } => pane_id,
        }
    }

    fn same_target(&self, other: &AgentAddress) -> bool {
        match (self, other) {
            (
                AgentAddress::TmuxPane { pane_id: a, .. },
                AgentAddress::TmuxPane { pane_id: b, .. },
            ) => a == b,
        }
    }
}

/// The agent that last had a reply routed to it.
static LAST_ROUTED_AGENT: Mutex<Option<AgentAddress>> = Mutex::new(None);

/// The agent whose turn completion most recently triggered an away (iMessage) notification.
static LAST_AWAY_NOTIFICATION_SOURCE_AGENT: Mutex<Option<AgentAddress>> = Mutex::new(None);

pub(crate) fn set_last_routed_agent(addr: AgentAddress) {
    *LAST_ROUTED_AGENT.lock().unwrap() = Some(addr);
}

pub(crate) fn set_last_away_notification_source_agent(addr: AgentAddress) {
    *LAST_AWAY_NOTIFICATION_SOURCE_AGENT.lock().unwrap() = Some(addr);
}

fn get_last_routed_agent() -> Option<AgentAddress> {
    LAST_ROUTED_AGENT.lock().unwrap().clone()
}

fn get_last_away_notification_source_agent() -> Option<AgentAddress> {
    LAST_AWAY_NOTIFICATION_SOURCE_AGENT.lock().unwrap().clone()
}

#[cfg(test)]
pub(crate) fn clear_routing_state() {
    *LAST_ROUTED_AGENT.lock().unwrap() = None;
    *LAST_AWAY_NOTIFICATION_SOURCE_AGENT.lock().unwrap() = None;
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

pub(crate) fn live_claude_panes() -> Vec<AgentAddress> {
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
                Some(AgentAddress::TmuxPane { pane_id, label })
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

pub(crate) fn semantic_resolve(body: &str, panes: &[AgentAddress]) -> Option<(usize, String)> {
    if panes.len() <= 1 {
        return None;
    }
    let cfg = get_settings();
    let cli = cfg.ai.cli_path.as_deref()?;

    let labels_list = panes
        .iter()
        .map(|p| format!("- {}", p.label()))
        .collect::<Vec<_>>()
        .join("\n");

    // Strip the closing tag to prevent prompt injection via the message body.
    let safe_body = body.replace("</message>", "");
    let prompt = format!(
        "You are a routing classifier. Do NOT answer or respond to the message content.\n\n\
         MESSAGE TO CLASSIFY:\n<message>\n{safe_body}\n</message>\n\n\
         ACTIVE TMUX PANES:\n{labels_list}\n\n\
         Pane labels use hyphens where users may write spaces (e.g. 'my agent' refers to 'my-agent').\n\
         Does the message contain EXPLICIT routing intent to a specific pane? \
         (direct address like 'To X,', 'ask X', '[X]', 'my agent')\n\
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
            "sonnet",
            "--max-turns",
            "1",
            "--settings",
            r#"{"disableAllHooks":true}"#,
        ])
        .env_remove("CLAUDECODE")
        .envs(ai_cli_env())
        .output()
        .ok()?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        info!(
            status = %out.status,
            stderr = %stderr.chars().take(200).collect::<String>(),
            "semantic resolve: AI CLI failed"
        );
        return None;
    }

    let output = String::from_utf8_lossy(&out.stdout).trim().to_string();
    info!(raw_output = %output, "semantic resolve: AI CLI output");
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
        p.label() == answer
            || answer.to_lowercase().contains(&p.label().to_lowercase())
            || p.label().to_lowercase().contains(&answer.to_lowercase())
    })?;

    Some((idx, cleaned))
}

// ---------------------------------------------------------------------------
// Pane resolution
// ---------------------------------------------------------------------------

pub(crate) fn resolve_pane<'a>(
    tag: Option<&str>,
    body: &str,
    panes: &'a [AgentAddress],
) -> Option<(&'a AgentAddress, String)> {
    let pane_labels: Vec<&str> = panes.iter().map(|p| p.label()).collect();
    info!(available_panes = ?pane_labels, tag = ?tag, "resolving pane");

    if let Some(tag) = tag {
        if let Some(p) = panes.iter().find(|p| p.label() == tag) {
            info!(pane = %p.label(), "resolved via exact tag match");
            return Some((p, body.to_string()));
        }
        let tag_lc = tag.to_lowercase();
        let result = panes
            .iter()
            .find(|p| p.label().to_lowercase().contains(&tag_lc))
            .map(|p| (p, body.to_string()));
        if let Some((p, _)) = &result {
            info!(pane = %p.label(), "resolved via tag substring match");
        } else {
            info!(tag, "no pane matched tag");
        }
        return result;
    }

    if let Some((idx, cleaned)) = semantic_resolve(body, panes) {
        info!(pane = %panes[idx].label(), "resolved via semantic match");
        return Some((&panes[idx], cleaned));
    }
    info!("semantic resolve returned none");

    if let Some(last) = get_last_routed_agent() {
        if let Some(p) = panes.iter().find(|p| p.same_target(&last)) {
            info!(pane = %p.label(), "resolved via last routed agent");
            return Some((p, body.to_string()));
        }
        info!(last_agent = %last.label(), "last routed agent no longer alive");
    } else {
        info!("no last routed agent");
    }

    if let Some(last) = get_last_away_notification_source_agent() {
        if let Some(p) = panes.iter().find(|p| p.same_target(&last)) {
            info!(pane = %p.label(), "resolved via last notification source agent");
            return Some((p, body.to_string()));
        }
        info!(last_agent = %last.label(), "last notification source agent no longer alive");
    } else {
        info!("no last notification source agent");
    }

    if let Some(p) = panes
        .iter()
        .find(|p| p.label().to_lowercase().contains("my-agent"))
    {
        info!(pane = %p.label(), "resolved via my-agent fallback");
        return Some((p, body.to_string()));
    }

    info!("resolution failed — no matching agent");
    None
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
    info!(pane_id, text, "relay_to_pane");
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
    info!(msg, "sending iMessage");
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
    info!(text, "route_reply entered");
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
                .map(|p| p.label())
                .collect::<Vec<_>>()
                .join(", ");
            let msg = match tag {
                Some(t) => format!("No pane matching '{t}'. Available: {available}"),
                None => format!("No active pane found. Available: {available}"),
            };
            send_imessage(&msg);
        }
        Some((agent, cleaned_body)) => {
            let pane_id = agent.tmux_pane_id();
            if !is_pane_alive(pane_id) {
                let available = panes
                    .iter()
                    .filter(|p| !p.same_target(agent))
                    .map(|p| p.label())
                    .collect::<Vec<_>>()
                    .join(", ");
                send_imessage(&format!(
                    "Pane {} is no longer active. Available: {}",
                    agent.label(),
                    available
                ));
                return;
            }
            info!(pane_id, label = %agent.label(), "routing reply");
            relay_to_pane(pane_id, &format!("📱 {cleaned_body}"));
            set_last_routed_agent(agent.clone());
            send_imessage(&format!("✓ Delivered to [{}]", agent.label()));
        }
    }
}

#[cfg(test)]
#[path = "route_reply_test.rs"]
mod tests;
