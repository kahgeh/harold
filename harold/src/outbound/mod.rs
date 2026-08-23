pub mod tts;

use std::process::Command;
use std::sync::Mutex;
use std::time::Instant;

use tracing::info;

use crate::channels;
use crate::inbound::{AgentAddress, set_last_away_notification_source_agent};
use crate::settings::get_settings;
use crate::store::TurnCompleted;
use crate::tmux;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeliveryOutcome {
    Delivered,
    Skipped,
}

// ---------------------------------------------------------------------------
// Deduplication — suppress repeated notifications for the same turn
// ---------------------------------------------------------------------------

static LAST_NOTIFY: Mutex<Option<(String, Instant)>> = Mutex::new(None);
const DEDUP_WINDOW_SECS: u64 = 30;

// ---------------------------------------------------------------------------
// OutboundChannel — notification to human
// ---------------------------------------------------------------------------

pub enum OutboundChannel {
    Tts,
    Away,
}

impl OutboundChannel {
    /// Send notification. Returns the source agent address if applicable (for routing state).
    pub fn notify(
        &self,
        turn: &TurnCompleted,
        trace_id: &str,
    ) -> Result<(DeliveryOutcome, Option<AgentAddress>), String> {
        match self {
            OutboundChannel::Tts => {
                if tts::notify_at_desk(turn, trace_id) {
                    Ok((DeliveryOutcome::Delivered, None))
                } else {
                    Err("TTS notification command failed".into())
                }
            }
            OutboundChannel::Away => match channels::notify_away(turn, trace_id)? {
                channels::AwayNotification::Sent(agent) => {
                    Ok((DeliveryOutcome::Delivered, Some(agent)))
                }
                channels::AwayNotification::Skipped => Ok((DeliveryOutcome::Skipped, None)),
            },
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

pub fn notify(turn: &TurnCompleted, trace_id: &str) -> Result<DeliveryOutcome, String> {
    // Dedup: skip if same pane+prompt was notified within the window.
    let dedup_key = format!("{}:{}", turn.pane_id, turn.last_user_prompt);
    {
        let last = LAST_NOTIFY.lock().unwrap();
        if let Some((ref prev_key, ref ts)) = *last
            && prev_key == &dedup_key
            && ts.elapsed().as_secs() < DEDUP_WINDOW_SECS
        {
            info!("notification skipped (duplicate within dedup window)");
            return Ok(DeliveryOutcome::Skipped);
        }
    }

    let cfg = get_settings();
    let screen_locked = is_screen_locked();

    // Session-level skip: if completing pane's session has an attached client AND the
    // screen is not locked, skip entirely.  When the screen is locked the user is away
    // from the desk, so we must still notify even though tmux is attached.
    if cfg.notify.skip_if_session_active
        && !screen_locked
        && tmux::is_session_attached(&turn.pane_id)
    {
        info!("notification skipped (session is active, screen unlocked)");
        return Ok(DeliveryOutcome::Skipped);
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
        return Ok(DeliveryOutcome::Skipped);
    }

    let channel = if screen_locked {
        OutboundChannel::Away
    } else {
        OutboundChannel::Tts
    };

    let (outcome, source_agent) = channel.notify(turn, trace_id)?;
    if let Some(source_agent) = source_agent {
        set_last_away_notification_source_agent(source_agent);
    }

    if outcome == DeliveryOutcome::Delivered {
        *LAST_NOTIFY.lock().unwrap() = Some((dedup_key, Instant::now()));
    }
    Ok(outcome)
}

// ---------------------------------------------------------------------------
// Channel-aware reply — delegates to channels::send()
// ---------------------------------------------------------------------------

/// Send a confirmation/error message back through the configured away channel.
pub fn send_reply(msg: &str) -> Result<(), String> {
    channels::send(msg)
}
