use std::collections::HashSet;
use std::process::Command;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI64, Ordering};

use tracing::info;

use crate::settings::get_settings;

// Seeded in run_reply_router before the poll loop starts.
static LAST_PROCESSED_ROWID: OnceLock<AtomicI64> = OnceLock::new();

fn last_rowid() -> &'static AtomicI64 {
    LAST_PROCESSED_ROWID
        .get()
        .expect("reply router not initialised")
}

#[derive(Debug, Clone)]
pub struct PaneInfo {
    pub pane_id: String,
    pub label: String,
}

// ---------------------------------------------------------------------------
// State — last_notified_pane (in-memory; Harold owns all routing state)
// ---------------------------------------------------------------------------

use std::sync::Mutex;
static LAST_NOTIFIED_PANE: Mutex<Option<PaneInfo>> = Mutex::new(None);

pub fn set_last_notified_pane(pane: PaneInfo) {
    *LAST_NOTIFIED_PANE.lock().unwrap() = Some(pane);
}

fn get_last_notified_pane() -> Option<PaneInfo> {
    LAST_NOTIFIED_PANE.lock().unwrap().clone()
}

// ---------------------------------------------------------------------------
// chat.db access via sqlite3 CLI
// ---------------------------------------------------------------------------

fn db_path() -> String {
    get_settings().chat_db.resolved_path()
}

fn handle_ids() -> HashSet<i64> {
    let cfg = get_settings();
    let mut ids = HashSet::new();
    if let Some(id) = cfg.imessage.handle_id {
        ids.insert(id);
    }
    if let Some(extras) = &cfg.imessage.extra_handle_ids {
        ids.extend(extras);
    }
    ids
}

fn get_max_rowid() -> i64 {
    // last_rowid is i64 from sqlite3 output — numeric, safe to interpolate if reused in SQL.
    let out = Command::new("sqlite3")
        .arg(db_path())
        .arg("SELECT MAX(ROWID) FROM message;")
        .output()
        .ok();
    out.and_then(|o| {
        String::from_utf8_lossy(&o.stdout)
            .trim()
            .parse::<i64>()
            .ok()
    })
    .unwrap_or(0)
}

