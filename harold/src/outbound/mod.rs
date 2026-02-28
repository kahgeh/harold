pub mod imessage;
pub mod tts;

use std::process::Command;

use tracing::info;

use crate::inbound::{AgentAddress, set_last_away_notification_source_agent};
use crate::settings::get_settings;
use crate::store::TurnCompleted;
use crate::tmux;

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
// Screen lock detection
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

// ---------------------------------------------------------------------------
// Notify orchestrator
// ---------------------------------------------------------------------------

pub fn notify(turn: &TurnCompleted, trace_id: &str) {
    let cfg = get_settings();
    let screen_locked = is_screen_locked();

    // Session-level skip: if completing pane's session has an attached client, skip entirely.
    // Takes precedence — when this fires, pane-level skip is irrelevant.
    if cfg.notify.skip_if_session_active && tmux::is_session_attached(&turn.pane_id) {
        info!("notification skipped (session is active)");
        return;
    }

    // Pane-level skip: skip only when the completing pane is the active pane
    // AND the screen is not locked (user is at desk looking at it).
    // If screen is locked, always notify even if pane matches.
    if cfg.notify.skip_if_pane_active
        && !screen_locked
        && let Some(active_pane) = tmux::active_pane_in_session(&turn.pane_id)
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
