use std::sync::Arc;
use std::time::Duration;

use time::OffsetDateTime;
use tokio::sync::watch;
use tracing::{info, info_span, warn};
use tracing_opentelemetry::OpenTelemetrySpanExt as _;

use crate::agent::snapshot::AgentSnapshotHub;
use crate::inbound::{AgentAddress, RouteReport, route_inbound_message};
use crate::outbound::left_out::{
    TrackedAgents, message_dropped_reason, spoken_line_dropped_reason,
};
use crate::outbound::{DeliveryOutcome, NotifyEnvironment, NotifyState, SystemEnvironment, notify};
use crate::store::{
    AwayMessageRequested, HaroldStore, InboundMessage, PendingDelivery, ProjectionBatch,
    SpokenLineRequested, TurnCompleted,
};
use crate::telemetry::{self, OpenReply, ResolvedBy, Routing, TRACE_TARGET};

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
        }
    }

    /// The dispatcher with `router` in place of tmux routing.
    #[cfg(test)]
    fn with_router(self, router: Router) -> Self {
        Self { router, ..self }
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
                notify(
                    &turn,
                    &delivery.trace_id,
                    &self.notify_state,
                    &self.tracked_agents(),
                    self.environment.as_ref(),
                )
                .map_err(DispatchError::Retryable)
            }
            "InboundMessageReceived" => {
                // Checked before the payload is read, so a backlog queued before the
                // pause drains without being routed or confirmed.
                if messaging_paused {
                    finish_paused_request(origin);
                    return Ok(DeliveryOutcome::Skipped);
                }
                let message = serde_json::from_value::<InboundMessage>(delivery.payload.clone())
                    .map_err(|error| {
                        DispatchError::Permanent(format!(
                            "invalid InboundMessageReceived payload: {error}"
                        ))
                    })?;
                route_message(&message, &delivery.event_id, origin, |text, forwarded| {
                    (self.router)(text, forwarded)
                })
            }
            "AwayMessageRequested" => dispatch_away_message(
                delivery,
                messaging_paused,
                |pane_id| {
                    message_dropped_reason(
                        self.environment
                            .notify_settings()
                            .away_messages_only_from
                            .as_deref(),
                        &self.tracked_agents(),
                        pane_id,
                    )
                },
                |message| self.environment.send_away_message(message),
                |source| {
                    // The pane's mark: its turn-end update for this turn is left out.
                    self.notify_state
                        .mark_requested_message(source.pane_id(), self.environment.now());
                    self.environment.record_reply_target(source);
                },
            ),
            "SpokenLineRequested" => dispatch_spoken_line(
                delivery,
                messaging_paused,
                crate::outbound::is_tts_muted(),
                |pane_id| {
                    spoken_line_dropped_reason(
                        self.environment
                            .notify_settings()
                            .away_messages_only_from
                            .as_deref(),
                        &self.tracked_agents(),
                        pane_id,
                    )
                },
                self.environment.as_ref(),
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
        Routing::NotOwned | Routing::Paused | Routing::NoPane => ResolvedBy::None,
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
    drop_reason: impl FnOnce(&str) -> Option<String>,
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
    if let Some(reason) = drop_reason(&message.pane_id) {
        info!(pane_id = %message.pane_id, "{reason}");
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
fn dispatch_spoken_line(
    delivery: &PendingDelivery,
    messaging_paused: bool,
    voice_muted: bool,
    drop_reason: impl FnOnce(&str) -> Option<String>,
    environment: &dyn NotifyEnvironment,
) -> Result<DeliveryOutcome, DispatchError> {
    let line = serde_json::from_value::<SpokenLineRequested>(delivery.payload.clone()).map_err(
        |error| DispatchError::Permanent(format!("invalid SpokenLineRequested payload: {error}")),
    )?;
    let pane_id = line.pane_id.as_str();
    if let Some(reason) = drop_reason(pane_id) {
        info!(pane_id = %pane_id, "{reason}");
        return Ok(DeliveryOutcome::Skipped);
    }
    if environment.screen_locked() {
        if messaging_paused {
            info!(pane_id = %pane_id, "spoken line dropped (messaging paused)");
            return Ok(DeliveryOutcome::Skipped);
        }
        environment
            .send_spoken_line(&line.text)
            .map_err(DispatchError::Retryable)?;
        info!(pane_id = %pane_id, "spoken line sent to the away device");
        return Ok(DeliveryOutcome::Delivered);
    }
    if voice_muted {
        info!(pane_id = %pane_id, "spoken line dropped (voice off)");
        return Ok(DeliveryOutcome::Skipped);
    }
    environment
        .speak_line(&line.text)
        .map_err(DispatchError::Retryable)?;
    info!(pane_id = %pane_id, "spoken line said at the desk");
    Ok(DeliveryOutcome::Delivered)
}

/// The dispatcher the daemon runs: it reads the tracked agent panes from `snapshots`, the
/// hub the event handler publishes to before each delivery.
fn production_dispatcher(
    environment: Arc<dyn NotifyEnvironment>,
    snapshots: &AgentSnapshotHub,
) -> ProductionDispatcher {
    ProductionDispatcher::new(environment, Some(snapshots.clone()))
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum HandlerError {
    #[error(transparent)]
    Store(#[from] events::EsError),

    #[error("delivery task failed: {0}")]
    Join(#[from] tokio::task::JoinError),

    #[error("delivery failed: {0}")]
    Delivery(String),
}

#[cfg(test)]
pub(crate) async fn handle_next_delivery(
    store: &HaroldStore,
    dispatcher: Arc<dyn DeliveryDispatcher>,
) -> Result<bool, HandlerError> {
    handle_next_delivery_inner(store, dispatcher, None).await
}

async fn handle_next_delivery_inner(
    store: &HaroldStore,
    dispatcher: Arc<dyn DeliveryDispatcher>,
    snapshots: Option<&AgentSnapshotHub>,
) -> Result<bool, HandlerError> {
    match snapshots {
        Some(snapshots) => {
            project_and_publish_agent_snapshot(store, snapshots, 500).await?;
        }
        None => {
            store.project_unhandled_events(500).await?;
        }
    }
    let Some(delivery) = store.next_pending_delivery().await? else {
        return Ok(false);
    };

    let event_id = delivery.event_id.clone();
    let event_version = delivery.event_version;
    let event_type = delivery.event_type.clone();
    let trace_id = delivery.trace_id.clone();
    let origin = if event_type == "InboundMessageReceived" {
        Some(reply_origin(store, event_version).await)
    } else {
        None
    };
    let span = info_span!(
        "event",
        trace_id = %trace_id,
        event_version = event_version.get(),
        event_type = %event_type,
    );
    let blocking_dispatcher = Arc::clone(&dispatcher);
    let result = tokio::task::spawn_blocking(move || {
        let _guard = span.enter();
        match &origin {
            Some(origin) => blocking_dispatcher.dispatch_reply(&delivery, origin),
            None => blocking_dispatcher.dispatch(&delivery),
        }
    })
    .await?;

    match result {
        Ok(outcome) => {
            store.mark_delivered(&event_id).await?;
            if outcome == DeliveryOutcome::Skipped {
                info!(
                    event_id,
                    event_version = event_version.get(),
                    event_type,
                    "delivery intentionally skipped"
                );
            }
            Ok(true)
        }
        Err(error) => match error {
            DispatchError::Permanent(error) => {
                store.mark_undeliverable(&event_id, &error).await?;
                warn!(
                    event_id,
                    event_version = event_version.get(),
                    event_type,
                    error,
                    "event is permanently undeliverable; continuing"
                );
                Ok(true)
            }
            DispatchError::Retryable(error) => {
                store.record_delivery_failure(&event_id, &error).await?;
                Err(HandlerError::Delivery(error))
            }
        },
    }
}

/// The trace and time the stored reply at `version` recorded. When its event cannot be
/// read, the reply's request has no trace to continue and starts now.
async fn reply_origin(store: &HaroldStore, version: events::EventStreamVersion) -> ReplyOrigin {
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
/// stream itself: a store that replaces `HaroldStore::stream()` swaps this function.
async fn stored_envelope(
    store: &HaroldStore,
    version: events::EventStreamVersion,
) -> Option<(Option<String>, OffsetDateTime)> {
    let before = events::EventStreamVersion::new(version.get() - 1)
        .unwrap_or_else(|_| events::EventStreamVersion::start());
    let envelopes = match store.stream().load_after_version(before, 1).await {
        Ok(envelopes) => envelopes,
        Err(error) => {
            warn!(error = %error, "stored reply's event could not be read; its phone request starts now");
            return None;
        }
    };
    envelopes
        .into_iter()
        .find(|envelope| envelope.version == version)
        .map(|envelope| (envelope.trace_id, envelope.created_at))
}

pub(crate) async fn project_and_publish_agent_snapshot(
    store: &HaroldStore,
    snapshots: &AgentSnapshotHub,
    limit: usize,
) -> events::Result<ProjectionBatch> {
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

pub async fn run_event_handler(
    store: Arc<HaroldStore>,
    snapshots: AgentSnapshotHub,
    mut shutdown: watch::Receiver<()>,
) {
    let dispatcher: Arc<dyn DeliveryDispatcher> = Arc::new(production_dispatcher(
        Arc::new(SystemEnvironment),
        &snapshots,
    ));
    // Voice notes sent before a restart are kept for Messages to read; remove the old ones.
    crate::outbound::voice_note::remove_old_notes(
        &crate::outbound::voice_note::notes_parent(),
        std::time::SystemTime::now(),
    );
    info!("event handler starting");

    loop {
        if shutdown.has_changed().unwrap_or(true) {
            break;
        }

        match handle_next_delivery_inner(&store, Arc::clone(&dispatcher), Some(&snapshots)).await {
            Ok(true) => continue,
            Ok(false) => {
                if wait_or_shutdown(&mut shutdown, Duration::from_millis(100)).await {
                    break;
                }
            }
            Err(error) => {
                warn!(error = %error, "event handler cycle failed");
                if wait_or_shutdown(&mut shutdown, Duration::from_secs(1)).await {
                    break;
                }
            }
        }
    }

    info!("event handler shutting down");
}

#[cfg(test)]
#[path = "projector_tests.rs"]
mod tests;