fn fetch_new_messages(last_rowid: i64) -> Vec<(i64, String)> {
    let ids = handle_ids();
    if ids.is_empty() {
        return vec![];
    }
    // id_list is built from HashSet<i64> — all values are numeric, no injection risk.
    // last_rowid is i64 from AtomicI64 — numeric, safe to interpolate.
    let id_list = ids
        .iter()
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT ROWID, text FROM message \
         WHERE ROWID > {last_rowid} AND handle_id IN ({id_list}) AND is_from_me = 0 \
         ORDER BY ROWID ASC;"
    );

    let out = match Command::new("sqlite3").arg(db_path()).arg(&sql).output() {
        Ok(o) if o.status.success() => o,
        _ => return vec![],
    };

    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let (rowid_s, text) = line.split_once('|')?;
            let rowid = rowid_s.trim().parse::<i64>().ok()?;
            let text = text.trim().to_string();
            if text.is_empty() {
                None
            } else {
                Some((rowid, text))
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tag parsing
// ---------------------------------------------------------------------------

fn parse_tag(text: &str) -> (Option<&str>, &str) {
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

fn is_claude_code_process(cmd: &str) -> bool {
    // Claude Code runs as a node process named like "16.20.1" (the node version)
    // or with the claude binary name. We match process names that look like
    // a semver string (digits.digits.digits) which is how node appears in tmux.
    // TODO: replace with explicit pane registration via the TurnComplete RPC.
    cmd.split('.').count() >= 3
        && cmd
            .split('.')
            .all(|p| p.chars().all(|c| c.is_ascii_digit()))
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

fn ai_cli_env() -> Vec<(String, String)> {
    let allowed = [
        "PATH",
        "HOME",
        "ANTHROPIC_API_KEY",
        "TMPDIR",
        "LANG",
        "LC_ALL",
    ];
    std::env::vars()
        .filter(|(k, _)| allowed.contains(&k.as_str()))
        .collect()
}

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
// Pane resolution — returns index into panes slice + cleaned body
// ---------------------------------------------------------------------------

fn resolve_pane<'a>(
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

fn strip_control(text: &str) -> String {
    // Remove ANSI escape sequences and control characters before sending to tmux.
    // The '-l' flag prevents shell interpretation but raw bytes still reach the pane.
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip ANSI escape sequence: ESC [ ... final-byte
            if chars.peek() == Some(&'[') {
                chars.next();
                for c2 in chars.by_ref() {
                    if c2.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else if c.is_control() && c != '\n' {
            // Drop other control characters; keep newline (handled as Enter)
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
// Error / confirmation iMessage
// ---------------------------------------------------------------------------

fn send_imessage(msg: &str) {
    let cfg = get_settings();
    let Some(recipient) = cfg.imessage.recipient.as_deref() else {
        return;
    };
    // Both msg and recipient are internal strings — sanitise for AppleScript safety.
    let sanitise = |s: &str| -> String {
        s.chars()
            .filter(|c| *c != '\n' && *c != '\r' && *c != '¬' && !c.is_control())
            .collect()
    };
    let escaped_msg = sanitise(msg).replace('\\', "\\\\").replace('"', "\\\"");
    let escaped_rec = sanitise(recipient)
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    let script = format!(
        "tell application \"Messages\" to send \"{escaped_msg}\" to buddy \"{escaped_rec}\""
    );
    let _ = Command::new("osascript").args(["-e", &script]).status();
}

// ---------------------------------------------------------------------------
// Poll logic
// ---------------------------------------------------------------------------

fn do_poll() {
    let current_rowid = last_rowid().load(Ordering::SeqCst);
    let messages = fetch_new_messages(current_rowid);

    for (rowid, text) in messages {
        // Mark as seen first — prevents re-processing if we crash mid-loop.
        last_rowid().store(rowid, Ordering::SeqCst);

        info!(rowid, text = %text.chars().take(80).collect::<String>(), "new iMessage");

        let (tag, body) = parse_tag(&text);
        let panes = live_claude_panes();

        if panes.is_empty() {
            send_imessage("No Claude Code sessions active.");
            continue;
        }

        match resolve_pane(tag, body, &panes) {
            None => {
                let available = panes
                    .iter()
                    .map(|p| p.label.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                if let Some(t) = tag {
                    send_imessage(&format!("No pane matching '{t}'. Available: {available}"));
                } else {
                    send_imessage(&format!("No active pane found. Available: {available}"));
                }
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
                } else {
                    info!(pane_id = %pane.pane_id, label = %pane.label, "routing reply");
                    relay_to_pane(&pane.pane_id, &format!("📱 {cleaned_body}"));
                    send_imessage(&format!("✓ Delivered to [{}]", pane.label));
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

pub async fn run_reply_router() {
    let initial = tokio::task::spawn_blocking(get_max_rowid)
        .await
        .unwrap_or(0);
    LAST_PROCESSED_ROWID
        .set(AtomicI64::new(initial))
        .expect("run_reply_router called more than once");
    info!(initial_rowid = initial, "reply router started");

    loop {
        tokio::task::spawn_blocking(do_poll).await.ok();
        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tag_with_tag() {
        let (tag, body) = parse_tag("[main] hello world");
        assert_eq!(tag, Some("main"));
        assert_eq!(body, "hello world");
    }

    #[test]
    fn parse_tag_without_tag() {
        let (tag, body) = parse_tag("just a message");
        assert_eq!(tag, None);
        assert_eq!(body, "just a message");
    }

    #[test]
    fn parse_tag_unclosed_bracket() {
        let (tag, body) = parse_tag("[unclosed message");
        assert_eq!(tag, None);
        assert_eq!(body, "[unclosed message");
    }

    #[test]
    fn resolve_pane_exact_match() {
        let panes = vec![
            PaneInfo {
                pane_id: "%1".into(),
                label: "work:0.0".into(),
            },
            PaneInfo {
                pane_id: "%2".into(),
                label: "home:0.1".into(),
            },
        ];
        let result = resolve_pane(Some("work:0.0"), "hi", &panes);
        assert!(result.is_some());
        assert_eq!(result.unwrap().0.pane_id, "%1");
    }

    #[test]
    fn resolve_pane_substring_match() {
        let panes = vec![
            PaneInfo {
                pane_id: "%1".into(),
                label: "work:0.0".into(),
            },
            PaneInfo {
                pane_id: "%2".into(),
                label: "home:0.1".into(),
            },
        ];
        let result = resolve_pane(Some("home"), "hi", &panes);
        assert!(result.is_some());
        assert_eq!(result.unwrap().0.pane_id, "%2");
    }

    #[test]
    fn resolve_pane_no_tag_falls_back_to_my_agent() {
        let panes = vec![PaneInfo {
            pane_id: "%1".into(),
            label: "my-agent:0.0".into(),
        }];
        let result = resolve_pane(None, "hi", &panes);
        assert!(result.is_some());
        assert_eq!(result.unwrap().0.pane_id, "%1");
    }

    #[test]
    fn resolve_pane_no_match_returns_none() {
        let panes = vec![PaneInfo {
            pane_id: "%1".into(),
            label: "work:0.0".into(),
        }];
        let result = resolve_pane(Some("nonexistent"), "hi", &panes);
        assert!(result.is_none());
    }

    #[test]
    fn strip_control_removes_ansi_and_controls() {
        // ANSI escape sequences are stripped; \x01 (SOH) is a control char and stripped;
        // the text "hidden" after it is plain ASCII and passes through.
        let input = "\x1b[31mred\x1b[0m normal\x01hidden";
        let output = strip_control(input);
        assert_eq!(output, "red normalhidden");
    }

    #[test]
    fn strip_control_removes_lone_control_chars() {
        // A lone \x01 with no trailing text is stripped entirely.
        let input = "clean\x01";
        let output = strip_control(input);
        assert_eq!(output, "clean");
    }

    #[test]
    fn is_claude_code_process_matches_node_version() {
        assert!(is_claude_code_process("16.20.1"));
        assert!(is_claude_code_process("20.11.0"));
        assert!(!is_claude_code_process("python3.11"));
        assert!(!is_claude_code_process("bash"));
        assert!(!is_claude_code_process("node"));
    }
}
