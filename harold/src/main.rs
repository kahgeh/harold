mod activity_summary;
mod agent;
mod alerts;
mod away_message;
mod channels;
mod cli;
mod crash_guard;
mod inbound;
mod outbound;
mod peers;
mod pod_stats;
mod projector;
mod settings;
mod store;
mod telemetry;
mod timed_prompts;
mod tmux;
mod tools;
mod util;
mod voice_input;
mod web_app;

use std::sync::Arc;
use std::time::Duration;

use settings::{get_settings, init_settings};
use telemetry::init_telemetry;
use tokio::sync::{mpsc, watch};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, transport::Server};
use tracing::{Instrument, info, info_span};
use tracing_opentelemetry::OpenTelemetrySpanExt as _;

pub use harold_api::harold;

use harold::harold_server::{Harold, HaroldServer};
use harold::{
    AgentMonitorHealth, AgentPaneState, AgentState, AgentStateSnapshot, CallRequest, CallResponse,
    ForwardReplyRequest, ForwardReplyResponse, MonitorHealthState, ReportAgentStateRequest,
    ReportAgentStateResponse, ReportNotificationRequest, ReportNotificationResponse,
    SendMessageToAwayDeviceRequest, SendMessageToAwayDeviceResponse, SetMessagingPausedRequest,
    SetMessagingPausedResponse, SetTtsMutedRequest, SetTtsMutedResponse, SpeakLineRequest,
    SpeakLineResponse, TurnCompleteRequest, TurnCompleteResponse, WatchAgentStatesRequest,
};

/// Release tag compiled in by the release workflow; "source" for any other build.
const HAROLD_VERSION: &str = version_or_source(option_env!("HAROLD_RELEASE_VERSION"));

