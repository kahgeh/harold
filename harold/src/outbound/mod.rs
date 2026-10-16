pub mod tts;

pub(crate) mod left_out;
pub(crate) mod spoken_line;
pub(crate) mod voice_note;

#[cfg(test)]
pub(crate) mod fake;
#[cfg(test)]
pub(crate) mod notify_tests;

use std::collections::HashMap;
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use tracing::info;

use crate::channels;
use crate::inbound::ownership::agent_message_text;
use crate::inbound::{AgentAddress, set_last_away_notification_source_agent};
use crate::settings::{NotifySettings, get_settings};
use crate::store::{AppReplyState, AwayMessageRequested, TurnCompleted};
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
    /// Say a spoken line at the desk with the `[tts]` command, adding nothing to it.
    fn speak_line(&self, line: &str) -> Result<(), String>;
    /// Send a spoken line to the away device: a voice note with iMessage, text with
    /// Telegram.
    fn send_spoken_line(&self, line: &str) -> Result<(), String>;
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

    fn speak_line(&self, line: &str) -> Result<(), String> {
        if tts::speak_line(line) {
            Ok(())
        } else {
            Err("TTS command failed".into())
        }
    }

    fn send_spoken_line(&self, line: &str) -> Result<(), String> {
        channels::send_spoken_line(line)
    }

    fn record_reply_target(&self, agent: AgentAddress) {
        set_last_away_notification_source_agent(agent);
    }
}

// ---------------------------------------------------------------------------
// The app route — the chief's messages go to the web app
// ---------------------------------------------------------------------------

/// Whether the web app pushed a message to the phone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AppDelivery {
    /// At least one push service accepted the alert.
    Pushed,
    /// No phone is subscribed, or no push service accepted the alert.
    NotPushed,
}

/// The web app as the delivery loop sees it. The daemon uses `web_app::route::WebAppRoute`;
/// tests pass a fake.
pub(crate) trait AppRoute: Send + Sync {
    /// Adds the chief's `text` to the thread under `event_id` (once, however often called)
    /// and pushes its alert body to every subscription. Err only when the thread write fails.
    fn deliver(&self, event_id: &str, text: &str) -> Result<AppDelivery, String>;
    fn set_reply_state(&self, event_id: &str, state: AppReplyState) -> Result<(), String>;
}

/// The away channel's plain-text send and the turn-end summary. `SystemAwayText` uses
/// `channels::send` and `channels::summarise_for_notification`; tests pass a fake.
pub(crate) trait AwayText: Send + Sync {
    fn machine_name(&self) -> String;
    fn summarise(&self, turn: &TurnCompleted) -> String;
    fn send(&self, text: &str) -> Result<(), String>;
}

pub(crate) struct SystemAwayText;

impl AwayText for SystemAwayText {
    fn machine_name(&self) -> String {
        get_settings().machine_name.clone()
    }

    fn summarise(&self, turn: &TurnCompleted) -> String {
        channels::summarise_for_notification(&turn.assistant_message, &turn.last_user_prompt)
    }

    fn send(&self, text: &str) -> Result<(), String> {
        channels::send(text)
    }
}

/// What a message the app could not push starts with when it goes to iMessage instead.
pub(crate) const APP_UNREACHABLE: &str = "(app unreachable)";

/// Which route carried a chief's message to the phone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CarriedBy {
    App,
    Away,
}

/// The app route for one delivery of a chief's message: the web app, and the away
/// channel's text send for when no push is accepted.
pub(crate) struct ChiefApp<'a> {
    pub app: &'a dyn AppRoute,
    pub away: &'a dyn AwayText,
}

/// The app route: deliver to the app; when not pushed, send "(app unreachable) <away_text>" once.
///
/// An error means nothing reached the phone this try, and the caller retries: a failed
/// thread write sends nothing, and a failed away send follows a push no service accepted.
/// The thread keeps one row per `event_id`, so a retry adds no second one.
pub(crate) fn deliver_from_chief(
    chief_app: &ChiefApp<'_>,
    event_id: &str,
    text: &str,
    away_text: &str,
) -> Result<CarriedBy, String> {
    match chief_app.app.deliver(event_id, text)? {
        AppDelivery::Pushed => Ok(CarriedBy::App),
        AppDelivery::NotPushed => {
            chief_app
                .away
                .send(&format!("{APP_UNREACHABLE} {away_text}"))?;
            Ok(CarriedBy::Away)
        }
    }
}

/// The `channel` attribute of a turn-end update the web app carried.
const APP_CHANNEL: &str = "app";

