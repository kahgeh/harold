pub mod directory;
pub(crate) mod ownership;
pub(crate) mod tmux;

use std::path::Path;
use std::process::Command;
use std::sync::Mutex;

use opentelemetry::Context;
use time::OffsetDateTime;
use tracing::{info, warn};

use crate::outbound::{DeliveryOutcome, send_reply};
use crate::settings::get_settings;
use crate::telemetry::{self, ResolvedBy, Routing, StartedBy};
use crate::timed_prompts::pane_lock;
use crate::util::ai_cli_env;
use crate::web_app::chief::ChiefPane;

pub use directory::AgentAddress;
use directory::AgentDirectory;
use ownership::{Ownership, decide_ownership, delivered_text, error_text};

// ---------------------------------------------------------------------------
// State — agent routing (in-memory; Harold owns all routing state)
// ---------------------------------------------------------------------------

static LAST_AWAY_NOTIFICATION: Mutex<Option<AgentAddress>> = Mutex::new(None);

pub(crate) fn set_last_away_notification_source_agent(addr: AgentAddress) {
    *LAST_AWAY_NOTIFICATION.lock().unwrap() = Some(addr);
}

fn get_last_away_notification_source_agent() -> Option<AgentAddress> {
    LAST_AWAY_NOTIFICATION.lock().unwrap().clone()
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

/// The pane `tag` or `body` names, the text to type into it, and how it was chosen: by
/// the tag (exact, then as part of a label), by the AI model, as the pane that last sent
/// an away notification, or as the `my-agent` pane. A tag that matches no pane is not
/// tried any other way.
pub(crate) fn resolve_pane<'a>(
    tag: Option<&str>,
    body: &str,
    panes: &'a [AgentAddress],
) -> Option<(&'a AgentAddress, String, ResolvedBy)> {
    let pane_labels: Vec<&str> = panes.iter().map(|p| p.label()).collect();
    info!(available_panes = ?pane_labels, tag = ?tag, "resolving pane");

    if let Some(tag) = tag {
        if let Some(p) = panes.iter().find(|p| p.label() == tag) {
            info!(pane = %p.label(), "resolved via exact tag match");
            return Some((p, body.to_string(), ResolvedBy::ExactTag));
        }
        let tag_lc = tag.to_lowercase();
        let result = panes
            .iter()
            .find(|p| p.label().to_lowercase().contains(&tag_lc))
            .map(|p| (p, body.to_string(), ResolvedBy::TagSubstring));
        if let Some((p, _, _)) = &result {
            info!(pane = %p.label(), "resolved via tag substring match");
        } else {
            info!(tag, "no pane matched tag");
        }
        return result;
    }

    if let Some((idx, cleaned)) = semantic_resolve(body, panes) {
        info!(pane = %panes[idx].label(), "resolved via semantic match");
        return Some((&panes[idx], cleaned, ResolvedBy::AiModel));
    }
    info!("semantic resolve returned none");

    if let Some(last) = get_last_away_notification_source_agent() {
        if let Some(p) = panes.iter().find(|p| p.same_target(&last)) {
            info!(pane = %p.label(), "resolved via last notification source agent");
            return Some((p, body.to_string(), ResolvedBy::LatestNotifier));
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
        return Some((p, body.to_string(), ResolvedBy::MyAgent));
    }

    info!("resolution failed — no matching agent");
    None
}

// ---------------------------------------------------------------------------
// Route a received reply — called from the event handler.
// ---------------------------------------------------------------------------

/// What routing one reply did: the delivery's outcome, and how the phone request counts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RouteReport {
    pub outcome: DeliveryOutcome,
    pub routing: Routing,
}

impl RouteReport {
    /// The reply was not typed: no pane, a tag that matches none, or a pane that is gone.
    fn no_pane() -> Self {
        Self {
            outcome: DeliveryOutcome::Skipped,
            routing: Routing::NoPane,
        }
    }
}

