use std::cell::Cell;
use std::sync::Arc;
use std::time::Duration;

use time::OffsetDateTime;
use tokio::sync::watch;
use tracing::{info, info_span, warn};
use tracing_opentelemetry::OpenTelemetrySpanExt as _;

use crate::agent::snapshot::AgentSnapshotHub;
use crate::alerts::AlertQueue;
use crate::inbound::ownership::agent_message_text;
use crate::inbound::{
    AgentAddress, ChiefRouting, RouteReport, route_inbound_message, route_to_chief,
    type_into_pane_quietly,
};
use crate::outbound::left_out::{
    self, TrackedAgents, agent_message_dropped, desk_line_dropped_reason, message_dropped_reason,
    spoken_line_dropped, spoken_line_dropped_reason,
};
use crate::outbound::{
    AppRoute, AwayText, CarriedBy, ChiefApp, DeliveryOutcome, NotifyEnvironment, NotifyState,
    SystemAwayText, SystemEnvironment, deliver_from_chief, notify_with_app,
};
use crate::settings::Channel;
use crate::store::{
    AppReplyState, AwayMessageRequested, EventVersion, HaroldStore, InboundMessage,
    PendingDelivery, ProjectionBatch, ReceivedReply, SpokenLineRequested, StoreError,
    TurnCompleted,
};
use crate::telemetry::{self, OpenReply, ResolvedBy, Routing, TRACE_TARGET};
use crate::voice_input::Speaker;
use crate::web_app::chief::{ChiefPane, find_chief, is_chief_pane};

pub(crate) trait DeliveryDispatcher: Send + Sync + 'static {
    fn dispatch(&self, delivery: &PendingDelivery) -> Result<DeliveryOutcome, DispatchError>;

    /// A stored phone reply, with the trace and time its stored event records. A
    /// dispatcher that does not count phone requests handles it as any other delivery.
    fn dispatch_reply(
        &self,
        delivery: &PendingDelivery,
        _origin: &ReplyOrigin,
    ) -> Result<DeliveryOutcome, DispatchError> {
        self.dispatch(delivery)
    }
}

/// Where a stored reply's phone request comes from: the trace its event recorded, if
/// any, and when the event was stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReplyOrigin {
    pub trace_id: Option<String>,
    pub created_at: OffsetDateTime,
}

/// Routes a stored reply's text, given whether ownership is already decided.
pub(crate) type Router = Arc<dyn Fn(&str, bool) -> Result<RouteReport, String> + Send + Sync>;

/// Types a text into a pane, given the pane id and the text, and presses Enter.
pub(crate) type ChiefTyper = Arc<dyn Fn(&str, &str) -> Result<(), String> + Send + Sync>;

#[derive(Debug, thiserror::Error)]
pub(crate) enum DispatchError {
    #[error("{0}")]
    Retryable(String),

    #[error("{0}")]
    Permanent(String),
}

struct ProductionDispatcher {
    environment: Arc<dyn NotifyEnvironment>,
    notify_state: NotifyState,
    /// Where the tracked agent panes are read from. Without it no agent is seen, so the
    /// folder rule leaves no update out and drops no message.
    snapshots: Option<AgentSnapshotHub>,
    /// Routes a stored reply: `route_inbound_message`, or a fake in tests.
    router: Router,
    /// The web app. When set, the chief's messages to the phone go to it first.
    app: Option<Arc<dyn AppRoute>>,
    /// The away channel's plain-text send, for a chief's message the app did not push.
    away_text: Arc<dyn AwayText>,
    /// Types a reply from the web app into the chief's pane: `type_into_pane_quietly`, or
    /// a fake in tests.
    chief_typer: ChiefTyper,
    /// Where Harold queues its own alert lines, when the alert address is on.
    alerts: Option<AlertQueue>,
}

impl DeliveryDispatcher for ProductionDispatcher {
    fn dispatch(&self, delivery: &PendingDelivery) -> Result<DeliveryOutcome, DispatchError> {
        self.dispatch_with_pause(delivery, None, crate::outbound::is_messaging_paused())
    }

    fn dispatch_reply(
        &self,
        delivery: &PendingDelivery,
        origin: &ReplyOrigin,
    ) -> Result<DeliveryOutcome, DispatchError> {
        self.dispatch_with_pause(
            delivery,
            Some(origin),
            crate::outbound::is_messaging_paused(),
        )
    }
}

impl ProductionDispatcher {
    fn new(environment: Arc<dyn NotifyEnvironment>, snapshots: Option<AgentSnapshotHub>) -> Self {
        Self {
            environment,
            notify_state: NotifyState::default(),
            snapshots,
            router: Arc::new(route_inbound_message),
            app: None,
            away_text: Arc::new(SystemAwayText),
            chief_typer: Arc::new(type_into_pane_quietly),
            alerts: None,
        }
    }

    /// The dispatcher queueing `chief-message-held-back` into `alerts`.
    fn with_alerts(self, alerts: Option<AlertQueue>) -> Self {
        Self { alerts, ..self }
    }