const fn version_or_source(value: Option<&'static str>) -> &'static str {
    match value {
        Some(version) if !version.is_empty() => version,
        _ => "source",
    }
}

struct HaroldService {
    monitor: agent::runtime::AgentMonitorHandle,
    store: Arc<store::HaroldStore>,
    snapshots: agent::snapshot::AgentSnapshotHub,
    /// Held across a pause request's append and switch, so overlapping requests
    /// cannot leave the store and the switch disagreeing. Holds the last pause value
    /// stored by a request, `None` until the first one since startup.
    messaging_switch: tokio::sync::Mutex<Option<bool>>,
    /// The same for a voice mute request. The two settings are independent, so a mute
    /// does not queue behind a pause.
    tts_switch: tokio::sync::Mutex<Option<bool>>,
    shutdown: watch::Receiver<()>,
    /// Which of the two peer RPCs this Harold answers, and for whom.
    peers: peers::PeerEndpoint,
}

/// The service Harold serves, answering the peer calls its `settings` call for.
fn harold_service(
    settings: &settings::Settings,
    monitor: agent::runtime::AgentMonitorHandle,
    store: Arc<store::HaroldStore>,
    snapshots: agent::snapshot::AgentSnapshotHub,
    shutdown: watch::Receiver<()>,
) -> HaroldService {
    HaroldService {
        monitor,
        store,
        snapshots,
        messaging_switch: tokio::sync::Mutex::new(None),
        tts_switch: tokio::sync::Mutex::new(None),
        shutdown,
        peers: peers::PeerEndpoint::from_settings(&settings.notify.away_channel, &settings.peers),
    }
}

/// Stores a changed on/off setting with `append`, then hands it to `switch` straight away
/// so it acts before the projector reaches the event. Returns whether anything was stored.
/// Requests run one at a time, so the last one stored is also the last one switched.
///
/// A request is a repeat when it equals the last value stored by a request. The published
/// snapshot and the switch cannot decide that: both are written by the projector from a
/// snapshot that may predate a stored change, so they can briefly show the value before
/// it. The request handlers are the only writers of these settings, so once one has stored
/// a value, `last_requested` is the stream's last word. A repeat stores nothing but still
/// sets the switch, which the projector may have just put back to the older value.
///
/// With nothing remembered, `already_in_force` decides. That is the case before the first
/// request since startup, and after an append that failed or was dropped part-way: the
/// event may have been written all the same, so the remembered value is forgotten first.
async fn apply_setting<A, E>(
    last_requested: &tokio::sync::Mutex<Option<bool>>,
    requested: bool,
    already_in_force: impl FnOnce() -> bool,
    append: impl FnOnce() -> A,
    switch: impl FnOnce(bool),
) -> Result<bool, E>
where
    A: Future<Output = Result<(), E>>,
{
    let mut last_requested = last_requested.lock().await;
    let repeat = match *last_requested {
        Some(last) => last == requested,
        None => already_in_force(),
    };
    if !repeat {
        *last_requested = None;
        append().await?;
        *last_requested = Some(requested);
    }
    switch(requested);
    Ok(!repeat)
}

impl HaroldService {
    /// Persists a changed pause flag and flips the switch, see `apply_setting`. `switched`
    /// reads the switch's current value: the published snapshot can lag a change that is
    /// stored but not yet projected, so with no earlier request to go by, a request is a
    /// repeat only when both already agree with it.
    async fn apply_messaging_paused(
        &self,
        paused: bool,
        switched: impl FnOnce() -> bool,
        switch: impl FnOnce(bool),
    ) -> Result<(), Status> {
        let stored = apply_setting(
            &self.messaging_switch,
            paused,
            || switched() == paused && self.snapshots.messaging_paused() == paused,
            || store::append_messaging_paused_changed(&self.store, paused),
            switch,
        )
        .await
        .map_err(|error| {
            tracing::error!(result = "append_failed", error = %error, "messaging pause rejected");
            Status::internal("event store write failed")
        })?;
        if stored {
            info!(paused, "messaging pause change persisted");
        }
        Ok(())
    }

    /// The voice mute, handled like the messaging pause above.
    async fn apply_tts_muted(
        &self,
        muted: bool,
        switched: impl FnOnce() -> bool,
        switch: impl FnOnce(bool),
    ) -> Result<(), Status> {
        let stored = apply_setting(
            &self.tts_switch,
            muted,
            || switched() == muted && self.snapshots.tts_muted() == muted,
            || store::append_tts_mute_changed(&self.store, muted),
            switch,
        )
        .await
        .map_err(|error| {
            tracing::error!(result = "append_failed", error = %error, "tts mute rejected");
            Status::internal("event store write failed")
        })?;
        if stored {
            info!(muted, "tts mute change persisted");
        }
        Ok(())
    }
}

#[tonic::async_trait]
impl Harold for HaroldService {
    type WatchAgentStatesStream = ReceiverStream<Result<AgentStateSnapshot, Status>>;

    async fn set_tts_muted(
        &self,
        request: Request<SetTtsMutedRequest>,
    ) -> Result<Response<SetTtsMutedResponse>, Status> {
        let muted = request.into_inner().muted;
        self.apply_tts_muted(muted, outbound::is_tts_muted, outbound::set_tts_muted)
            .await?;
        Ok(Response::new(SetTtsMutedResponse { muted }))
    }

    async fn set_messaging_paused(
        &self,
        request: Request<SetMessagingPausedRequest>,
    ) -> Result<Response<SetMessagingPausedResponse>, Status> {
        let paused = request.into_inner().paused;
        self.apply_messaging_paused(
            paused,
            outbound::is_messaging_paused,
            outbound::set_messaging_paused,
        )
        .await?;
        Ok(Response::new(SetMessagingPausedResponse { paused }))
    }

    async fn turn_complete(
        &self,
        request: Request<TurnCompleteRequest>,
    ) -> Result<Response<TurnCompleteResponse>, Status> {
        let req = request.into_inner();
        let trace_id = uuid::Uuid::new_v4().to_string();
        let span = info_span!("grpc_turn_complete", trace_id = %trace_id);
        let hook_reached = time::OffsetDateTime::now_utc();
        let trace = telemetry::begin_turn_complete(
            &req.pane_id,
            non_empty(&req.session_id),
            non_empty(&req.turn_started_at),
            hook_reached,
        );
        let turn_cx = trace.turn_complete.context();
        // Kept by the pane before the monitor stores the turn end: the event handler may
        // deliver it before the monitor answers with its event id.
        telemetry::keep_pending_turn(&req.pane_id, turn_cx.clone());

        async {
            let pane_id = req.pane_id.clone();
            let pane_id_log = pane_id_for_log(&pane_id);
            info!(pane_id = %pane_id_log, "turn complete received");

            let event = store::TurnCompleted {
                pane_id: req.pane_id,
                pane_label: req.pane_label,
                last_user_prompt: req.last_user_prompt,
                assistant_message: req.assistant_message,
                main_context: req.main_context,
                agent_incarnation: None,
                work_summary: agent::domain::CompletionSummaryUpdate::Unchanged,
            };

            let appended = self.monitor
                .turn_completed(event)
                .await
                .map_err(|e| {
                    telemetry::forget_pending_turn(&pane_id, &turn_cx);
                    tracing::error!(pane_id = %pane_id_log, result = "append_failed", error = %e, "turn complete rejected");
                    Status::internal("event store write failed")
                })?;
            match appended
                .events
                .iter()
                .find(|envelope| envelope.r#type == "TurnCompleted")
            {
                Some(stored) => telemetry::settle_pending_turn(&pane_id, stored.id, &turn_cx),
                None => telemetry::forget_pending_turn(&pane_id, &turn_cx),
            }

            info!(pane_id = %pane_id_log, result = "accepted", "turn complete persisted");

            Ok(Response::new(TurnCompleteResponse { accepted: true }))
        }
        // `harold.turn_complete` is entered inside `grpc_turn_complete`, so the lines
        // written along the way carry the turn's trace.
        .instrument(trace.turn_complete)
        .instrument(span)
        .await
    }

    async fn report_agent_state(
        &self,
        request: Request<ReportAgentStateRequest>,
    ) -> Result<Response<ReportAgentStateResponse>, Status> {
        let req = request.into_inner();
        let state = match AgentState::try_from(req.state) {
            Ok(AgentState::Busy) => agent::domain::ObservedAgentState::Busy,
            Ok(AgentState::Idle) => agent::domain::ObservedAgentState::Idle,
            Ok(AgentState::Unspecified | AgentState::Unknown) | Err(_) => {
                return Err(Status::invalid_argument("invalid agent state report"));
            }
        };
        let summary_update = match req.work_summary {
            None => agent::domain::WorkSummaryUpdate::Unchanged,
            Some(value) if value.trim().is_empty() => agent::domain::WorkSummaryUpdate::Clear,
            Some(value) => agent::domain::WorkSummaryUpdate::Set(value),
        };

        self.monitor
            .report_lifecycle(req.pane_id, state, req.adapter_id, summary_update)
            .await
            .map_err(|error| match error {
                agent::runtime::MonitorCommandError::InvalidInput => {
                    Status::invalid_argument("invalid agent state report")
                }
                agent::runtime::MonitorCommandError::AgentNotFound => {
                    Status::failed_precondition("agent incarnation not found")
                }
                agent::runtime::MonitorCommandError::InventoryUnavailable
                | agent::runtime::MonitorCommandError::EventAppend(_)
                | agent::runtime::MonitorCommandError::RuntimeStopped => {
                    Status::unavailable("agent state report unavailable")
                }
            })?;

        Ok(Response::new(ReportAgentStateResponse { accepted: true }))
    }

    async fn watch_agent_states(
        &self,
        _request: Request<WatchAgentStatesRequest>,
    ) -> Result<Response<Self::WatchAgentStatesStream>, Status> {
        let snapshots = self.snapshots.subscribe();
        let shutdown = self.shutdown.clone();
        let (sender, receiver) = mpsc::channel(1);
        tokio::spawn(forward_agent_snapshots(snapshots, sender, shutdown));
        Ok(Response::new(ReceiverStream::new(receiver)))
    }

    async fn report_notification(
        &self,
        request: Request<ReportNotificationRequest>,
    ) -> Result<Response<ReportNotificationResponse>, Status> {
        let req = request.into_inner();
        let member =
            peers::authorize_report(&self.peers, &req.machine, &req.secret).map_err(|denied| {
                tracing::warn!(reason = denied.reason(), "notification report rejected");
                Status::permission_denied("peer call rejected")
            })?;
        inbound::ownership::record_notification(&member.name);
        info!(machine = %member.name, "member notification recorded");
        Ok(Response::new(ReportNotificationResponse { accepted: true }))
    }

    async fn forward_reply(
        &self,
        request: Request<ForwardReplyRequest>,
    ) -> Result<Response<ForwardReplyResponse>, Status> {
        let req = request.into_inner();
        peers::authorize_forward(&self.peers, &req.secret).map_err(|denied| {
            tracing::warn!(reason = denied.reason(), "forwarded reply rejected");
            Status::permission_denied("peer call rejected")
        })?;
        // forwarded = true: the hub decided this Mac owns the reply, so routing does not
        // decide again and never forwards it.
        // The same filter as the hub's Telegram listener. Text it would skip is accepted and
        // ignored: the hub did its job, and a retry could not make the text routable.
        let Some(text) = channels::reply_text(&req.text) else {
            info!("forwarded reply ignored (empty or sent by Harold)");
            return Ok(Response::new(ForwardReplyResponse { accepted: true }));
        };
        let message = store::InboundMessage {
            text,
            forwarded: true,
        };
        store::append_inbound_message(&self.store, &message)
            .await
            .map_err(|error| {
                tracing::error!(result = "append_failed", error = %error, "forwarded reply rejected");
                Status::internal("event store write failed")
            })?;
        info!("forwarded reply stored");
        Ok(Response::new(ForwardReplyResponse { accepted: true }))
    }

    async fn send_message_to_away_device(
        &self,
        request: Request<SendMessageToAwayDeviceRequest>,
    ) -> Result<Response<SendMessageToAwayDeviceResponse>, Status> {
        let req = request.into_inner();
        if !agent::runtime::valid_pane_id(&req.src_id) {
            return Err(Status::invalid_argument(away_message::INVALID_SOURCE_ID));
        }
        let text = away_message::one_line(&req.text);
        if !away_message::is_sendable_length(&text) {
            return Err(Status::invalid_argument(
                away_message::INVALID_MESSAGE_LENGTH,
            ));
        }
        if outbound::is_messaging_paused() {
            return Err(Status::failed_precondition(away_message::MESSAGING_PAUSED));
        }
        let pane =
            self.monitor
                .resolve_agent_pane(req.src_id)
                .await
                .map_err(|error| match error {
                    agent::runtime::MonitorCommandError::InvalidInput => {
                        Status::invalid_argument(away_message::INVALID_SOURCE_ID)
                    }
                    agent::runtime::MonitorCommandError::AgentNotFound => {
                        Status::failed_precondition(away_message::AGENT_NOT_FOUND)
                    }
                    agent::runtime::MonitorCommandError::InventoryUnavailable
                    | agent::runtime::MonitorCommandError::EventAppend(_)
                    | agent::runtime::MonitorCommandError::RuntimeStopped => {
                        Status::unavailable("agent lookup unavailable")
                    }
                })?;
        let event = store::AwayMessageRequested {
            pane_label: inbound::tmux::pane_label(&pane),
            pane_id: pane.incarnation.pane_id,
            text,
        };
        store::append_away_message_requested(&self.store, &event)
            .await
            .map_err(|error| {
                tracing::error!(pane_id = %event.pane_id, result = "append_failed", error = %error, "away message rejected");
                Status::internal("event store write failed")
            })?;
        // The text is the agent's own words to the user: it is stored, not logged here.
        info!(pane_id = %event.pane_id, result = "accepted", "away message stored");
        Ok(Response::new(SendMessageToAwayDeviceResponse {
            accepted: true,
        }))
    }

    async fn speak_line(
        &self,
        request: Request<SpeakLineRequest>,
    ) -> Result<Response<SpeakLineResponse>, Status> {
        let req = request.into_inner();
        if !agent::runtime::valid_pane_id(&req.src_id) {
            return Err(Status::invalid_argument(away_message::INVALID_SOURCE_ID));
        }
        let text = away_message::one_line(&req.text);
        if !outbound::spoken_line::is_speakable_length(&text) {
            return Err(Status::invalid_argument(
                away_message::INVALID_MESSAGE_LENGTH,
            ));
        }
        // No pause check: the pause covers the phone, and at the desk the line is said.
        let pane =
            self.monitor
                .resolve_agent_pane(req.src_id)
                .await
                .map_err(|error| match error {
                    agent::runtime::MonitorCommandError::InvalidInput => {
                        Status::invalid_argument(away_message::INVALID_SOURCE_ID)
                    }
                    agent::runtime::MonitorCommandError::AgentNotFound => {
                        Status::failed_precondition(away_message::AGENT_NOT_FOUND)
                    }
                    agent::runtime::MonitorCommandError::InventoryUnavailable
                    | agent::runtime::MonitorCommandError::EventAppend(_)
                    | agent::runtime::MonitorCommandError::RuntimeStopped => {
                        Status::unavailable("agent lookup unavailable")
                    }
                })?;
        let event = store::SpokenLineRequested {
            pane_id: pane.incarnation.pane_id,
            text,
        };
        store::append_spoken_line_requested(&self.store, &event)
            .await
            .map_err(|error| {
                tracing::error!(pane_id = %event.pane_id, result = "append_failed", error = %error, "spoken line rejected");
                Status::internal("event store write failed")
            })?;
        // The text is the chief's own words: it is stored, not logged.
        info!(pane_id = %event.pane_id, result = "accepted", "spoken line stored");
        Ok(Response::new(SpeakLineResponse {}))
    }

    async fn call(&self, request: Request<CallRequest>) -> Result<Response<CallResponse>, Status> {
        let action = harold::CallAction::try_from(request.into_inner().action)
            .unwrap_or(harold::CallAction::Unspecified);
        if action == harold::CallAction::Unspecified {
            return Err(Status::invalid_argument("call action is required"));
        }
        let Some(calls) = voice_input::daemon_calls() else {
            return Ok(Response::new(call_response(&voice_input::CallStatus {
                state: voice_input::CallState::NotSetUp,
                chief_pane_id: None,
                last_transcript: None,
                last_result: None,
            })));
        };
        let status = match action {
            harold::CallAction::Start => calls.start().await,
            harold::CallAction::Stop => calls.stop().await,
            harold::CallAction::TalkNow => calls.talk_now(),
            harold::CallAction::Status | harold::CallAction::Unspecified => calls.status(),
        };
        Ok(Response::new(call_response(&status)))
    }
}

/// A call's status as the `Call` RPC answers it.
fn call_response(status: &voice_input::CallStatus) -> CallResponse {
    use voice_input::{CallState, TurnResult};
    let state = match status.state {
        CallState::NotSetUp => harold::CallState::NotSetUp,
        CallState::Off => harold::CallState::Off,
        CallState::Listening => harold::CallState::Listening,
        CallState::Transcribing => harold::CallState::Transcribing,
        CallState::Waiting => harold::CallState::Waiting,
        CallState::Speaking => harold::CallState::Speaking,
    };
    let last_result = match status.last_result {
        None => harold::CallResult::Unspecified,
        Some(TurnResult::Typed) => harold::CallResult::Typed,
        Some(TurnResult::NothingHeard) => harold::CallResult::NothingHeard,
        Some(TurnResult::NoChief) => harold::CallResult::NoChief,
        Some(TurnResult::ChiefBusy) => harold::CallResult::ChiefBusy,
        Some(TurnResult::Failed) => harold::CallResult::Failed,
        Some(TurnResult::VoiceMuted) => harold::CallResult::VoiceMuted,
    };
    CallResponse {
        state: state as i32,
        chief_pane_id: status.chief_pane_id.clone().unwrap_or_default(),
        last_transcript: status.last_transcript.clone().unwrap_or_default(),
        last_result: last_result as i32,
    }
}

async fn forward_agent_snapshots(
    mut snapshots: watch::Receiver<agent::domain::AgentSnapshot>,
    sender: mpsc::Sender<Result<AgentStateSnapshot, Status>>,
    mut shutdown: watch::Receiver<()>,
) {
    if shutdown.has_changed().unwrap_or(true) {
        return;
    }

    let initial = map_agent_snapshot(snapshots.borrow_and_update().clone());
    if !send_agent_snapshot(&sender, initial, &mut shutdown).await {
        return;
    }

    loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => break,
            _ = sender.closed() => break,
            changed = snapshots.changed() => {
                if changed.is_err() {
                    break;
                }
                let snapshot = map_agent_snapshot(snapshots.borrow_and_update().clone());
                if !send_agent_snapshot(&sender, snapshot, &mut shutdown).await {
                    break;
                }
            }
        }
    }
}

