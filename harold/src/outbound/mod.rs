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

use tracing::{info, warn};

use crate::channels;
use crate::inbound::ownership::agent_message_text;
use crate::inbound::{AgentAddress, set_last_away_notification_source_agent};
use crate::settings::{Channel, NotifySettings, get_settings};
use crate::store::{AppReplyState, AwayMessageRequested, TurnCompleted};
use crate::tmux;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeliveryOutcome {
    Delivered,
    Skipped,
    /// A channel's list or the chief rule kept it from going out. `reason` is
    /// `not_listed` or `chief`.
    HeldBack {
        reason: &'static str,
    },
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
        is_chief: bool,
    ) -> Result<(DeliveryOutcome, Option<AgentAddress>), String> {
        self.notify_with_switches(
            turn,
            trace_id,
            is_chief,
            is_tts_muted(),
            is_messaging_paused(),
        )
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
        is_chief: bool,
        tts_muted: bool,
        messaging_paused: bool,
    ) -> Result<(DeliveryOutcome, Option<AgentAddress>), String> {
        if let Some(reason) = self.skip_reason(tts_muted, messaging_paused) {
            info!("{reason}");
            return Ok((DeliveryOutcome::Skipped, None));
        }
        match self {
            OutboundChannel::Tts => {
                if tts::notify_at_desk_as(turn, trace_id, is_chief) {
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
        is_chief: bool,
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
    /// Whether `channel` is set up: `imessage` has a recipient, `telegram` a bot token and
    /// a chat id, `web_app` its table. `machine_audio_output` always is.
    fn channel_set_up(&self, channel: Channel) -> bool;
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
        is_chief: bool,
    ) -> Result<(DeliveryOutcome, Option<AgentAddress>), String> {
        channel.notify(turn, trace_id, is_chief)
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

    fn channel_set_up(&self, channel: Channel) -> bool {
        let settings = get_settings();
        match channel {
            Channel::Imessage => settings.imessage.recipient.is_some(),
            Channel::Telegram => {
                settings.telegram.bot_token.is_some() && settings.telegram.chat_id.is_some()
            }
            Channel::WebApp => settings.web_app.is_some(),
            Channel::MachineAudioOutput => true,
        }
    }
}

/// Counts a notification from `pane_id` on `channel` (`sent` or `kept`), only when the
/// channel is set up and turned on.
pub(crate) fn count_notification(
    environment: &dyn NotifyEnvironment,
    agents: &left_out::TrackedAgents,
    pane_id: &str,
    channel: Channel,
    outcome: &str,
) {
    let live = left_out::channel_live(
        environment.notify_settings(),
        environment.channel_set_up(channel),
        channel,
        is_tts_muted(),
        is_messaging_paused(),
    );
    if live {
        crate::telemetry::count_notification(&agents.agent_of(pane_id), channel.key(), outcome);
    }
}

/// Writes the line of something held back, and counts it `kept` when the channel's list
/// held it.
pub(crate) fn report_held(
    environment: &dyn NotifyEnvironment,
    agents: &left_out::TrackedAgents,
    held: &left_out::Held,
    kind: left_out::UpdateKind,
    pane_id: &str,
) {
    held.log(kind, pane_id);
    if held.is_kept() {
        count_notification(environment, agents, pane_id, held.channel, "kept");
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
    /// No push was accepted, for the reason given.
    NotPushed(NotPushedReason),
}

/// Why the web app pushed nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NotPushedReason {
    /// No phone is subscribed.
    NoSubscriptions,
    /// The list of subscriptions could not be read.
    SubscriptionsUnread,
    /// Every push was refused, unreachable or gone.
    NoneAccepted,
}

impl NotPushedReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::NoSubscriptions => "no_subscriptions",
            Self::SubscriptionsUnread => "subscriptions_unread",
            Self::NoneAccepted => "none_accepted",
        }
    }
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
    /// The away channel's name: `imessage` or `telegram`.
    fn channel(&self) -> &'static str;
    fn summarise(&self, turn: &TurnCompleted) -> String;
    fn send(&self, text: &str) -> Result<(), String>;
}

pub(crate) struct SystemAwayText;

impl AwayText for SystemAwayText {
    fn machine_name(&self) -> String {
        get_settings().machine_name.clone()
    }

    fn channel(&self) -> &'static str {
        get_settings().notify.away().key()
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
    /// The app could not push it and the pane is not on the away channel's list, so it
    /// went nowhere.
    Held,
}

/// The app route for one delivery of a chief's message: the web app, and the away
/// channel's text send for when no push is accepted.
pub(crate) struct ChiefApp<'a> {
    pub app: &'a dyn AppRoute,
    pub away: &'a dyn AwayText,
    /// Whether the pane may use the away channel when the app cannot push.
    pub away_allowed: bool,
}