    /// Counts something an agent message or spoken line a list held back as `kept`, and
    /// tells the chief when it was the chief's own and the channel is set up and turned on.
    /// The line is written by the dispatch that drops it.
    fn note_held(
        &self,
        agents: &TrackedAgents,
        held: &left_out::Held,
        pane_id: &str,
        voice_muted: bool,
    ) {
        if !held.is_kept() {
            return;
        }
        let settings = self.environment.notify_settings();
        let live = left_out::channel_live(
            settings,
            self.environment.channel_set_up(held.channel),
            held.channel,
            voice_muted,
            crate::outbound::is_messaging_paused(),
        );
        if !live {
            return;
        }
        crate::telemetry::count_notification(&held.agent, held.channel.key(), "kept");
        if let Some(alerts) = &self.alerts
            && agents.is_in_chief_folder(settings.chief.as_deref(), pane_id)
        {
            alerts.firing(crate::alerts::CHIEF_MESSAGE_HELD_BACK);
        }
    }

    /// Counts a message or line from `pane_id` that `channel` took, as `sent`.
    fn note_sent(&self, pane_id: &str, channel: Channel) {
        crate::outbound::count_notification(
            self.environment.as_ref(),
            &self.tracked_agents(),
            pane_id,
            channel,
            "sent",
        );
    }

    /// The dispatcher with `typer` in place of typing into the chief's tmux pane.
    #[cfg(test)]
    fn with_chief_typer(self, typer: ChiefTyper) -> Self {
        Self {
            chief_typer: typer,
            ..self
        }
    }

    /// The dispatcher with `router` in place of tmux routing.
    #[cfg(test)]
    fn with_router(self, router: Router) -> Self {
        Self { router, ..self }
    }

    /// The dispatcher with the web app as the chief's route to the phone.
    fn with_app(self, app: Arc<dyn AppRoute>) -> Self {
        Self {
            app: Some(app),
            ..self
        }
    }

    /// The dispatcher with `away` in place of the away channel's text send.
    #[cfg(test)]
    fn with_away_text(self, away: Arc<dyn AwayText>) -> Self {
        Self {
            away_text: away,
            ..self
        }
    }

