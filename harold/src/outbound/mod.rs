pub mod imessage;
pub mod tts;

use std::process::Command;

use tracing::info;

use crate::inbound::{AgentAddress, set_last_away_notification_source_agent};
use crate::settings::get_settings;
use crate::store::TurnCompleted;

// ---------------------------------------------------------------------------
// OutboundChannel — notification to human
// ---------------------------------------------------------------------------

pub enum OutboundChannel {
    Tts,
    IMessage,
}

impl OutboundChannel {
    /// Send notification. Returns the source agent address if applicable (for routing state).
    pub fn notify(&self, turn: &TurnCompleted, trace_id: &str) -> Option<AgentAddress> {
        match self {
            OutboundChannel::Tts => {
                tts::notify_at_desk(turn, trace_id);
                None
            }
            OutboundChannel::IMessage => imessage::notify_away(turn, trace_id),
        }
    }
}

// ---------------------------------------------------------------------------
// Session helpers
// ---------------------------------------------------------------------------

pub fn is_screen_locked() -> bool {
    let result = Command::new("bash")
        .args([
            "-c",
            "ioreg -n Root -d1 -a | plutil -extract IOConsoleLocked raw -",
        ])
        .output();

    match result {
        Ok(out) => String::from_utf8_lossy(&out.stdout).trim() == "true",
        Err(_) => false,
    }
}

fn tmux_query(args: &[&str]) -> Option<String> {
    let out = Command::new("tmux").args(args).output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

fn active_tmux_session() -> Option<String> {
    tmux_query(&["display-message", "-p", "#{session_name}"])
}

fn pane_session(pane_id: &str) -> Option<String> {
    tmux_query(&["display-message", "-t", pane_id, "-p", "#{session_name}"])
}

fn active_tmux_pane() -> Option<String> {
    tmux_query(&["display-message", "-p", "#{pane_id}"])
}

// ---------------------------------------------------------------------------
// Notify orchestrator
// ---------------------------------------------------------------------------

pub fn notify(turn: &TurnCompleted, trace_id: &str) {
    let cfg = get_settings();
    let screen_locked = is_screen_locked();

    // Session-level skip: if completing pane is in the active session, skip entirely.
    // Takes precedence — when this fires, pane-level skip is irrelevant.
    if cfg.notify.skip_if_session_active
        && let (Some(active_session), Some(pane_session)) =
            (active_tmux_session(), pane_session(&turn.pane_id))
        && active_session == pane_session
    {
        info!("notification skipped (session is active)");
        return;
    }

    // Pane-level skip: skip only when the completing pane is the active pane
    // AND the screen is not locked (user is at desk looking at it).
    // If screen is locked, always notify even if pane matches.
    if cfg.notify.skip_if_pane_active
        && !screen_locked
        && let Some(active_pane) = active_tmux_pane()
        && active_pane == turn.pane_id
    {
        info!("notification skipped (pane is active and screen unlocked)");
        return;
    }

    let channel = if screen_locked {
        OutboundChannel::IMessage
    } else {
        OutboundChannel::Tts
    };

    if let Some(source_agent) = channel.notify(turn, trace_id) {
        set_last_away_notification_source_agent(source_agent);
    }
}