async fn send_agent_snapshot(
    sender: &mpsc::Sender<Result<AgentStateSnapshot, Status>>,
    snapshot: AgentStateSnapshot,
    shutdown: &mut watch::Receiver<()>,
) -> bool {
    tokio::select! {
        biased;
        _ = shutdown.changed() => false,
        result = sender.send(Ok(snapshot)) => result.is_ok(),
    }
}

fn map_agent_snapshot(snapshot: agent::domain::AgentSnapshot) -> AgentStateSnapshot {
    AgentStateSnapshot {
        tts_muted: snapshot.tts_muted,
        messaging_paused: snapshot.messaging_paused,
        harold_version: HAROLD_VERSION.to_owned(),
        through_event_version: snapshot.through_event_version.get(),
        server_time_ms: snapshot.server_time_ms,
        monitor_health: snapshot
            .monitor_health
            .into_iter()
            .map(|health| AgentMonitorHealth {
                component: health.component,
                state: if health.healthy {
                    MonitorHealthState::Healthy.into()
                } else {
                    MonitorHealthState::Degraded.into()
                },
                reason_code: health.reason_code,
                observed_at_ms: health.observed_at_ms,
            })
            .collect(),
        panes: snapshot
            .panes
            .into_iter()
            .map(|projection| {
                let pane = projection.pane;
                AgentPaneState {
                    pane_id: pane.incarnation.pane_id,
                    tmux_target: pane.tmux_target,
                    session_name: pane.session_name,
                    window_index: pane.window_index,
                    pane_index: pane.pane_index,
                    pane_pid: pane.incarnation.pane_pid,
                    agent_pid: pane.incarnation.agent_pid,
                    agent_started_at_ms: pane.incarnation.agent_started_at_ms,
                    provider_id: pane.incarnation.provider_id,
                    provider_display_name: pane.provider_display_name,
                    working_directory: pane.working_directory,
                    state: match projection.effective_state {
                        agent::domain::EffectiveAgentState::Busy => AgentState::Busy.into(),
                        agent::domain::EffectiveAgentState::Idle => AgentState::Idle.into(),
                        agent::domain::EffectiveAgentState::Unknown => AgentState::Unknown.into(),
                    },
                    last_transition_at_ms: projection.last_transition_at_ms,
                    work_summary: projection.work_summary,
                }
            })
            .collect(),
    }
}