    /// The app route for a message from `pane_id`: some only when a web app is set and
    /// the pane is the chief's in the snapshot the handler published before this delivery.
    fn chief_app(&self, pane_id: &str) -> Option<ChiefApp<'_>> {
        let app = self.app.as_deref()?;
        let snapshots = self.snapshots.as_ref()?;
        let settings = self.environment.notify_settings();
        let current = snapshots.current();
        let agents = TrackedAgents::from_snapshot(&current);
        let on_app = is_chief_pane(&current, settings.chief.as_deref(), pane_id)
            && agents.may_use(settings, Channel::WebApp, pane_id);
        on_app.then(|| ChiefApp {
            app,
            away: self.away_text.as_ref(),
            away_allowed: agents.may_use(settings, settings.away(), pane_id),
        })
    }

    /// The chief's pane in the snapshot the handler published before this delivery. With
    /// no hub, no agent is seen.
    fn chief_pane(&self) -> ChiefPane {
        let Some(snapshots) = self.snapshots.as_ref() else {
            return ChiefPane::NoChief;
        };
        let chief_folder = self.environment.notify_settings().chief.as_deref();
        find_chief(&snapshots.current(), chief_folder)
    }

    /// Shows `state` on the app's row for the reply `event_id` stored, when a web app is
    /// set. A failed write is logged and not retried: the reply may have been typed, and a
    /// retry would type it twice.
    fn set_reply_state(&self, event_id: &str, state: AppReplyState) {
        let Some(app) = self.app.as_deref() else {
            return;
        };
        if let Err(error) = app.set_reply_state(event_id, state) {
            warn!(event_id, error, "app reply state not recorded");
        }
    }

    /// A stored reply. One the user typed in the web app (`to_chief`) is typed into the
    /// chief's pane, and its row in the app shows what became of it; any other reply is
    /// routed as an iMessage reply is.
    fn dispatch_inbound(
        &self,
        delivery: &PendingDelivery,
        origin: Option<&ReplyOrigin>,
        messaging_paused: bool,
    ) -> Result<DeliveryOutcome, DispatchError> {
        if messaging_paused {
            // A backlog queued before the pause drains without being routed or confirmed.
            // Only a reply for the chief is read, so that its row says it was not sent.
            let for_chief = serde_json::from_value::<ReceivedReply>(delivery.payload.clone())
                .is_ok_and(|reply| reply.to_chief);
            if for_chief {
                self.set_reply_state(&delivery.event_id, AppReplyState::NotSentPaused);
            } else {
                finish_paused_request(origin);
            }
            return Ok(DeliveryOutcome::Skipped);
        }
        let reply =
            serde_json::from_value::<ReceivedReply>(delivery.payload.clone()).map_err(|error| {
                DispatchError::Permanent(format!("invalid InboundMessageReceived payload: {error}"))
            })?;
        if !reply.to_chief {
            let message = InboundMessage {
                text: reply.text,
                forwarded: reply.forwarded,
            };
            return route_message(&message, &delivery.event_id, origin, |text, forwarded| {
                (self.router)(text, forwarded)
            });
        }
        // The reply's text is never logged: an error names the listing or tmux only.
        let routing = route_to_chief(&reply.text, &self.chief_pane(), self.chief_typer.as_ref())
            .map_err(DispatchError::Retryable)?;
        let (state, outcome) = match routing {
            ChiefRouting::Typed { busy: false, .. } => {
                (AppReplyState::Typed, DeliveryOutcome::Delivered)
            }
            ChiefRouting::Typed { busy: true, .. } => {
                (AppReplyState::WaitingChiefBusy, DeliveryOutcome::Delivered)
            }
            ChiefRouting::NoChief => (AppReplyState::NotSentNoChief, DeliveryOutcome::Skipped),
        };
        self.set_reply_state(&delivery.event_id, state);
        Ok(outcome)
    }

    /// The agent panes in the snapshot the handler published before this delivery.
    fn tracked_agents(&self) -> TrackedAgents {
        self.snapshots
            .as_ref()
            .map(|snapshots| TrackedAgents::from_snapshot(&snapshots.current()))
            .unwrap_or_default()
    }

    /// `origin` is where a stored reply's phone request comes from. Without it, the
    /// reply's request starts now.
    fn dispatch_with_pause(
        &self,
        delivery: &PendingDelivery,
        origin: Option<&ReplyOrigin>,
        messaging_paused: bool,
    ) -> Result<DeliveryOutcome, DispatchError> {
        match delivery.event_type.as_str() {
            "TurnCompleted" => {
                let turn = serde_json::from_value::<TurnCompleted>(delivery.payload.clone())
                    .map_err(|error| {
                        DispatchError::Permanent(format!("invalid TurnCompleted payload: {error}"))
                    })?;
                notify_with_app(
                    &turn,
                    &delivery.event_id,
                    &delivery.trace_id,
                    &self.notify_state,
                    &self.tracked_agents(),
                    self.environment.as_ref(),
                    self.chief_app(&turn.pane_id).as_ref(),
                )
                .map_err(DispatchError::Retryable)
            }
            "InboundMessageReceived" => self.dispatch_inbound(delivery, origin, messaging_paused),
            "AwayMessageRequested" => {
                // Whether the web app carried the message, in which case no reply target
                // is recorded: a reply to it comes through the app.
                let carried_by_app = Cell::new(false);
                // Whether the app could not push the message and the pane is not on the
                // away channel's list, so it went nowhere.
                let held = Cell::new(false);
                let outcome = dispatch_away_message(
                    delivery,
                    messaging_paused,
                    |pane_id| {
                        let agents = self.tracked_agents();
                        let held = message_dropped_reason(
                            self.environment.notify_settings(),
                            &agents,
                            self.chief_app(pane_id).is_some(),
                            pane_id,
                        )?;
                        self.note_held(&agents, &held, pane_id, crate::outbound::is_tts_muted());
                        Some(held)
                    },
                    |message| {
                        let settings = self.environment.notify_settings();
                        let Some(chief_app) = self.chief_app(&message.pane_id) else {
                            self.environment.send_away_message(message)?;
                            self.note_sent(&message.pane_id, settings.away());
                            return Ok(());
                        };
                        let away_text = agent_message_text(
                            &chief_app.away.machine_name(),
                            &message.pane_label,
                            &message.text,
                        );
                        let carried_by = deliver_from_chief(
                            &chief_app,
                            &delivery.event_id,
                            &message.text,
                            &away_text,
                        )?;
                        carried_by_app.set(carried_by == CarriedBy::App);
                        match carried_by {
                            CarriedBy::App => self.note_sent(&message.pane_id, Channel::WebApp),
                            CarriedBy::Away => self.note_sent(&message.pane_id, settings.away()),
                            CarriedBy::Held => {
                                held.set(true);
                                let agents = self.tracked_agents();
                                let dropped = left_out::Held::not_listed(
                                    agent_message_dropped(settings.away().key()),
                                    settings.away(),
                                    agents.agent_of(&message.pane_id),
                                );
                                dropped.log(left_out::UpdateKind::AgentMessage, &message.pane_id);
                                self.note_held(
                                    &agents,
                                    &dropped,
                                    &message.pane_id,
                                    crate::outbound::is_tts_muted(),
                                );
                            }
                        }
                        Ok(())
                    },
                    |source| {
                        // A message held back sent nothing: no mark and no reply target.
                        if held.get() {
                            return;
                        }
                        // The pane's mark: its turn-end update for this turn is left out.
                        self.notify_state
                            .mark_requested_message(source.pane_id(), self.environment.now());
                        if !carried_by_app.get() {
                            self.environment.record_reply_target(source);
                        }
                    },
                );
                // A message held back was dropped, not delivered.
                match outcome {
                    Ok(DeliveryOutcome::Delivered) if held.get() => Ok(DeliveryOutcome::Skipped),
                    other => other,
                }
            }
            "SpokenLineRequested" => dispatch_spoken_line_with_app(
                delivery,
                messaging_paused,
                crate::outbound::is_tts_muted(),
                |pane_id| {
                    let agents = self.tracked_agents();
                    let held = spoken_line_dropped_reason(
                        self.environment.notify_settings(),
                        &agents,
                        self.chief_app(pane_id).is_some(),
                        pane_id,
                    )?;
                    self.note_held(&agents, &held, pane_id, false);
                    Some(held)
                },
                |pane_id| {
                    let agents = self.tracked_agents();
                    let held = desk_line_dropped_reason(
                        self.environment.notify_settings(),
                        &agents,
                        pane_id,
                    )?;
                    self.note_held(&agents, &held, pane_id, false);
                    Some(held)
                },
                self.environment.as_ref(),
                |pane_id| self.chief_app(pane_id),
                |pane_id, text| {
                    // The phone's call is the web app's.
                    let on_app = self.tracked_agents().may_use(
                        self.environment.notify_settings(),
                        Channel::WebApp,
                        pane_id,
                    );
                    if on_app {
                        crate::voice_input::say_in_phone_call(text, Speaker::Chief)
                    } else {
                        None
                    }
                },
                |pane_id, channel| self.note_sent(pane_id, channel),
                |pane_id| {
                    let agents = self.tracked_agents();
                    let away = self.environment.notify_settings().away();
                    let held = left_out::Held::not_listed(
                        spoken_line_dropped(away.key()),
                        away,
                        agents.agent_of(pane_id),
                    );
                    self.note_held(&agents, &held, pane_id, false);
                    held
                },
            ),
            other => Err(DispatchError::Permanent(format!(
                "unknown event type: {other}"
            ))),
        }
    }
}

