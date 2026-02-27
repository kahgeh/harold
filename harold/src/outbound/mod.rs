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

fn active_tmux_session() -> Option<String> {
    let out = Command::new("tmux")
        .args(["display-message", "-l", "-p", "#{session_name}"])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

fn pane_session(pane_id: &str) -> Option<String> {
    let out = Command::new("tmux")
        .args(["display-message", "-t", pane_id, "-p", "#{session_name}"])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

// ---------------------------------------------------------------------------
// Notify orchestrator
// ---------------------------------------------------------------------------

pub fn notify(turn: &TurnCompleted, trace_id: &str) {
    let cfg = get_settings();

    if cfg.notify.skip_if_session_active
        && let (Some(active_session), Some(pane_session)) =
            (active_tmux_session(), pane_session(&turn.pane_id))
        && active_session == pane_session
    {
        info!("notification skipped (session is active)");
        return;
    }

    let channel = match is_screen_locked() {
        true => OutboundChannel::IMessage,
        false => OutboundChannel::Tts,
    };

    if let Some(source_agent) = channel.notify(turn, trace_id) {
        set_last_away_notification_source_agent(source_agent);
    }
}