/// `value` trimmed, or `None` when that is empty: an old hook sends an empty string for a
/// field it does not know.
fn non_empty(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

fn pane_id_for_log(value: &str) -> &str {
    let valid = value.len() <= 32
        && value.strip_prefix('%').is_some_and(|suffix| {
            !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
        });
    if valid { value } else { "<invalid-pane-id>" }
}

async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut sigint = signal(SignalKind::interrupt()).expect("failed to install SIGINT handler");
    let mut sigterm = signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
    tokio::select! {
        _ = sigint.recv() => info!("received SIGINT"),
        _ = sigterm.recv() => info!("received SIGTERM"),
    }
}

fn run_diagnostics(delay_secs: u64) {
    use outbound::{is_screen_locked, tts::notify_at_desk};
    use store::TurnCompleted;

    let turn = TurnCompleted {
        pane_id: "diag".into(),
        pane_label: "harold:0.0".into(),
        last_user_prompt: "diagnostic test".into(),
        assistant_message: "Harold diagnostic test complete.".into(),
        main_context: "harold".into(),
        agent_incarnation: None,
        work_summary: agent::domain::CompletionSummaryUpdate::Unchanged,
    };

    println!("=== Harold diagnostics ===\n");

    if delay_secs > 0 {
        println!("Waiting {delay_secs}s — lock your screen now...");
        std::thread::sleep(std::time::Duration::from_secs(delay_secs));
    }

    let locked = is_screen_locked();
    println!("screen_locked : {locked}");

    let cfg = get_settings();
    println!("away_channel  : {}", cfg.notify.away_channel);
    println!(
        "iMessage      : recipient={} handle_ids={:?}",
        cfg.imessage.recipient.as_deref().unwrap_or("(not set)"),
        cfg.imessage.handle_ids,
    );
    println!(
        "Telegram      : bot_token={} chat_id={}",
        if cfg.telegram.bot_token.is_some() {
            "(set)"
        } else {
            "(not set)"
        },
        cfg.telegram
            .chat_id
            .map_or("(not set)".to_string(), |id| id.to_string()),
    );
    println!(
        "TTS           : command={} voice={:?}",
        cfg.tts.command, cfg.tts.voice,
    );
    println!(
        "AI cli        : {:?}",
        cfg.ai.cli_path.as_deref().unwrap_or("(not set)"),
    );

    println!("\n--- Testing semantic resolver ---");
    let panes = inbound::scan_live_panes();
    let pane_labels: Vec<&str> = panes.iter().map(|p| p.label()).collect();
    println!("live panes    : {pane_labels:?}");

    let test_phrases = ["to my agent, hi", "ask harold to check logs", "hi"];
    for phrase in &test_phrases {
        let result = inbound::semantic_resolve(phrase, &panes);
        match result {
            Some((idx, cleaned)) => {
                println!(
                    "  \"{phrase}\" → {} (cleaned: \"{cleaned}\")",
                    panes[idx].label()
                );
            }
            None => {
                println!("  \"{phrase}\" → none");
            }
        }
    }

    println!("\n--- Testing notify path (screen_locked={locked}) ---");
    if !locked {
        println!("Running TTS...");
        let _ = notify_at_desk(&turn, "diag");
        println!("TTS done");
        return;
    }

    println!(
        "Sending away notification via {}...",
        cfg.notify.away_channel
    );
    // Run in a separate thread so the blocking reqwest client
    // doesn't panic when dropped inside the async runtime.
    let diag_turn = turn.clone();
    std::thread::spawn(move || {
        if let Err(error) = channels::notify_away(&diag_turn, "diag") {
            eprintln!("Away notification failed: {error}");
        }
    })
    .join()
    .expect("diag away channel thread");
    println!("Away notification sent (check your phone)");

    println!("\nDone.");
}