/// Hand a stored reply to `route` together with whether the hub forwarded it, inside
/// `harold.route`, and finish its phone request with what routing did. A failed route is
/// retried later, so its request stays open.
fn route_message(
    message: &InboundMessage,
    event_id: &str,
    origin: Option<&ReplyOrigin>,
    route: impl FnOnce(&str, bool) -> Result<RouteReport, String>,
) -> Result<DeliveryOutcome, DispatchError> {
    let reply = phone_request(message, event_id, origin);
    let routing = info_span!(
        target: TRACE_TARGET,
        "harold.route",
        resolved_by = tracing::field::Empty
    );
    // Fails only when no export layer is installed, and then there is no trace to join.
    let _ = routing.set_parent(reply.cx.clone());
    let report = routing
        .in_scope(|| route(&message.text, message.forwarded))
        .map_err(DispatchError::Retryable)?;
    let resolved_by = match &report.routing {
        Routing::Typed { resolved_by, .. } => *resolved_by,
        Routing::NotOwned { .. } | Routing::Paused | Routing::NoPane => ResolvedBy::None,
    };
    routing.record("resolved_by", resolved_by.as_str());
    drop(routing);
    telemetry::finish_phone_request(&reply, &report.routing, OffsetDateTime::now_utc());
    Ok(report.outcome)
}

/// The key a request the event handler opened is kept under, so a retry finds it.
fn event_key(event_id: &str) -> String {
    format!("event:{event_id}")
}

/// The open phone request of a stored reply: the one its listener opened, found by the
/// trace its event recorded, or one opened by an earlier try of this delivery. Otherwise a
/// new request that starts when the event was stored, on the `forwarded` channel when
/// another Mac's hub forwarded it, and `unknown` when it is not known where it came from
/// (after a restart, say). The new request is kept for a retry.
fn phone_request(
    message: &InboundMessage,
    event_id: &str,
    origin: Option<&ReplyOrigin>,
) -> OpenReply {
    let key = event_key(event_id);
    let open = origin
        .and_then(|origin| origin.trace_id.as_deref())
        .and_then(telemetry::open_reply_for)
        .or_else(|| telemetry::open_reply_for(&key));
    if let Some(reply) = open {
        return reply;
    }
    let channel = if message.forwarded {
        "forwarded"
    } else {
        "unknown"
    };
    let started = origin.map_or_else(OffsetDateTime::now_utc, |origin| origin.created_at);
    let reply = telemetry::open_phone_request(channel, started);
    telemetry::keep_open_reply_as(key, reply.clone());
    reply
}

/// A reply given up is one Harold dropped: its open request, found as `phone_request`
/// finds it, is finished at once as unavailable.
fn finish_given_up_request(event_id: &str, origin: &ReplyOrigin) {
    let open = origin
        .trace_id
        .as_deref()
        .and_then(telemetry::open_reply_for)
        .or_else(|| telemetry::open_reply_for(&event_key(event_id)));
    if let Some(reply) = open {
        telemetry::finish_phone_request(&reply, &Routing::NoPane, OffsetDateTime::now_utc());
    }
}

/// A reply handled while messaging is paused is not routed: its open request, if any,
/// ends uncounted.
fn finish_paused_request(origin: Option<&ReplyOrigin>) {
    let open = origin
        .and_then(|origin| origin.trace_id.as_deref())
        .and_then(telemetry::open_reply_for);
    if let Some(reply) = open {
        telemetry::finish_phone_request(&reply, &Routing::Paused, OffsetDateTime::now_utc());
    }
}

