pub mod directory;
pub(crate) mod tmux;

use std::process::Command;
use std::sync::{Arc, Mutex};

use events::EventStore;
use tracing::{info, warn};

use crate::outbound::imessage::send_imessage;
use crate::settings::get_settings;
use crate::store::{ReadoutRequested, append_readout_requested};
use crate::util::ai_cli_env;

pub use directory::AgentAddress;
use directory::AgentDirectory;

// ---------------------------------------------------------------------------
// State — agent routing (in-memory; Harold owns all routing state)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub(crate) struct LastTurnContext {
    pub assistant_message: String,
    pub last_user_prompt: String,
}

struct AwayNotificationState {
    source_agent: AgentAddress,
    turn_context: LastTurnContext,
}

static LAST_AWAY_NOTIFICATION: Mutex<Option<AwayNotificationState>> = Mutex::new(None);

pub(crate) fn set_last_away_notification_source_agent(
    addr: AgentAddress,
    turn_context: LastTurnContext,
) {
    *LAST_AWAY_NOTIFICATION.lock().unwrap() = Some(AwayNotificationState {
        source_agent: addr,
        turn_context,
    });
}

fn get_last_away_notification_source_agent() -> Option<AgentAddress> {
    LAST_AWAY_NOTIFICATION
        .lock()
        .unwrap()
        .as_ref()
        .map(|s| s.source_agent.clone())
}

pub(crate) fn get_last_turn_context() -> Option<LastTurnContext> {
    LAST_AWAY_NOTIFICATION
        .lock()
        .unwrap()
        .as_ref()
        .map(|s| s.turn_context.clone())
}

#[cfg(test)]
pub(crate) fn clear_routing_state() {
    *LAST_AWAY_NOTIFICATION.lock().unwrap() = None;
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
// Readout intent detection
// ---------------------------------------------------------------------------

fn is_readout_request(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.starts_with("read the ")
        || lower.starts_with("read me ")
        || lower.starts_with("read that")
        || lower.starts_with("read file")
        || lower == "read it"
}

// ---------------------------------------------------------------------------
// Route a received reply — called from projector
// ---------------------------------------------------------------------------

/// Route a received reply to the appropriate agent pane.
///
/// **Must be called from `spawn_blocking`** — this function uses
/// `Handle::block_on` internally and will deadlock if called from an
/// async task on the tokio worker pool.
pub fn route_reply(text: &str, store: Arc<EventStore>) {
    let directory = AgentDirectory::TmuxProcessScan;
    info!(text, "route_reply entered");
    let (tag, body) = parse_tag(text);

    // Check for readout request before routing to agents.
    if is_readout_request(body) {
        let Some(last_agent) = get_last_away_notification_source_agent() else {
            send_imessage("No recent notification to look up files from");
            return;
        };
        let Some(ctx) = get_last_turn_context() else {
            send_imessage("No recent conversation context for file readout");
            return;
        };
        let event = ReadoutRequested {
            user_message: body.to_string(),
            last_assistant_message: ctx.assistant_message,
            last_user_prompt: ctx.last_user_prompt,
            pane_id: last_agent.pane_id().to_string(),
        };
        let rt = tokio::runtime::Handle::current();
        match rt.block_on(append_readout_requested(&store, &event)) {
            Ok(()) => {
                send_imessage("Looking up that file...");
            }
            Err(e) => {
                warn!(error = %e, "failed to append ReadoutRequested event");
                send_imessage("Couldn't process the readout request");
            }
        }
        return;
    }

    let panes = directory.discover();

    if panes.is_empty() {
        send_imessage("No active agent sessions found.");
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
            if !directory.is_alive(agent) {
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
            info!(label = %agent.label(), "routing reply");
            agent.relay(&format!("📱 {cleaned_body}"));
            send_imessage(&format!("✓ Delivered to [{}]", agent.label()));
        }
    }
}

// ---------------------------------------------------------------------------
// Public re-exports for diagnostics / other modules
// ---------------------------------------------------------------------------

pub fn scan_live_panes() -> Vec<AgentAddress> {
    tmux::scan_live_panes()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::is_readout_request;
    use crate::inbound::{
        AgentAddress, LastTurnContext, clear_routing_state, parse_tag, resolve_pane,
        set_last_away_notification_source_agent,
    };
    use crate::settings::init_settings_for_test;

    /// Serialises tests that mutate global routing state.
    static ROUTING_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn tmux(pane_id: &str, label: &str) -> AgentAddress {
        AgentAddress::TmuxPane {
            pane_id: pane_id.into(),
            label: label.into(),
        }
    }

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
        let panes = vec![tmux("%1", "work:0.0"), tmux("%2", "home:0.1")];
        let result = resolve_pane(Some("work:0.0"), "hi", &panes);
        assert!(result.is_some());
        assert_eq!(result.unwrap().0.pane_id(), "%1");
    }

    #[test]
    fn resolve_pane_substring_match() {
        let panes = vec![tmux("%1", "work:0.0"), tmux("%2", "home:0.1")];
        let result = resolve_pane(Some("home"), "hi", &panes);
        assert!(result.is_some());
        assert_eq!(result.unwrap().0.pane_id(), "%2");
    }

    #[test]
    fn resolve_pane_no_tag_falls_back_to_my_agent() {
        let _lock = ROUTING_TEST_LOCK.lock().unwrap();
        clear_routing_state();
        let panes = vec![tmux("%1", "my-agent:0.0")];
        let result = resolve_pane(None, "hi", &panes);
        assert!(result.is_some());
        assert_eq!(result.unwrap().0.pane_id(), "%1");
    }

    #[test]
    fn resolve_pane_last_away_notification_source_beats_my_agent() {
        let _lock = ROUTING_TEST_LOCK.lock().unwrap();
        init_settings_for_test();
        clear_routing_state();
        let panes = vec![tmux("%3", "alir-app:0.1"), tmux("%4", "my-agent:0.0")];
        set_last_away_notification_source_agent(
            tmux("%3", "alir-app:0.1"),
            LastTurnContext {
                assistant_message: String::new(),
                last_user_prompt: String::new(),
            },
        );
        let result = resolve_pane(None, "hi", &panes);
        assert!(result.is_some());
        assert_eq!(result.unwrap().0.pane_id(), "%3");
    }

    #[test]
    fn resolve_pane_no_match_returns_none() {
        let panes = vec![tmux("%1", "work:0.0")];
        let result = resolve_pane(Some("nonexistent"), "hi", &panes);
        assert!(result.is_none());
    }

    #[test]
    fn is_readout_request_matches_expected_phrases() {
        assert!(is_readout_request("read the Cargo.toml"));
        assert!(is_readout_request("Read the main.rs"));
        assert!(is_readout_request("read me the config"));
        assert!(is_readout_request("read that file"));
        assert!(is_readout_request("read file settings.toml"));
        assert!(is_readout_request("read it"));
    }

    #[test]
    fn is_readout_request_rejects_non_readout() {
        assert!(!is_readout_request("hello there"));
        assert!(!is_readout_request("please read"));
        assert!(!is_readout_request("can you read this?"));
        assert!(!is_readout_request("reading the file now"));
    }
}