/// Print the one line a command failed with and exit 1. Returning the error from `main`
/// would print it as `Error: "…"`.
fn exit_with_line(line: &str) -> ! {
    eprintln!("{line}");
    std::process::exit(1)
}

/// Logs why the store at `path` could not be opened and returns the line Harold stops with:
/// the error's own text, such as `store is in use by another Harold`. Returning the error
/// from `main` would print its Debug form instead, such as `Error: InUse`.
fn report_store_open_failure(path: &str, error: &store::StoreError) -> String {
    let line = error.to_string();
    tracing::error!(path, error = %line, "store could not be opened");
    line
}

fn print_help() {
    println!("harold — agent notification and inbound message routing daemon\n");
    println!("USAGE:");
    println!("  harold                  Start the Harold daemon");
    println!("  harold --check-config   Validate config and print address/store JSON; no writes");
    println!("  harold --check-ready    Read one agent snapshot; 5s timeout, no writes");
    println!("  harold accept EVENT --pane ID --adapter NAME");
    println!("                          Report a pane busy (turn-started, still-working) or idle");
    println!("                          (turn-completed, subagents-finished); 3s timeout");
    println!("  harold send-message-to-away-device --src-id ID TEXT");
    println!("                          Send TEXT to the away device from tmux pane ID;");
    println!("                          exit 0 once the daemon has stored it; 3s timeout");
    println!("  harold speak --src-id ID TEXT");
    println!("                          Say TEXT from tmux pane ID, or send it as a voice note;");
    println!("                          exit 0 once the daemon has stored it; 3s timeout");
    println!("  harold tool questions put");
    println!(
        "                          Add a question from JSON on stdin; prints its id and version"
    );
    println!("  harold tool questions dismiss ID --reason TEXT");
    println!("                          Withdraw a question, with a reason the dash shows");
    println!("  harold tool questions order ID...   Put the listed questions first, in order");
    println!("  harold tool questions list          List the waiting questions");
    println!("  harold tool backlog put PRODUCT");
    println!("                          Add or replace an item from JSON on stdin");
    println!("  harold tool backlog rank PRODUCT ID...  Put the listed items first, in order");
    println!("  harold tool backlog remove PRODUCT ID [--reason TEXT]");
    println!("                          Remove an item; a reason is kept as a history note");
    println!("  harold tool backlog list PRODUCT [--json]   List a product's ranked items");
    println!("  harold tool backlog note add PRODUCT KIND");
    println!("                          Add a rule, history, held or left_out note from JSON");
    println!("  harold tool backlog note update|remove PRODUCT NOTE-ID");
    println!("                          Change a note from JSON on stdin, or remove it");
    println!("  harold tool backlog notes PRODUCT [--kind KIND] [--last N]");
    println!("                          List a product's notes, of one kind or the last N");
    println!("  harold tool backlog queue add [--now | --at N]");
    println!("                          Add text on stdin to the queue; parked at the end");
    println!("  harold tool backlog queue now ID    Make ID Now; the old Now is parked first");
    println!("  harold tool backlog queue order ID...   Put the listed parked items first");
    println!("  harold tool backlog queue update ID Replace an item's text from stdin");
    println!("  harold tool backlog queue done ID   Mark a queue item done");
    println!("  harold tool backlog queue note      Set the queue's note from stdin");
    println!("  harold tool backlog queue list [--json] [--done-since YYYY-MM-DD]");
    println!("                          List Now, the parked, and the done since midnight");
    println!("  harold tool backlog import PRODUCT FILE [--apply]");
    println!("                          Load backlog.md, dry run unless --apply; 30s timeout");
    println!("  harold tool backlog queue import FILE [--apply]");
    println!("                          Load queue.md, dry run unless --apply; 30s timeout");
    println!("  harold tool backlog export PRODUCT");
    println!("                          Print a product's backlog as markdown; 30s timeout");
    println!("  harold tool backlog queue export    Print the queue as markdown; 30s timeout");
    println!("  harold --diagnostics [--delay [N]]  Test screen lock, TTS, and iMessage config");
    println!("                                      --delay defaults to 10s if no value given");
    println!("  harold --help           Show this help\n");
    println!("ENVIRONMENT:");
    println!("  HAROLD_CONFIG_DIR       Config directory (default: config/ next to the binary)");
    println!("  HAROLD_ENV              Config environment overlay (default: local)");
    println!("  HAROLD__*               Override any config key via env var");
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = match cli::Mode::parse(&args) {
        Ok(mode) => mode,
        Err(line)
            if matches!(
                args.first().map(String::as_str),
                Some(cli::AWAY_MESSAGE_COMMAND | cli::SPEAK_COMMAND | cli::TOOL_COMMAND)
            ) =>
        {
            exit_with_line(&line)
        }
        Err(error) => return Err(error.into()),
    };

    if mode == cli::Mode::Help {
        print_help();
        return Ok(());
    }

    // Read before the runtime starts threads: the `time` crate refuses it after.
    telemetry::set_local_offset(
        time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC),
    );
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async_main(mode))
}