/// Send a stored agent message through `send`, then hand its pane to `record_source` as the
/// one an untagged reply goes to. `drop_reason` is asked first, with the message's pane id:
/// when it gives a log line, the message is dropped with that line and neither `send` nor
/// `record_source` runs. None of a notification's presence checks apply: the message is
/// sent, or dropped, whether the screen is locked or not, and the voice mute does not
/// cover it.
fn dispatch_away_message(
    delivery: &PendingDelivery,
    messaging_paused: bool,
    drop_reason: impl FnOnce(&str) -> Option<left_out::Held>,
    send: impl FnOnce(&AwayMessageRequested) -> Result<(), String>,
    record_source: impl FnOnce(AgentAddress),
) -> Result<DeliveryOutcome, DispatchError> {
    // Checked before the payload is read, as for a reply: a paused message is dropped, not
    // sent later.
    if messaging_paused {
        info!("away message skipped (messaging paused)");
        return Ok(DeliveryOutcome::Skipped);
    }
    let message = serde_json::from_value::<AwayMessageRequested>(delivery.payload.clone())
        .map_err(|error| {
            DispatchError::Permanent(format!("invalid AwayMessageRequested payload: {error}"))
        })?;
    // Asked on every try, so a message whose send failed earlier is dropped once the rule
    // applies. The line names the pane and never holds the message's text.
    if let Some(held) = drop_reason(&message.pane_id) {
        held.log(left_out::UpdateKind::AgentMessage, &message.pane_id);
        return Ok(DeliveryOutcome::Skipped);
    }
    send(&message).map_err(DispatchError::Retryable)?;
    record_source(AgentAddress::TmuxPane {
        pane_id: message.pane_id,
        label: message.pane_label,
    });
    Ok(DeliveryOutcome::Delivered)
}

/// Say a stored spoken line at the desk or send it to the away device. `drop_reason` is
/// asked first, with the line's pane id: when it gives a log line, the line is dropped.
/// Then the screen lock decides: locked, the line goes to the away device unless messaging
/// is paused; unlocked, it is said unless the voice is muted. Both are asked on every try,
/// so a line whose send failed while away is said if the user is back for the retry. Log
/// lines name the pane and never hold the text.
#[cfg(test)]
fn dispatch_spoken_line(
    delivery: &PendingDelivery,
    messaging_paused: bool,
    voice_muted: bool,
    drop_reason: impl Fn(&str) -> Option<left_out::Held>,
    environment: &dyn NotifyEnvironment,
) -> Result<DeliveryOutcome, DispatchError> {
    dispatch_spoken_line_with_app(
        delivery,
        messaging_paused,
        voice_muted,
        &drop_reason,
        &drop_reason,
        environment,
        |_| None,
        |_, _| None,
        |_, _| {},
        |_| unreachable!("the app route is off"),
    )
}

/// `dispatch_spoken_line` with the app route. The phone's call comes first: `in_call` is
/// asked with the line's pane id and text, and says the line in the open call, if any.
/// On the locked-screen branch, `chief_app` is asked with the pane id: when it gives the
/// app route, the line goes to the app as text, and to iMessage as text when the app does
/// not push it. No voice note is made for it. `phone_drop` and `desk_drop` give the log
/// line for a pane not on the list of the branch taken, once the voice mute and the
/// messaging pause have been looked at.
#[allow(clippy::too_many_arguments)]
fn dispatch_spoken_line_with_app<'a>(
    delivery: &PendingDelivery,
    messaging_paused: bool,
    voice_muted: bool,
    phone_drop: impl FnOnce(&str) -> Option<left_out::Held>,
    desk_drop: impl FnOnce(&str) -> Option<left_out::Held>,
    environment: &dyn NotifyEnvironment,
    chief_app: impl FnOnce(&str) -> Option<ChiefApp<'a>>,
    in_call: impl FnOnce(&str, &str) -> Option<bool>,
    sent: impl Fn(&str, Channel),
    app_held: impl FnOnce(&str) -> left_out::Held,
) -> Result<DeliveryOutcome, DispatchError> {
    let line = serde_json::from_value::<SpokenLineRequested>(delivery.payload.clone()).map_err(
        |error| DispatchError::Permanent(format!("invalid SpokenLineRequested payload: {error}")),
    )?;
    let pane_id = line.pane_id.as_str();
    if in_call(pane_id, &line.text) == Some(true) {
        info!(pane_id = %pane_id, "spoken line said in the open call");
        sent(pane_id, Channel::WebApp);
        return Ok(DeliveryOutcome::Delivered);
    }
    if environment.screen_locked() {
        if messaging_paused {
            info!(pane_id = %pane_id, "spoken line dropped (messaging paused)");
            return Ok(DeliveryOutcome::Skipped);
        }
        if let Some(held) = phone_drop(pane_id) {
            held.log(left_out::UpdateKind::SpokenLine, pane_id);
            return Ok(DeliveryOutcome::Skipped);
        }
        if let Some(chief_app) = chief_app(pane_id) {
            let carried_by =
                deliver_from_chief(&chief_app, &delivery.event_id, &line.text, &line.text)
                    .map_err(DispatchError::Retryable)?;
            match carried_by {
                CarriedBy::Held => {
                    app_held(pane_id).log(left_out::UpdateKind::SpokenLine, pane_id);
                    return Ok(DeliveryOutcome::Skipped);
                }
                CarriedBy::App => sent(pane_id, Channel::WebApp),
                CarriedBy::Away => sent(pane_id, environment.notify_settings().away()),
            }
            return Ok(DeliveryOutcome::Delivered);
        }
        environment
            .send_spoken_line(&line.text)
            .map_err(DispatchError::Retryable)?;
        info!(pane_id = %pane_id, "spoken line sent to the away device");
        sent(pane_id, environment.notify_settings().away());
        return Ok(DeliveryOutcome::Delivered);
    }
    if voice_muted {
        info!(pane_id = %pane_id, "spoken line dropped (voice off)");
        return Ok(DeliveryOutcome::Skipped);
    }
    if let Some(held) = desk_drop(pane_id) {
        held.log(left_out::UpdateKind::SpokenLine, pane_id);
        return Ok(DeliveryOutcome::Skipped);
    }
    environment
        .speak_line(&line.text)
        .map_err(DispatchError::Retryable)?;
    info!(pane_id = %pane_id, "spoken line said at the desk");
    sent(pane_id, Channel::MachineAudioOutput);
    Ok(DeliveryOutcome::Delivered)
}

