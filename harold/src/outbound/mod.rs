pub mod tts;

pub(crate) mod left_out;

#[cfg(test)]
pub(crate) mod fake;
#[cfg(test)]
mod notify_tests;

use std::collections::HashMap;
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use tracing::info;

use crate::channels;
use crate::inbound::{AgentAddress, set_last_away_notification_source_agent};
use crate::settings::{NotifySettings, get_settings};
use crate::store::{AwayMessageRequested, TurnCompleted};
use crate::tmux;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeliveryOutcome {
    Delivered,
    Skipped,
}

// ---------------------------------------------------------------------------
// Deduplication — suppress repeated notifications for the same turn
// ---------------------------------------------------------------------------

const DEDUP_WINDOW_SECS: u64 = 30;

// Tests run in parallel and several of them flip these switches through the production
// wiring, so under test every thread gets its own.
#[cfg(not(test))]
static TTS_MUTED: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
thread_local! {
    static TTS_MUTED: AtomicBool = const { AtomicBool::new(false) };
}

fn with_tts_switch<T>(read_or_write: impl FnOnce(&AtomicBool) -> T) -> T {
    #[cfg(not(test))]
    return read_or_write(&TTS_MUTED);
    #[cfg(test)]
    TTS_MUTED.with(read_or_write)
}

pub fn set_tts_muted(muted: bool) {
    with_tts_switch(|switch| switch.store(muted, Ordering::SeqCst));
}

pub fn is_tts_muted() -> bool {
    with_tts_switch(|switch| switch.load(Ordering::SeqCst))
}

#[cfg(not(test))]
static MESSAGING_PAUSED: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
thread_local! {
    static MESSAGING_PAUSED: AtomicBool = const { AtomicBool::new(false) };
}

fn with_messaging_switch<T>(read_or_write: impl FnOnce(&AtomicBool) -> T) -> T {
    #[cfg(not(test))]
    return read_or_write(&MESSAGING_PAUSED);
    #[cfg(test)]
    MESSAGING_PAUSED.with(read_or_write)
}

pub fn set_messaging_paused(paused: bool) {
    with_messaging_switch(|switch| switch.store(paused, Ordering::SeqCst));
}

pub fn is_messaging_paused() -> bool {
    with_messaging_switch(|switch| switch.load(Ordering::SeqCst))
}