async fn async_main(mode: cli::Mode) -> Result<(), Box<dyn std::error::Error>> {
    let is_probe = matches!(
        mode,
        cli::Mode::CheckConfig
            | cli::Mode::CheckReady
            | cli::Mode::Accept(_)
            | cli::Mode::SendMessageToAwayDevice(_)
            | cli::Mode::Speak(_)
            | cli::Mode::Tool(_)
    );
    let launchd = started_by_launchd(&mode);
    let settings = match settings::Settings::load() {
        Ok(settings) => settings,
        Err(_) if is_probe => return Err("invalid configuration".into()),
        Err(error) if launchd => refuse_invalid_config(&error.to_string()),
        Err(error) => return Err(error.into()),
    };

    let errors = settings.validate();
    if !errors.is_empty() {
        if is_probe {
            return Err("invalid configuration".into());
        }
        let message = format!("invalid configuration: {}", errors.join("; "));
        if launchd {
            refuse_invalid_config(&message);
        }
        return Err(message.into());
    }

    match mode {
        cli::Mode::CheckConfig => return cli::check_config(&settings),
        cli::Mode::CheckReady => return cli::check_ready(&settings).await,
        cli::Mode::Accept(ref args) => return cli::accept(&settings, args).await,
        cli::Mode::SendMessageToAwayDevice(ref args) => {
            if let Err(line) = cli::send_message_to_away_device(&settings, args).await {
                exit_with_line(&line);
            }
            return Ok(());
        }
        cli::Mode::Speak(ref args) => {
            if let Err(line) = cli::speak(&settings, args).await {
                exit_with_line(&line);
            }
            return Ok(());
        }
        cli::Mode::Tool(ref command) => {
            let address = settings.grpc.addr()?.to_string();
            match cli::tool(&address, command, std::io::stdin()).await {
                Ok(line) => println!("{line}"),
                Err(line) => exit_with_line(&line),
            }
            return Ok(());
        }
        _ => {}
    }

    let telemetry = init_telemetry(
        &settings.log.level,
        &settings.telemetry,
        &settings.machine_name,
    );
    if telemetry.exporting() {
        crate::telemetry::register_phone_counters();
    }
    init_settings(settings);
    let cfg = get_settings();

    if let cli::Mode::Diagnostics { delay_seconds } = mode {
        run_diagnostics(delay_seconds);
        telemetry.shutdown().await;
        return Ok(());
    }

    let store_path = cfg.store.resolved_path();
    // Under launchd, count this start if the last run did not exit cleanly, and refuse it
    // after a crash loop. A crash file that cannot be written leaves the guard off.
    let mut guard = None;
    if launchd {
        let file = crash_guard::crash_file(std::path::Path::new(&store_path));
        match crash_guard::begin(&file, std::time::SystemTime::now()) {
            Ok(crash_guard::Start::Run(started)) => guard = Some(started),
            Ok(crash_guard::Start::Refuse) => refuse_start("crash_loop", telemetry).await,
            Err(error) => tracing::warn!(error = %error, "crash guard unavailable"),
        }
    }
    let store = match store::open_store(&store_path).await {
        Ok(store) => store,
        Err(error) => {
            let line = report_store_open_failure(&store_path, &error);
            if let (true, Some(reason)) = (launchd, store_refusal_reason(&error)) {
                // A refused start is not a crash: it must not count toward a crash loop.
                if let Some(guard) = guard {
                    mark_clean(guard);
                }
                refuse_start(reason, telemetry).await;
            }
            telemetry.shutdown().await;
            exit_with_line(&line);
        }
    };

    let addr = cfg.grpc.addr()?;

    // Shutdown channel: sender closes on signal, receivers see the channel close.
    let (shutdown_tx, shutdown_rx) = watch::channel(());

    let inventory: Arc<dyn agent::inventory::AgentInventoryPort> = Arc::new(
        agent::inventory::TmuxAgentInventory::new(cfg.agents.clone()),
    );
    let screen: Arc<dyn agent::screen::VisibleScreenPort> =
        Arc::new(agent::screen::TmuxVisibleScreen::new());
    let providers = cfg.agents.0.clone();
    let initial_agent_snapshot = restore_startup_state(&store).await?;
    let snapshots = agent::snapshot::AgentSnapshotHub::new(initial_agent_snapshot.clone());

    let activity_provider = cfg.activity_summary.enabled.then(|| {
        Arc::new(activity_summary::ClaudeActivitySummarizer::new(
            cfg.activity_summary.clone(),
        )) as Arc<dyn activity_summary::ActivitySummarizer>
    });
    let (monitor, monitor_task) = agent::runtime::spawn_agent_monitor(
        Arc::clone(&store),
        inventory,
        screen,
        providers,
        initial_agent_snapshot,
        agent::runtime::AgentMonitorRuntimeConfig {
            activity_summary: activity_provider
                .as_ref()
                .map(|provider| (Arc::clone(provider), cfg.activity_summary.clone())),
            inventory_interval: Duration::from_millis(cfg.agent_monitor.inventory_interval_ms),
            screen_interval: Duration::from_millis(cfg.agent_monitor.screen_interval_ms),
            hook_grace_ms: cfg.agent_monitor.hook_grace_ms,
            report_grace_ms: cfg.agent_monitor.report_grace_ms,
            inventory_timeout: Duration::from_millis(cfg.agent_monitor.inventory_timeout_ms),
            acquisition_timeout: Duration::from_millis(500),
        },
        shutdown_rx.clone(),
    );

    let telemetry_tasks = start_telemetry_tasks(cfg, &snapshots, &shutdown_rx).await;
    if let Some(queue) = &telemetry_tasks.alert_queue {
        store.set_alert_queue(queue.clone());
    }
    // The desk's calls come first: they sweep leftover `call-*` folders before either
    // control can start a call.
    let call_ports = voice_input::MacPorts::from_settings(cfg, snapshots.clone());
    let set_up = call_ports.set_up();
    voice_input::init_daemon_calls(voice_input::CallControl::new(call_ports, set_up));

    // The phone's calls exist only with the web app. One value serves the web app's call
    // routes and the chief's spoken lines.
    let phone_calls = cfg.web_app.as_ref().map(|_| {
        let ports = voice_input::MacPorts::from_settings(cfg, snapshots.clone());
        let set_up = ports.set_up();
        let end_desk: web_app::call::EndDeskCall = Arc::new(|| {
            Box::pin(async {
                if let Some(calls) = voice_input::daemon_calls() {
                    calls.stop().await;
                }
            })
        });
        Arc::new(web_app::call::AppCalls::new(
            voice_input::PhoneCalls::new(ports, set_up),
            Arc::clone(&store),
            end_desk,
        ))
    });
    if let Some(calls) = &phone_calls {
        voice_input::init_phone_calls(Arc::clone(calls) as Arc<dyn voice_input::LineTaker>);
    }
    let web_app = web_app::start_if_set(
        cfg.web_app.as_ref(),
        Arc::clone(&store),
        std::path::Path::new(&store_path),
        shutdown_rx.clone(),
        phone_calls.map(|calls| calls as Arc<dyn web_app::call::PhoneCallApi>),
    )
    .await;
    let event_handler_handle = tokio::spawn(projector::run_event_handler_with_app(
        Arc::clone(&store),
        snapshots.clone(),
        telemetry_tasks.alert_queue.clone(),
        web_app.as_ref().map(|running| Arc::clone(&running.route)),
        shutdown_rx.clone(),
    ));
    let listener_handle = tokio::spawn(channels::listen_for_inbound_messages(
        Arc::clone(&store),
        shutdown_rx.clone(),
    ));
    let timed_prompts_handle = timed_prompts::start(
        cfg,
        Arc::clone(&store),
        snapshots.clone(),
        shutdown_rx.clone(),
    )
    .await?;
    // The tools' services join the core service on one server. A tool that cannot start
    // is logged and left out.
    let tools = tools::start(cfg, &store_path, snapshots.clone(), shutdown_rx.clone()).await;

    info!(address = %addr, "Harold listening");
    let service = harold_service(
        cfg,
        monitor,
        Arc::clone(&store),
        snapshots,
        shutdown_rx.clone(),
    );
    let server_result = Server::builder()
        .add_routes(tools.routes.clone())
        .add_service(HaroldServer::new(service))
        .serve_with_shutdown(addr, async {
            shutdown_signal().await;
            info!("shutting down");
            // Drop the sender to signal all receivers.
            drop(shutdown_tx);
        })
        .await;

    // An open call ends with reason `shutdown`, and its folder and child programs go.
    if let Some(calls) = voice_input::daemon_calls() {
        calls.shutdown().await;
    }

    // Wait for the handler, the listener, the timed-prompt task, the tools, the web app, the
    // pod facts and the alert address to stop before returning.
    let _ = event_handler_handle.await;
    let _ = listener_handle.await;
    let _ = timed_prompts_handle.await;
    tools.stopped().await;
    if let Some(running) = web_app {
        let _ = running.handle.await;
    }
    telemetry_tasks.stopped().await;
    stop_agent_monitor(monitor_task, activity_provider, Duration::from_secs(1)).await;
    if server_result.is_ok()
        && let Some(guard) = guard
    {
        mark_clean(guard);
    }
    telemetry.shutdown().await;
    server_result?;
    Ok(())
}