/// The dispatcher the daemon runs: it reads the tracked agent panes from `snapshots`, the
/// hub the event handler publishes to before each delivery.
///
/// With `app` set, the chief's messages to the phone go to the web app.
fn production_dispatcher(
    environment: Arc<dyn NotifyEnvironment>,
    snapshots: &AgentSnapshotHub,
    app: Option<Arc<dyn AppRoute>>,
    alerts: Option<AlertQueue>,
) -> ProductionDispatcher {
    let dispatcher =
        ProductionDispatcher::new(environment, Some(snapshots.clone())).with_alerts(alerts);
    match app {
        Some(app) => dispatcher.with_app(app),
        None => dispatcher,
    }
}

/// A delivery whose send fails this many times is given up.
const MAX_TRIES: i64 = 10;

/// The longest wait between two tries of a delivery.
const LONGEST_RETRY_WAIT: Duration = Duration::from_secs(60);

/// The wait after a delivery's `failures`-th failure: 1 second, doubling up to a minute.
fn retry_wait(failures: i64) -> Duration {
    let doublings = u32::try_from(failures.saturating_sub(1))
        .unwrap_or(0)
        .min(6);
    Duration::from_secs(1_u64 << doublings).min(LONGEST_RETRY_WAIT)
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum HandlerError {
    #[error(transparent)]
    Store(#[from] StoreError),

    #[error("delivery task failed: {0}")]
    Join(#[from] tokio::task::JoinError),

    #[error("delivery failed: {error}")]
    Delivery { error: String, retry_in: Duration },

    /// The row was sent, but the outbox could not store that, so the store is failing. The
    /// row must be marked delivered later, not sent again.
    #[error("delivery sent but not marked delivered: {error}")]
    NotMarkedDelivered { event_id: String, error: StoreError },
}

#[cfg(test)]
pub(crate) async fn handle_next_delivery(
    store: &HaroldStore,
    dispatcher: Arc<dyn DeliveryDispatcher>,
) -> Result<bool, HandlerError> {
    handle_next_delivery_inner(store, dispatcher, None, None).await
}

/// `handle_next_delivery` with the alert address on, queueing into `alerts`.
#[cfg(test)]
pub(crate) async fn handle_next_delivery_alerting(
    store: &HaroldStore,
    dispatcher: Arc<dyn DeliveryDispatcher>,
    alerts: &AlertQueue,
) -> Result<bool, HandlerError> {
    handle_next_delivery_inner(store, dispatcher, None, Some(alerts)).await
}

async fn handle_next_delivery_inner(
    store: &HaroldStore,
    dispatcher: Arc<dyn DeliveryDispatcher>,
    snapshots: Option<&AgentSnapshotHub>,
    alerts: Option<&AlertQueue>,
) -> Result<bool, HandlerError> {
    project(store, snapshots).await?;
    let Some(delivery) = store.next_pending_delivery().await? else {
        return Ok(false);
    };

    let event_id = delivery.event_id.clone();
    let attempt_count = delivery.attempt_count;
    let last_try = attempt_count + 1 >= MAX_TRIES;
    let event_version = delivery.event_version;
    let event_type = delivery.event_type.clone();
    let trace_id = delivery.trace_id.clone();
    let origin = if event_type == "InboundMessageReceived" {
        Some(reply_origin(store, event_version).await)
    } else {
        None
    };
    let given_up_origin = origin.clone();
    let span = info_span!(
        "event",
        trace_id = %trace_id,
        event_version = event_version.get(),
        event_type = %event_type,
    );
    let blocking_dispatcher = Arc::clone(&dispatcher);
    let (result, delivering) = tokio::task::spawn_blocking(move || {
        dispatch_in_spans(
            blocking_dispatcher.as_ref(),
            &delivery,
            origin.as_ref(),
            &span,
            last_try,
        )
    })
    .await?;

    match result {
        Ok(outcome) => {
            if let Err(error) = store.mark_delivered(&event_id).await {
                return Err(HandlerError::NotMarkedDelivered { event_id, error });
            }
            if matches!(
                outcome,
                DeliveryOutcome::Skipped | DeliveryOutcome::HeldBack { .. }
            ) {
                in_delivery(delivering.as_ref(), || {
                    info!(
                        event_id,
                        event_version = event_version.get(),
                        event_type,
                        "delivery intentionally skipped"
                    );
                });
            }
            Ok(true)
        }
        Err(error) => match error {
            DispatchError::Permanent(error) => {
                store.mark_undeliverable(&event_id, &error).await?;
                in_delivery(delivering.as_ref(), || {
                    warn!(
                        event_id,
                        event_version = event_version.get(),
                        event_type,
                        error,
                        "event is permanently undeliverable; continuing"
                    );
                });
                Ok(true)
            }
            DispatchError::Retryable(error) => {
                if !last_try {
                    store.record_delivery_failure(&event_id, &error).await?;
                    return Err(HandlerError::Delivery {
                        error,
                        retry_in: retry_wait(attempt_count + 1),
                    });
                }
                // Stored before anything is said, so a give-up that was not stored is
                // tried again and said then.
                store.mark_undeliverable(&event_id, &error).await?;
                if let Some(origin) = &given_up_origin {
                    finish_given_up_request(&event_id, origin);
                }
                in_delivery(delivering.as_ref(), || {
                    warn!(
                        event_type,
                        event_version = event_version.get(),
                        outcome = "given_up",
                        reason = "too_many_tries",
                        "delivery given up"
                    );
                });
                if let Some(alerts) = alerts {
                    alerts.firing(crate::alerts::DELIVERY_GIVEN_UP);
                }
                Ok(true)
            }
        },
    }
}

/// Dispatches `delivery` inside `event_span` and, for a turn end, inside
/// `harold.delivery`, which records the channel and the outcome. Returns the result and
/// the delivery span, if any, for the lines written about the result.
///
/// - The delivery continues the turn's trace, found by the event id, or by the turn's pane
///   when the handler that stored the turn end has not yet learned its event id. With
///   neither (after a restart, say), it starts its own trace.
/// - A failed delivery is tried again, so the turn's trace is kept for the retry, unless
///   this was its last try, which records `given_up`.
/// - `harold.delivery` has no `tracing` parent, so a line written in it after `event_span`
///   is left gets no span prefix in harold.log.
fn dispatch_in_spans(
    dispatcher: &dyn DeliveryDispatcher,
    delivery: &PendingDelivery,
    origin: Option<&ReplyOrigin>,
    event_span: &tracing::Span,
    last_try: bool,
) -> (
    Result<DeliveryOutcome, DispatchError>,
    Option<tracing::Span>,
) {
    let _event = event_span.enter();
    let dispatch = || match origin {
        Some(origin) => dispatcher.dispatch_reply(delivery, origin),
        None => dispatcher.dispatch(delivery),
    };
    if delivery.event_type != "TurnCompleted" {
        return (dispatch(), None);
    }
    let delivering = info_span!(
        target: TRACE_TARGET,
        parent: None,
        "harold.delivery",
        kind = "turn_end",
        channel = tracing::field::Empty,
        outcome = tracing::field::Empty,
        reason = tracing::field::Empty,
    );
    let turn = telemetry::take_turn_delivery(&delivery.event_id).or_else(|| {
        delivery
            .payload
            .get("pane_id")
            .and_then(serde_json::Value::as_str)
            .and_then(telemetry::take_pending_turn)
    });
    if let Some(turn) = &turn {
        // Fails only when no export layer is installed, and then there is no trace to join.
        let _ = delivering.set_parent(turn.clone());
    }
    let result = delivering.in_scope(dispatch);
    let outcome = match &result {
        Ok(DeliveryOutcome::Delivered) => "delivered",
        Ok(DeliveryOutcome::Skipped) => "skipped",
        Ok(DeliveryOutcome::HeldBack { reason }) => {
            delivering.record("reason", *reason);
            "held_back"
        }
        Err(DispatchError::Retryable(_)) if last_try => "given_up",
        Err(_) => "failed",
    };
    delivering.record("outcome", outcome);
    if !last_try
        && let (Some(turn), Err(DispatchError::Retryable(_))) = (turn, &result)
        && let Ok(event_id) = uuid::Uuid::parse_str(&delivery.event_id)
    {
        telemetry::keep_turn_delivery(event_id, turn);
    }
    (result, Some(delivering))
}

/// Runs `log` inside the delivery span `delivering`, when there is one, so the line
/// carries the turn's trace.
fn in_delivery(delivering: Option<&tracing::Span>, log: impl FnOnce()) {
    match delivering {
        Some(span) => span.in_scope(log),
        None => log(),
    }
}

/// The trace and time the stored reply at `version` recorded. When its event cannot be
/// read, the reply's request has no trace to continue and starts now.
async fn reply_origin(store: &HaroldStore, version: EventVersion) -> ReplyOrigin {
    match stored_envelope(store, version).await {
        Some((trace_id, created_at)) => ReplyOrigin {
            trace_id,
            created_at,
        },
        None => ReplyOrigin {
            trace_id: None,
            created_at: OffsetDateTime::now_utc(),
        },
    }
}

/// The trace id and stored time of the event at `version`, read from its envelope, or
/// `None` when it cannot be read. This is the event handler's only read of the event
/// stream itself.
async fn stored_envelope(
    store: &HaroldStore,
    version: EventVersion,
) -> Option<(Option<String>, OffsetDateTime)> {
    match store.stream().read(version).await {
        Ok(envelope) => envelope.map(|envelope| (envelope.trace_id, envelope.created_at)),
        Err(error) => {
            warn!(error = %error, "stored reply's event could not be read; its phone request starts now");
            None
        }
    }
}

pub(crate) async fn project_and_publish_agent_snapshot(
    store: &HaroldStore,
    snapshots: &AgentSnapshotHub,
    limit: usize,
) -> crate::store::Result<ProjectionBatch> {
    let batch = store.project_unhandled_events(limit).await?;
    if batch.through_event_version.get() > snapshots.through_event_version().get() {
        let snapshot = store.load_agent_snapshot().await?;
        crate::outbound::set_tts_muted(snapshot.tts_muted);
        crate::outbound::set_messaging_paused(snapshot.messaging_paused);
        snapshots.publish_committed(snapshot);
    }
    Ok(batch)
}

async fn wait_or_shutdown(shutdown: &mut watch::Receiver<()>, delay: Duration) -> bool {
    tokio::select! {
        _ = shutdown.changed() => true,
        _ = tokio::time::sleep(delay) => false,
    }
}

/// How often the handler looks for work when idle, and projects events while a delivery
/// waits for its next try.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// The pause after a cycle that failed for any reason but a delivery's send.
const ERROR_PAUSE: Duration = Duration::from_secs(1);

/// `run_event_handler_with_app` without the web app. Only tests call it now.
#[cfg(test)]
pub async fn run_event_handler(
    store: Arc<HaroldStore>,
    snapshots: AgentSnapshotHub,
    alerts: Option<AlertQueue>,
    shutdown: watch::Receiver<()>,
) {
    run_event_handler_with_app(store, snapshots, alerts, None, shutdown).await;
}

/// `run_event_handler` with `app`: when it is set, the chief's messages to the phone go to
/// the web app.
pub async fn run_event_handler_with_app(
    store: Arc<HaroldStore>,
    snapshots: AgentSnapshotHub,
    alerts: Option<AlertQueue>,
    app: Option<Arc<dyn AppRoute>>,
    shutdown: watch::Receiver<()>,
) {
    let dispatcher: Arc<dyn DeliveryDispatcher> = Arc::new(production_dispatcher(
        Arc::new(SystemEnvironment),
        &snapshots,
        app,
        alerts.clone(),
    ));
    // Voice notes sent before a restart are kept for Messages to read; remove the old ones.
    crate::outbound::voice_note::remove_old_notes(
        &crate::outbound::voice_note::notes_parent(),
        std::time::SystemTime::now(),
    );
    info!("event handler starting");
    run_deliveries(store, dispatcher, Some(snapshots), alerts, shutdown).await;
    info!("event handler shutting down");
}

/// Delivers the outbox oldest first through `dispatcher` until shutdown. After a failed
/// send, the same row is tried again once its wait is up, and no later row is tried
/// meanwhile. Events are still projected during the wait, every POLL_INTERVAL, so the
/// agent snapshots stay current.
///
/// While the store is failing, nothing is projected or sent: the loop only waits
/// POLL_INTERVAL between checks. A row that was sent but could not be marked delivered is
/// marked later, before anything else, and never sent again.
async fn run_deliveries(
    store: Arc<HaroldStore>,
    dispatcher: Arc<dyn DeliveryDispatcher>,
    snapshots: Option<AgentSnapshotHub>,
    alerts: Option<AlertQueue>,
    mut shutdown: watch::Receiver<()>,
) {
    let mut retry_at: Option<tokio::time::Instant> = None;
    let mut unmarked: Option<String> = None;
    loop {
        if shutdown.has_changed().unwrap_or(true) {
            break;
        }
        if let Some(event_id) = &unmarked {
            if store.mark_delivered(event_id).await.is_ok() {
                unmarked = None;
                continue;
            }
            if wait_or_shutdown(&mut shutdown, POLL_INTERVAL).await {
                break;
            }
            continue;
        }
        if store.is_failing() {
            if wait_or_shutdown(&mut shutdown, POLL_INTERVAL).await {
                break;
            }
            continue;
        }
        if let Some(at) = retry_at {
            let now = tokio::time::Instant::now();
            if now < at {
                let pause = match project(&store, snapshots.as_ref()).await {
                    Ok(()) => POLL_INTERVAL,
                    Err(error) => {
                        warn!(error = %error, "event handler cycle failed");
                        ERROR_PAUSE
                    }
                };
                if wait_or_shutdown(&mut shutdown, pause.min(at - now)).await {
                    break;
                }
                continue;
            }
            retry_at = None;
        }
        let handled = handle_next_delivery_inner(
            &store,
            Arc::clone(&dispatcher),
            snapshots.as_ref(),
            alerts.as_ref(),
        )
        .await;
        let pause = match handled {
            Ok(true) => continue,
            Ok(false) => POLL_INTERVAL,
            Err(HandlerError::Delivery { error, retry_in }) => {
                warn!(error = %error, "event handler cycle failed");
                retry_at = Some(tokio::time::Instant::now() + retry_in);
                continue;
            }
            Err(HandlerError::NotMarkedDelivered { event_id, error }) => {
                warn!(error = %error, "event handler cycle failed");
                unmarked = Some(event_id);
                continue;
            }
            Err(error) => {
                warn!(error = %error, "event handler cycle failed");
                ERROR_PAUSE
            }
        };
        if wait_or_shutdown(&mut shutdown, pause).await {
            break;
        }
    }
}

/// Projects new events, and publishes the agent snapshot when there is a hub.
async fn project(
    store: &HaroldStore,
    snapshots: Option<&AgentSnapshotHub>,
) -> crate::store::Result<()> {
    match snapshots {
        Some(snapshots) => project_and_publish_agent_snapshot(store, snapshots, 500)
            .await
            .map(drop),
        None => store.project_unhandled_events(500).await.map(drop),
    }
}

#[cfg(test)]
#[path = "projector_tests.rs"]
pub(crate) mod tests;