// ---------------------------------------------------------------------------
// OutboundChannel — notification to human
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
        self.notify_with_switches(turn, trace_id, is_tts_muted(), is_messaging_paused())
    }

    /// Why this channel stays silent under the given switches, if it does. The voice
    /// mute covers only TTS and the messaging pause covers only the away channel.
    fn skip_reason(&self, tts_muted: bool, messaging_paused: bool) -> Option<&'static str> {
        match self {
            OutboundChannel::Tts if tts_muted => Some("TTS notification skipped (voice muted)"),
            OutboundChannel::Away if messaging_paused => {
                Some("away notification skipped (messaging paused)")
            }
            _ => None,
        }
    }

    fn notify_with_switches(
        &self,
        turn: &TurnCompleted,
        trace_id: &str,
        tts_muted: bool,
        messaging_paused: bool,
    ) -> Result<(DeliveryOutcome, Option<AgentAddress>), String> {
        if let Some(reason) = self.skip_reason(tts_muted, messaging_paused) {
            info!("{reason}");
            return Ok((DeliveryOutcome::Skipped, None));
        }
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

/// What a turn-end update and a message an agent asked for read from, and do to, the
/// world outside the decision: the clock, the settings, the screen lock, tmux and the
/// channels. The daemon uses `SystemEnvironment`; tests pass a fake.
pub(crate) trait NotifyEnvironment: Send + Sync {
    fn now(&self) -> Instant;
    fn notify_settings(&self) -> &NotifySettings;
    fn screen_locked(&self) -> bool;
    /// Whether the session containing `pane_id` has an attached client.
    fn session_attached(&self, pane_id: &str) -> bool;
    /// The active pane of the session containing `pane_id`, if a client is attached.
    fn active_pane(&self, pane_id: &str) -> Option<String>;
    /// Send a turn-end update through `channel`.
    fn deliver(
        &self,
        channel: &OutboundChannel,
        turn: &TurnCompleted,
        trace_id: &str,
    ) -> Result<(DeliveryOutcome, Option<AgentAddress>), String>;
    /// Send a message an agent asked for through the away channel.
    fn send_away_message(&self, message: &AwayMessageRequested) -> Result<(), String>;
    /// Make `agent` the one an untagged reply goes to.
    fn record_reply_target(&self, agent: AgentAddress);
}

pub(crate) struct SystemEnvironment;

impl NotifyEnvironment for SystemEnvironment {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn notify_settings(&self) -> &NotifySettings {
        &get_settings().notify
    }

    fn screen_locked(&self) -> bool {
        is_screen_locked()
    }

    fn session_attached(&self, pane_id: &str) -> bool {
        tmux::is_session_attached(pane_id)
    }

    fn active_pane(&self, pane_id: &str) -> Option<String> {
        tmux::active_pane_in_session(pane_id)
    }

    fn deliver(
        &self,
        channel: &OutboundChannel,
        turn: &TurnCompleted,
        trace_id: &str,
    ) -> Result<(DeliveryOutcome, Option<AgentAddress>), String> {
        channel.notify(turn, trace_id)
    }

    fn send_away_message(&self, message: &AwayMessageRequested) -> Result<(), String> {
        channels::send_away_message(message)
    }

    fn record_reply_target(&self, agent: AgentAddress) {
        set_last_away_notification_source_agent(agent);
    }
}

/// What `notify` remembers between events. Kept in memory: a restart forgets it.
#[derive(Default)]
pub(crate) struct NotifyState {
    /// The pane and prompt of the last turn recorded, for the duplicate check.
    last_notify: Mutex<Option<(String, Instant)>>,
    /// For each pane, when it last sent a message it asked Harold to send. The pane's
    /// next `TurnCompleted` removes the entry.
    requested_message_marks: Mutex<HashMap<String, Instant>>,
}

impl NotifyState {
    /// Record that `pane_id` sent a message it asked Harold to send at `now`. A later
    /// message from the same pane replaces the record.
    pub(crate) fn mark_requested_message(&self, pane_id: &str, now: Instant) {
        self.requested_message_marks
            .lock()
            .unwrap()
            .insert(pane_id.to_string(), now);
    }

    fn take_requested_message_mark(&self, pane_id: &str) -> Option<Instant> {
        self.requested_message_marks.lock().unwrap().remove(pane_id)
    }

    fn is_duplicate(&self, dedup_key: &str, now: Instant) -> bool {
        self.last_notify
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|(previous_key, at)| {
                previous_key == dedup_key
                    && now.saturating_duration_since(*at).as_secs() < DEDUP_WINDOW_SECS
            })
    }

    fn record_turn(&self, dedup_key: String, now: Instant) {
        *self.last_notify.lock().unwrap() = Some((dedup_key, now));
    }
}

/// Leave a turn-end update out of the away device: nothing is sent, no reply target is
/// recorded, and the turn is recorded in the duplicate check so that a repeat of it is left
/// out as well.
fn leave_out(
    state: &NotifyState,
    dedup_key: String,
    now: Instant,
    reason: &str,
) -> Result<DeliveryOutcome, String> {
    info!("{reason}");
    state.record_turn(dedup_key, now);
    Ok(DeliveryOutcome::Skipped)
}