/// Whether launchd started this daemon: its plist sets `HAROLD_LAUNCHD=1`. Then a start that
/// must not be retried exits 0, so `KeepAlive` does not restart it; any other way, it exits 1.
fn started_by_launchd(mode: &cli::Mode) -> bool {
    *mode == cli::Mode::Daemon
        && std::env::var_os("HAROLD_LAUNCHD").is_some_and(|value| value == "1")
}

/// The reason a store that cannot be opened refuses a start under launchd, or `None` when
/// opening it again might work.
fn store_refusal_reason(error: &store::StoreError) -> Option<&'static str> {
    match error {
        store::StoreError::InUse => Some("store_in_use"),
        store::StoreError::Data(text) if text == "incompatible state schema" => {
            Some("store_incompatible")
        }
        _ => None,
    }
}

/// Logs why launchd's start is refused and exits 0. Telemetry is shut down first, so the line
/// is exported when the exporter is on.
async fn refuse_start(reason: &'static str, telemetry: telemetry::TelemetryGuard) -> ! {
    tracing::warn!(reason, "startup refused");
    telemetry.shutdown().await;
    std::process::exit(0)
}

/// Refuses launchd's start of a configuration that does not validate. Logging is not set up
/// yet, so the error and the refusal go to standard error, which the plist sends to the log.
fn refuse_invalid_config(message: &str) -> ! {
    eprintln!("Error: {message}");
    let subscriber = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        tracing::warn!(reason = "invalid_config", "startup refused");
    });
    std::process::exit(0)
}