/// The app route: deliver to the app; when not pushed, send "(app unreachable) <away_text>" once
/// if the pane is on the away channel's list, else hold the message back (`CarriedBy::Held`).
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
        AppDelivery::NotPushed(reason) if !chief_app.away_allowed => {
            warn!(
                reason = reason.as_str(),
                outcome = "held",
                "app message not pushed"
            );
            Ok(CarriedBy::Held)
        }
        AppDelivery::NotPushed(reason) => {
            warn!(
                reason = reason.as_str(),
                outcome = "away",
                channel = chief_app.away.channel(),
                "app message not pushed"
            );
            chief_app
                .away
                .send(&format!("{APP_UNREACHABLE} {away_text}"))?;
            Ok(CarriedBy::Away)
        }
    }
}

/// The `channel` attribute of a turn-end update the web app carried: the `[notify]` key
/// `web_app`.
const APP_CHANNEL: &str = "web_app";

/// Send a locked-screen turn-end update of a chief's pane by the app route. The update is
/// the turn's summary; iMessage gets it with the pane's address, as a requested message.
/// The source agent is returned only when iMessage carried it.
fn deliver_turn_from_chief(
    chief_app: &ChiefApp<'_>,
    turn: &TurnCompleted,
    event_id: &str,
) -> Result<(CarriedBy, Option<AgentAddress>), String> {
    let text = chief_app.away.summarise(turn);
    // The app gets the whole text; the away channel gets it cut at a sentence end.
    let fitted = channels::fit_turn_end_text(&text, channels::TURN_END_TEXT_LIMIT);
    let away_text = agent_message_text(&chief_app.away.machine_name(), &turn.pane_label, &fitted);
    let carried_by = deliver_from_chief(chief_app, event_id, &text, &away_text)?;
    match carried_by {
        CarriedBy::App => channels::record_text_cut(false),
        CarriedBy::Away => {
            let cut = fitted != text;
            if cut {
                info!(
                    channel = chief_app.away.channel(),
                    pane_id = turn.pane_id.as_str(),
                    count = text.chars().count(),
                    "turn-end update cut"
                );
            }
            channels::record_text_cut(cut);
        }
        CarriedBy::Held => {}
    }
    let source = match carried_by {
        CarriedBy::App | CarriedBy::Held => None,
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

/// The `channel` attribute of a turn-end update sent through `channel`, the `[notify]` key:
/// `machine_audio_output`, or the configured away channel, `imessage` or `telegram`.
fn channel_name(channel: OutboundChannel, away_channel: &str) -> &'static str {
    match channel {
        OutboundChannel::Tts => "machine_audio_output",
        OutboundChannel::Away if away_channel == "telegram" => "telegram",
        OutboundChannel::Away => "imessage",
    }
}

/// Whether `pane_id` is the chief's pane, found as `notify.chief` finds it, so the update
/// names `the chief` and not `the <project> agent`.
fn is_chief_pane(cfg: &NotifySettings, agents: &left_out::TrackedAgents, pane_id: &str) -> bool {
    agents.chief_pane_id(cfg.chief.as_deref(), cfg.chief_session.as_deref()) == Some(pane_id)
}

/// What a hold makes of the delivery: held back by a rule, or skipped by the user's own
/// choice.
fn outcome_of(held: &left_out::Held) -> DeliveryOutcome {
    if held.is_held_back() {
        DeliveryOutcome::HeldBack {
            reason: held.reason,
        }
    } else {
        DeliveryOutcome::Skipped
    }
}

/// Leave a turn-end update out: nothing is sent or said, no reply target is recorded,
/// and the turn is recorded in the duplicate check so that a repeat of it is left
/// out as well. The log line names the pane and the agent. The current span's `channel`
/// is the channel whose rule held it.
fn leave_out(
    state: &NotifyState,
    environment: &dyn NotifyEnvironment,
    agents: &left_out::TrackedAgents,
    dedup_key: String,
    now: Instant,
    pane_id: &str,
    held: &left_out::Held,
) -> Result<DeliveryOutcome, String> {
    report_held(
        environment,
        agents,
        held,
        left_out::UpdateKind::TurnEndUpdate,
        pane_id,
    );
    tracing::Span::current().record("channel", held.channel.key());
    if !held.is_held_back() {
        record_skip_reason(held.reason);
    }
    state.record_turn(dedup_key, now);
    Ok(outcome_of(held))
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
/// The current span's `channel` is `web_app` when the app carried the update, and the away
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

    let cfg = environment.notify_settings();
    let screen_locked = environment.screen_locked();
    let channel = if screen_locked {
        OutboundChannel::Away
    } else {
        OutboundChannel::Tts
    };
    let app_route = match (channel, app) {
        (OutboundChannel::Away, Some(chief_app)) => Some(chief_app),
        _ => None,
    };
    let name = channel_name(channel, &cfg.away_channel);

    // Dedup: skip if same pane+prompt was notified within the window.
    let dedup_key = format!("{}:{}", turn.pane_id, turn.last_user_prompt);
    if state.is_duplicate(&dedup_key, now) {
        record_delivery_channel(app_route.is_some(), app_route_channel, name);
        record_skip_reason("duplicate_turn");
        info!("notification skipped (duplicate within dedup window)");
        return Ok(DeliveryOutcome::Skipped);
    }

    // Session-level skip: if completing pane's session has an attached client AND the
    // screen is not locked, skip entirely.  When the screen is locked the user is away
    // from the desk, so we must still notify even though tmux is attached.
    if cfg.skip_if_session_active && !screen_locked && environment.session_attached(&turn.pane_id) {
        record_delivery_channel(app_route.is_some(), app_route_channel, name);
        record_skip_reason("session_active");
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
        record_delivery_channel(app_route.is_some(), app_route_channel, name);
        record_skip_reason("pane_active");
        info!("notification skipped (pane is active and screen unlocked)");
        return Ok(DeliveryOutcome::Skipped);
    }

    // The voice mute and the messaging pause skip as they always have, before any rule
    // below is looked at: a paused update would not go to the phone.
    if let Some(reason) = channel.skip_reason(is_tts_muted(), is_messaging_paused()) {
        record_delivery_channel(app_route.is_some(), app_route_channel, name);
        record_skip_reason(match channel {
            OutboundChannel::Tts => "voice_mute",
            OutboundChannel::Away => "messaging_pause",
        });
        info!("{reason}");
        return Ok(DeliveryOutcome::Skipped);
    }

    // A held-back update records the channel whose rule held it, once, in `leave_out`.
    let held = if screen_locked {
        left_out::left_out_reason(
            messaged_this_turn,
            cfg,
            agents,
            app_route.is_some(),
            &turn.pane_id,
        )
    } else {
        left_out::desk_update_reason(cfg, agents, &turn.pane_id)
    };
    if let Some(held) = held {
        return leave_out(
            state,
            environment,
            agents,
            dedup_key,
            now,
            &turn.pane_id,
            &held,
        );
    }

    // Recorded on `harold.delivery` when the update is delivered inside it. The app route
    // records it on return, once the route that carried the update is known.
    record_delivery_channel(app_route.is_some(), app_route_channel, name);

    // The channel that took the update, for `harold.notifications`.
    let mut sent_on = match channel {
        OutboundChannel::Tts => Channel::MachineAudioOutput,
        OutboundChannel::Away => cfg.away(),
    };
    let delivered = match app_route {
        Some(chief_app) => {
            deliver_turn_from_chief(chief_app, turn, event_id).map(|(carried_by, source)| {
                match carried_by {
                    CarriedBy::App => {
                        *app_route_channel = Some(APP_CHANNEL);
                        sent_on = Channel::WebApp;
                    }
                    CarriedBy::Held => {
                        let held = left_out::Held::not_listed(
                            left_out::notification_skipped(cfg.away().key()),
                            cfg.away(),
                            agents.agent_of(&turn.pane_id),
                        );
                        report_held(
                            environment,
                            agents,
                            &held,
                            left_out::UpdateKind::TurnEndUpdate,
                            &turn.pane_id,
                        );
                        *app_route_channel = Some(held.channel.key());
                        return (outcome_of(&held), source);
                    }
                    CarriedBy::Away => {}
                }
                (DeliveryOutcome::Delivered, source)
            })
        }
        None => environment.deliver(
            &channel,
            turn,
            trace_id,
            is_chief_pane(cfg, agents, &turn.pane_id),
        ),
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
        count_notification(environment, agents, &turn.pane_id, sent_on, "sent");
        state.record_turn(dedup_key, environment.now());
    } else if outcome == DeliveryOutcome::Skipped {
        // The away channel sent the same text a moment ago.
        record_skip_reason("duplicate_message");
    }
    Ok(outcome)
}

/// Records why the current `harold.delivery` span's update was skipped.
fn record_skip_reason(reason: &'static str) {
    tracing::Span::current().record("reason", reason);
}

/// Records the `channel` of the delivery span now, or leaves it to the app route to record
/// on return.
fn record_delivery_channel(
    on_app_route: bool,
    app_route_channel: &mut Option<&'static str>,
    name: &'static str,
) {
    if on_app_route {
        *app_route_channel = Some(name);
    } else {
        tracing::Span::current().record("channel", name);
    }
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
        let outcome =
            OutboundChannel::Tts.notify_with_switches(&turn(), "trace", false, true, false);
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