/// Route a received reply to the appropriate agent pane, if this Mac owns it.
///
/// **Must be called from `spawn_blocking`** — this function uses
/// `Handle::block_on` internally and will deadlock if called from an
/// async task on the tokio worker pool.
pub(crate) fn route_inbound_message(text: &str) -> Result<RouteReport, String> {
    info!(text, "route_inbound_message entered");
    let machine = get_settings().machine_name.as_str();
    let ownership = decide_ownership(
        text,
        machine,
        &ownership::known_machine_names(),
        ownership::latest_notifier().as_deref(),
    );
    route_if_owned(text, ownership, |pane_tag, body| {
        route_owned_reply(&TmuxEnv, machine, pane_tag, body)
    })
}

/// Hand an owned reply to `route`. A reply another Mac owns is skipped here, before any
/// pane discovery, tmux call or message to the user. It is `tagged` when the tag of
/// `text` named the owning Mac.
fn route_if_owned(
    text: &str,
    ownership: Ownership<'_>,
    route: impl FnOnce(Option<&str>, &str) -> Result<RouteReport, String>,
) -> Result<RouteReport, String> {
    match ownership {
        Ownership::Owned { pane_tag, body } => route(pane_tag, body),
        Ownership::NotOwned { owner } => {
            info!(owner, "reply dropped (another Mac owns it)");
            Ok(RouteReport {
                outcome: DeliveryOutcome::Skipped,
                routing: Routing::NotOwned {
                    tagged: tag_names_machine(text, &owner),
                },
            })
        }
    }
}

/// Whether the tag of `text` (`[machine]` or `[machine/pane]`) names `machine`. A reply
/// another Mac owns either named it so, or carried no machine and followed the latest
/// notifier there.
fn tag_names_machine(text: &str, machine: &str) -> bool {
    parse_tag(text).0.is_some_and(|tag| {
        let named = tag.split_once('/').map_or(tag, |(named, _)| named);
        ownership::same_machine(named.trim(), machine)
    })
}

/// What routing an owned reply needs from the machine around it: the panes, the relay and
/// the reply to the user. A seam so the texts sent to the user are tested without tmux.
trait RoutingEnv {
    fn discover(&self) -> Vec<AgentAddress>;
    fn is_alive(&self, agent: &AgentAddress) -> bool;
    fn relay(&self, agent: &AgentAddress, text: &str) -> Result<(), String>;
    fn send_reply(&self, text: &str) -> Result<(), String>;
}

struct TmuxEnv;

impl RoutingEnv for TmuxEnv {
    fn discover(&self) -> Vec<AgentAddress> {
        AgentDirectory::TmuxProcessScan.discover()
    }

    fn is_alive(&self, agent: &AgentAddress) -> bool {
        AgentDirectory::TmuxProcessScan.is_alive(agent)
    }

    fn relay(&self, agent: &AgentAddress, text: &str) -> Result<(), String> {
        agent.relay(text)
    }

    fn send_reply(&self, text: &str) -> Result<(), String> {
        send_reply(text)
    }
}