fn mark_clean(guard: crash_guard::CrashGuard) {
    if let Err(error) = guard.mark_clean() {
        tracing::warn!(error = %error, "crash guard could not record a clean exit");
    }
}

/// The pod facts and the alert address, each started only when its `[telemetry]` key is
/// set. With neither set, nothing starts: no `git` command runs and no address is opened.
struct TelemetryTasks {
    pod_stats: Option<tokio::task::JoinHandle<()>>,
    alerts: Option<tokio::task::JoinHandle<()>>,
    alert_queue: Option<alerts::AlertQueue>,
}

impl TelemetryTasks {
    /// Waits for the pod facts and the alert address to stop, after shutdown.
    async fn stopped(self) {
        if let Some(handle) = self.pod_stats {
            let _ = handle.await;
        }
        if let Some(handle) = self.alerts {
            let _ = handle.await;
        }
    }
}

/// Starts the pod facts when `pod_registry` is set and opens the alert address when
/// `alert_listen` is set. An address that cannot be listened on, such as a port another
/// program holds, logs a warning and Harold runs with the alert address off.
async fn start_telemetry_tasks(
    cfg: &settings::Settings,
    snapshots: &agent::snapshot::AgentSnapshotHub,
    shutdown_rx: &watch::Receiver<()>,
) -> TelemetryTasks {
    // The pod facts are read only when the chief's registry is named.
    let pod_stats = cfg.telemetry.pod_registry_path().map(|registry| {
        pod_stats::start(
            registry,
            pod_stats::tips_file(std::path::Path::new(&cfg.store.resolved_path())),
            shutdown_rx.clone(),
        )
    });
    // The alert address is opened only when `alert_listen` is set.
    let (alerts, alert_queue) = match cfg.telemetry.alert_addr() {
        Some(alert_addr) => alerts::start(
            alert_addr,
            cfg.notify.away_messages_only_from.clone(),
            timed_prompts::TmuxPorts::new(snapshots.clone(), cfg.agents.0.clone()),
            shutdown_rx.clone(),
        )
        .await
        .inspect_err(|error| {
            tracing::warn!(
                reason = "bind_failed",
                address = %alert_addr,
                error = %error,
                "alert address unavailable"
            );
        })
        .ok()
        .unzip(),
        None => (None, None),
    };
    TelemetryTasks {
        pod_stats,
        alerts,
        alert_queue,
    }
}

async fn stop_agent_monitor(
    mut monitor_task: tokio::task::JoinHandle<()>,
    activity_provider: Option<Arc<dyn activity_summary::ActivitySummarizer>>,
    deadline: Duration,
) {
    if tokio::time::timeout(deadline, &mut monitor_task)
        .await
        .is_err()
    {
        tracing::warn!("agent monitor did not stop within shutdown deadline; aborting task");
        monitor_task.abort();
        let _ = monitor_task.await;
    }
    // The scheduler may have been dropped during a blocked acquisition. Keep
    // subprocess cleanup owned here until every admitted request has finished.
    if let Some(provider) = activity_provider {
        provider.shutdown().await;
    }
}

/// Loads the caught-up snapshot and sets the outbound switches from it, so the persisted
/// mute and pause hold before any listener or handler starts.
async fn restore_startup_state(
    store: &store::HaroldStore,
) -> store::Result<agent::domain::AgentSnapshot> {
    let snapshot = load_startup_agent_snapshot(store).await?;
    outbound::set_tts_muted(snapshot.tts_muted);
    outbound::set_messaging_paused(snapshot.messaging_paused);
    Ok(snapshot)
}

async fn load_startup_agent_snapshot(
    store: &store::HaroldStore,
) -> store::Result<agent::domain::AgentSnapshot> {
    loop {
        let batch = store.project_unhandled_events(500).await?;
        if batch.applied == 0 {
            return store.load_agent_snapshot().await;
        }
    }
}

#[cfg(test)]
#[path = "main_tests.rs"]
mod tests;