pub(crate) fn notify(
    turn: &TurnCompleted,
    trace_id: &str,
    state: &NotifyState,
    environment: &dyn NotifyEnvironment,
) -> Result<DeliveryOutcome, String> {
    let now = environment.now();
    // Removed before any other check, so a mark affects one turn at most.
    let messaged_this_turn = state
        .take_requested_message_mark(&turn.pane_id)
        .is_some_and(|marked_at| left_out::mark_is_fresh(marked_at, now));

    // Dedup: skip if same pane+prompt was notified within the window.
    let dedup_key = format!("{}:{}", turn.pane_id, turn.last_user_prompt);
    if state.is_duplicate(&dedup_key, now) {
        info!("notification skipped (duplicate within dedup window)");
        return Ok(DeliveryOutcome::Skipped);
    }

    let cfg = environment.notify_settings();
    let screen_locked = environment.screen_locked();

    // Session-level skip: if completing pane's session has an attached client AND the
    // screen is not locked, skip entirely.  When the screen is locked the user is away
    // from the desk, so we must still notify even though tmux is attached.
    if cfg.skip_if_session_active && !screen_locked && environment.session_attached(&turn.pane_id) {
        info!("notification skipped (session is active, screen unlocked)");
        return Ok(DeliveryOutcome::Skipped);
    }

    // Pane-level skip: skip only when the completing pane is the active pane
    // AND the screen is not locked (user is at desk looking at it).
    // If screen is locked, always notify even if pane matches.
    if cfg.skip_if_pane_active
        && !screen_locked
        && environment.active_pane(&turn.pane_id).as_deref() == Some(turn.pane_id.as_str())
    {
        info!("notification skipped (pane is active and screen unlocked)");
        return Ok(DeliveryOutcome::Skipped);
    }

    let channel = if screen_locked {
        OutboundChannel::Away
    } else {
        OutboundChannel::Tts
    };

    // The voice mute and the messaging pause skip as they always have, before either rule
    // below is looked at: a paused update would not go to the phone.
    if let Some(reason) = channel.skip_reason(is_tts_muted(), is_messaging_paused()) {
        info!("{reason}");
        return Ok(DeliveryOutcome::Skipped);
    }

    if screen_locked && messaged_this_turn {
        return leave_out(state, dedup_key, now, left_out::MESSAGED_THIS_TURN);
    }

    let (outcome, source_agent) = environment.deliver(&channel, turn, trace_id)?;
    if let Some(source_agent) = source_agent {
        environment.record_reply_target(source_agent);
    }

    if outcome == DeliveryOutcome::Delivered {
        state.record_turn(dedup_key, environment.now());
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

#[cfg(test)]
mod tests {
    use super::*;

    fn turn() -> TurnCompleted {
        TurnCompleted {
            pane_id: "%1".into(),
            pane_label: "label".into(),
            last_user_prompt: "prompt".into(),
            assistant_message: "message".into(),
            main_context: "ctx".into(),
            agent_incarnation: None,
            work_summary: crate::agent::domain::CompletionSummaryUpdate::Unchanged,
        }
    }

    #[test]
    fn muted_tts_channel_skips_without_running_the_command() {
        let outcome = OutboundChannel::Tts.notify_with_switches(&turn(), "trace", true, false);
        assert!(matches!(outcome, Ok((DeliveryOutcome::Skipped, None))));
    }

    #[test]
    fn paused_away_channel_is_skipped_whatever_the_voice_state() {
        // Asserts the decision only. Going through `notify_with_switches` would reach
        // the real away send if this gate were ever removed.
        for tts_muted in [false, true] {
            assert_eq!(
                OutboundChannel::Away.skip_reason(tts_muted, true),
                Some("away notification skipped (messaging paused)")
            );
        }
    }

    #[test]
    fn messaging_pause_silences_the_away_channel_only() {
        assert!(OutboundChannel::Away.skip_reason(false, true).is_some());
        assert!(OutboundChannel::Tts.skip_reason(false, true).is_none());
    }

    #[test]
    fn voice_mute_does_not_silence_the_away_channel() {
        assert!(OutboundChannel::Away.skip_reason(true, false).is_none());
        assert!(OutboundChannel::Tts.skip_reason(false, false).is_none());
    }
}