fn route_owned_reply(
    env: &impl RoutingEnv,
    machine: &str,
    tag: Option<&str>,
    body: &str,
) -> Result<RouteReport, String> {
    let panes = env.discover();

    if panes.is_empty() {
        env.send_reply(&error_text(machine, "No active agent sessions found."))?;
        return Ok(RouteReport::no_pane());
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
            env.send_reply(&error_text(machine, &msg))?;
            Ok(RouteReport::no_pane())
        }
        Some((agent, cleaned_body, resolved_by)) => {
            if !env.is_alive(agent) {
                let available = panes
                    .iter()
                    .filter(|p| !p.same_target(agent))
                    .map(|p| p.label())
                    .collect::<Vec<_>>()
                    .join(", ");
                let msg = format!(
                    "Pane {} is no longer active. Available: {}",
                    agent.label(),
                    available
                );
                env.send_reply(&error_text(machine, &msg))?;
                return Ok(RouteReport::no_pane());
            }
            info!(label = %agent.label(), "routing reply");
            env.relay(agent, &format!("📱 {cleaned_body}"))?;
            // Typed and Enter pressed: the request ends here, before the user is told.
            let typed_at = OffsetDateTime::now_utc();
            if let Err(error) = env.send_reply(&delivered_text(machine, agent.label())) {
                warn!(error, "routed message but failed to send confirmation");
            }
            Ok(RouteReport {
                outcome: DeliveryOutcome::Delivered,
                routing: Routing::Typed {
                    pane_id: agent.pane_id().to_string(),
                    resolved_by,
                    tagged: tag.is_some(),
                    typed_at,
                },
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Route a reply typed in the web app — straight to the chief.
// ---------------------------------------------------------------------------

/// What routing a reply the user typed in the web app did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ChiefRouting {
    /// Typed into the chief's pane at `typed_at`; `busy` when the chief was busy then.
    Typed {
        pane_id: String,
        busy: bool,
        typed_at: OffsetDateTime,
    },
    /// No agent runs in the chief's folder: nothing was typed.
    NoChief,
}

/// Types "📱 <text>" into the chief's pane: no tag, AI or latest-notifier decision. No
/// agent → NoChief, nothing typed. With several agents in the folder the chief is the first
/// agent pane of the chief's tmux session. A failing
/// listing → Err. On typing, records telemetry::prompt_typed(pane, StartedBy::Phone, None,
/// typed_at), so the chief's turn counts as started from the phone. A typed app reply is
/// not counted in the phone-request counters, which keep a fixed channel list. Logs
/// nothing that holds the text.
#[cfg(test)]
pub(crate) fn route_to_chief(
    text: &str,
    photo: Option<&Path>,
    chief: &ChiefPane,
    type_into: &dyn Fn(&str, &str) -> Result<(), String>,
) -> Result<ChiefRouting, String> {
    route_to_chief_in(text, photo, chief, type_into, None)
}

/// `route_to_chief` for a reply whose phone request has the context `request`: the typed
/// prompt keeps it, so the chief's turn joins the request's trace.
pub(crate) fn route_to_chief_in(
    text: &str,
    photo: Option<&Path>,
    chief: &ChiefPane,
    type_into: &dyn Fn(&str, &str) -> Result<(), String>,
    request: Option<Context>,
) -> Result<ChiefRouting, String> {
    let (pane_id, busy) = match chief {
        ChiefPane::One { pane_id, busy } => (pane_id, *busy),
        ChiefPane::NoChief => return Ok(ChiefRouting::NoChief),
        ChiefPane::ListingFailed => {
            return Err(
                "the tmux pane listing is failing, so the chief's pane is not known".into(),
            );
        }
    };
    let line = match photo {
        Some(path) if text.is_empty() => format!("📱 📷 {}", path.display()),
        Some(path) => format!("📱 {text} 📷 {}", path.display()),
        None => format!("📱 {text}"),
    };
    type_into(pane_id, &line)?;
    let typed_at = OffsetDateTime::now_utc();
    telemetry::prompt_typed(pane_id, StartedBy::Phone, request, typed_at);
    Ok(ChiefRouting::Typed {
        pane_id: pane_id.clone(),
        busy,
        typed_at,
    })
}

/// relay_holding_lock(pane_lock(pane), type_text_into_tmux_pane + press_enter_in_tmux_pane): the iMessage
/// reply path's typing, without relay_to_tmux_pane's log line that holds the text.
pub(crate) fn type_into_pane_quietly(pane_id: &str, text: &str) -> Result<(), String> {
    tmux::relay_holding_lock(&pane_lock(pane_id), || {
        tmux::type_text_into_tmux_pane(pane_id, text)?;
        tmux::press_enter_in_tmux_pane(pane_id)
    })
}

// ---------------------------------------------------------------------------
// Public re-exports for diagnostics / other modules
// ---------------------------------------------------------------------------

pub fn scan_live_panes() -> Vec<AgentAddress> {
    tmux::scan_live_panes()
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