/// Send a locked-screen turn-end update of a chief's pane by the app route. The update is
/// the turn's summary; iMessage gets it with the pane's address, as a requested message.
/// The source agent is returned only when iMessage carried it.
fn deliver_turn_from_chief(
    chief_app: &ChiefApp<'_>,
    turn: &TurnCompleted,
    event_id: &str,
) -> Result<(CarriedBy, Option<AgentAddress>), String> {
    let text = chief_app.away.summarise(turn);
    let away_text = agent_message_text(&chief_app.away.machine_name(), &turn.pane_label, &text);
    let carried_by = deliver_from_chief(chief_app, event_id, &text, &away_text)?;
    let source = match carried_by {
        CarriedBy::App => None,
        CarriedBy::Away => Some(AgentAddress::TmuxPane {
            pane_id: turn.pane_id.clone(),
            label: turn.pane_label.clone(),
        }),
    };
    Ok((carried_by, source))
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

    /// Put back a mark `notify` removed for a turn whose delivery then failed, with the
    /// time it was set, so that the retry of the same turn finds it.
    fn restore_requested_message_mark(&self, pane_id: &str, marked_at: Instant) {
        self.requested_message_marks
            .lock()
            .unwrap()
            .entry(pane_id.to_string())
            .or_insert(marked_at);
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

/// The `channel` attribute of a turn-end update sent through `channel`: `tts`, or the
/// configured away channel, `imessage` or `telegram`.
fn channel_name(channel: OutboundChannel, away_channel: &str) -> &'static str {
    match channel {
        OutboundChannel::Tts => "tts",
        OutboundChannel::Away if away_channel == "telegram" => "telegram",
        OutboundChannel::Away => "imessage",
    }
}

/// Leave a turn-end update out: nothing is sent or said, no reply target is recorded,
/// and the turn is recorded in the duplicate check so that a repeat of it is left
/// out as well. The log line names the pane.
fn leave_out(
    state: &NotifyState,
    dedup_key: String,
    now: Instant,
    pane_id: &str,
    reason: &str,
) -> Result<DeliveryOutcome, String> {
    info!(pane_id = %pane_id, "{reason}");
    state.record_turn(dedup_key, now);
    Ok(DeliveryOutcome::Skipped)
}

/// The daemon sends through `notify_with_app`; this is its form without the app route.
#[cfg(test)]
pub(crate) fn notify(
    turn: &TurnCompleted,
    trace_id: &str,
    state: &NotifyState,
    agents: &left_out::TrackedAgents,
    environment: &dyn NotifyEnvironment,
) -> Result<DeliveryOutcome, String> {
    notify_with_app(turn, trace_id, trace_id, state, agents, environment, None)
}

/// `notify` with the app route for a chief's pane. `notify` calls it with `None`.
///
/// `app` is given only for a chief's pane. It changes the send alone: where `notify` would
/// send the update on `OutboundChannel::Away`, the update goes by `deliver_from_chief`
/// under `event_id`. Every check before the send is as in `notify`.
///
/// The current span's `channel` is `app` when the app carried the update, and the away
/// channel's name otherwise. A span keeps every value recorded for a field, so on the app
/// route the channel is recorded once, on return, when it is known.
pub(crate) fn notify_with_app(
    turn: &TurnCompleted,
    event_id: &str,
    trace_id: &str,
    state: &NotifyState,
    agents: &left_out::TrackedAgents,
    environment: &dyn NotifyEnvironment,
    app: Option<&ChiefApp<'_>>,
) -> Result<DeliveryOutcome, String> {
    let mut app_route_channel = None;
    let notified = notify_routed(
        turn,
        event_id,
        trace_id,
        state,
        agents,
        environment,
        app,
        &mut app_route_channel,
    );
    if let Some(channel) = app_route_channel {
        tracing::Span::current().record("channel", channel);
    }
    notified
}

/// The body of `notify_with_app`. On the app route it leaves the `channel` to record in
/// `app_route_channel` instead of recording it.
#[allow(clippy::too_many_arguments)]
fn notify_routed(
    turn: &TurnCompleted,
    event_id: &str,
    trace_id: &str,
    state: &NotifyState,
    agents: &left_out::TrackedAgents,
    environment: &dyn NotifyEnvironment,
    app: Option<&ChiefApp<'_>>,
    app_route_channel: &mut Option<&'static str>,
) -> Result<DeliveryOutcome, String> {
    let now = environment.now();
    // Removed before any other check, so a mark affects one turn at most.
    let mark = state.take_requested_message_mark(&turn.pane_id);
    let messaged_this_turn = mark.is_some_and(|marked_at| left_out::mark_is_fresh(marked_at, now));

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
    // Recorded on `harold.delivery` when the update is delivered inside it. The app route
    // records it on return, once the route that carried the update is known.
    let app_route = match (channel, app) {
        (OutboundChannel::Away, Some(chief_app)) => Some(chief_app),
        _ => None,
    };
    let name = channel_name(channel, &cfg.away_channel);
    match app_route {
        Some(_) => *app_route_channel = Some(name),
        None => {
            tracing::Span::current().record("channel", name);
        }
    }

    // The voice mute and the messaging pause skip as they always have, before any rule
    // below is looked at: a paused update would not go to the phone.
    if let Some(reason) = channel.skip_reason(is_tts_muted(), is_messaging_paused()) {
        info!("{reason}");
        return Ok(DeliveryOutcome::Skipped);
    }

    if screen_locked
        && let Some(reason) = left_out::left_out_reason(
            messaged_this_turn,
            cfg.away_messages_only_from.as_deref(),
            agents,
            &turn.pane_id,
        )
    {
        return leave_out(state, dedup_key, now, &turn.pane_id, &reason);
    }

    if !screen_locked
        && let Some(reason) = left_out::quiet_reason(
            cfg.away_messages_only_from.as_deref(),
            agents,
            &turn.pane_id,
        )
    {
        return leave_out(state, dedup_key, now, &turn.pane_id, &reason);
    }

    let delivered = match app_route {
        Some(chief_app) => {
            deliver_turn_from_chief(chief_app, turn, event_id).map(|(carried_by, source)| {
                if carried_by == CarriedBy::App {
                    *app_route_channel = Some(APP_CHANNEL);
                }
                (DeliveryOutcome::Delivered, source)
            })
        }
        None => environment.deliver(&channel, turn, trace_id),
    };
    let (outcome, source_agent) = match delivered {
        Ok(delivered) => delivered,
        Err(error) => {
            // The event stays pending and this turn comes again: its mark must still be
            // there then, or a retry made after the screen is locked would go to the phone.
            if let Some(marked_at) = mark {
                state.restore_requested_message_mark(&turn.pane_id, marked_at);
            }
            return Err(error);
        }
    };
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
