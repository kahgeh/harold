mod activity_summary;
mod agent;
mod alerts;
mod away_message;
mod build_features;
mod channels;
mod cli;
mod clickhouse_read;
mod crash_guard;
mod cron;
mod inbound;
mod log_trim;
mod machine_stats;
mod outbound;
mod pod_roles;
mod pod_stats;
mod projector;
mod remote_order;
mod restart_reminder;
mod settings;
mod slave;
mod store;
mod telemetry;
mod timed_prompts;
mod tools;
mod transcript;
mod tts;
mod util;
#[cfg(feature = "calls")]
mod voice_input;
#[cfg(not(feature = "calls"))]
#[path = "voice_input/off.rs"]
mod voice_input;
mod web_app;

use std::sync::Arc;
use std::time::Duration;

use settings::{get_settings, init_settings};
use telemetry::init_telemetry;
use tokio::sync::{mpsc, watch};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, transport::Server};
use tracing::{Instrument, info, info_span, warn};
use tracing_opentelemetry::OpenTelemetrySpanExt as _;

pub use harold_api::harold;

use harold::harold_server::{Harold, HaroldServer};
use harold::{
    AgentMonitorHealth, AgentPaneState, AgentSeenRequest, AgentSeenResponse, AgentState,
    AgentStateSnapshot, CallRequest, CallResponse, GetHaroldStateRequest, GetHaroldStateResponse,
    GetRemoteOrderRequest, GetRemoteOrderResponse, MonitorHealthState, PodAgentStartedRequest,
    PodAgentStartedResponse, ReportAgentStateRequest, ReportAgentStateResponse, RexHostIcon,
    SendMessageToAwayDeviceRequest, SendMessageToAwayDeviceResponse, SetMessagingPausedRequest,
    SetMessagingPausedResponse, SetRemoteOrderRequest, SetRemoteOrderResponse, SetTtsMutedRequest,
    SetTtsMutedResponse, SpeakLineRequest, SpeakLineResponse, TurnCompleteRequest,
    TurnCompleteResponse, WatchAgentStatesRequest,
};

/// Release tag compiled in by the release workflow; "source" for any other build.
const HAROLD_VERSION: &str = version_or_source(option_env!("HAROLD_RELEASE_VERSION"));

/// The tag's own time, compiled in by the release workflow. `None` for a build from source.
const HAROLD_RELEASE_TIME: Option<&str> = option_env!("HAROLD_RELEASE_TIME");

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
    /// Told of each turn end once it is stored. `None` when `[restart_reminder]` is off.
    restart_reminder: Option<Arc<dyn restart_reminder::TurnEnds>>,
    /// The current steps from the Rex status records. Empty for tmux.
    current_steps: agent::rex::CurrentSteps,
    /// The recap of each agent, read from its transcript.
    recaps: agent::recap::Recaps,
    /// The agents on the Rex hosts in `rex_hosts`, in memory only. Empty with no host.
    host_agents: agent::rex_host_runner::HostAgents,
    /// The window name of each agent pane of this Mac, from the scans. In memory only.
    window_names: agent::inventory::WindowNames,
    /// The mark name of Harold's own machine from `rex_host_marks`, or empty with no entry.
    local_icon: String,
    /// The settings values that `GetHaroldState` reports, read once at start.
    state_view: HaroldStateView,
    /// The watch counts the watch tool pushes. The stream puts the newest in each snapshot.
    watch_counts: watch::Receiver<tools::WatchCounts>,
    /// The agents that wait for the user, and whether Harold knows the focused pane.
    waiting: watch::Receiver<agent::waiting::WaitingView>,
    /// Sends the pane id of each `AgentSeen` call to the waiting task.
    seen_calls: mpsc::Sender<String>,
}

/// The settings values that `GetHaroldState` reports. It holds no other setting, so a
/// secret or a path cannot reach the answer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct HaroldStateView {
    machine_name: String,
    /// `tmux` or `rex`.
    multiplexer: String,
    /// Each label in `rex_hosts` with its host icon, which is empty with no entry.
    rex_hosts: Vec<(String, String)>,
    /// The host icon of this machine, or empty with no entry.
    machine_host_icon: String,
    /// `server` or `command`.
    tts_engine: String,
    /// The chief's session name, or empty with none.
    chief_session: String,
    /// Where the role of each pane comes from.
    roles: RoleSources,
    /// The order of the PnEMs in the remote control. `None` when the data folder is unknown.
    remote_order: Option<remote_order::RemoteOrder>,
}

impl HaroldStateView {
    fn from_settings(settings: &settings::Settings) -> Self {
        let monitor = &settings.agent_monitor;
        Self {
            machine_name: settings.machine_name.clone(),
            multiplexer: match monitor.multiplexer() {
                settings::Multiplexer::Tmux => "tmux",
                settings::Multiplexer::Rex => "rex",
            }
            .to_owned(),
            rex_hosts: monitor
                .rex_hosts
                .iter()
                .map(|label| {
                    let icon = monitor.rex_host_marks.get(label).cloned();
                    (label.clone(), icon.unwrap_or_default())
                })
                .collect(),
            machine_host_icon: monitor
                .mark_for_machine(&settings.machine_name)
                .unwrap_or_default()
                .to_owned(),
            tts_engine: match settings.tts.engine {
                settings::TtsEngine::Server => "server",
                settings::TtsEngine::Command => "command",
            }
            .to_owned(),
            chief_session: settings.notify.chief_session.clone().unwrap_or_default(),
            roles: RoleSources::from_settings(settings),
            remote_order: remote_order::RemoteOrder::in_data_folder_of(
                &settings.store.resolved_path(),
            ),
        }
    }
}

/// Where Harold finds the role of each agent pane: the chief's folder and session, and the
/// pod registry. The registry is read at each snapshot mapping, as the files are small.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RoleSources {
    chief_folder: Option<String>,
    chief_session: Option<String>,
    registry: Option<std::path::PathBuf>,
}

impl RoleSources {
    fn from_settings(settings: &settings::Settings) -> Self {
        Self {
            chief_folder: settings.notify.chief.clone(),
            chief_session: settings.notify.chief_session.clone(),
            registry: settings.telemetry.pod_registry_path(),
        }
    }

    /// The product names of the registry. No registry, or one that cannot be read, gives
    /// none.
    fn product_names(&self) -> Vec<String> {
        let Some(path) = &self.registry else {
            return Vec::new();
        };
        match pod_stats::read_registry(path) {
            Ok(products) => products.into_iter().map(|product| product.name).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// The pod panes of the registry. A registry that cannot be read gives none.
    fn pod_roles(&self) -> Vec<(String, pod_stats::Role, String)> {
        let Some(path) = &self.registry else {
            return Vec::new();
        };
        match pod_stats::read_registry(path) {
            Ok(products) => pod_stats::pod_pane_roles(&products),
            Err(_) => Vec::new(),
        }
    }
}

/// The provider id of Claude Code panes.
const CLAUDE_PROVIDER_ID: &str = "claude";

/// The service Harold serves.
#[allow(clippy::too_many_arguments)]
fn harold_service(
    monitor: agent::runtime::AgentMonitorHandle,
    store: Arc<store::HaroldStore>,
    snapshots: agent::snapshot::AgentSnapshotHub,
    shutdown: watch::Receiver<()>,
    restart_reminder: Option<Arc<dyn restart_reminder::TurnEnds>>,
    current_steps: agent::rex::CurrentSteps,
    recaps: agent::recap::Recaps,
    host_agents: agent::rex_host_runner::HostAgents,
    window_names: agent::inventory::WindowNames,
    local_icon: String,
    state_view: HaroldStateView,
    watch_counts: watch::Receiver<tools::WatchCounts>,
    waiting: watch::Receiver<agent::waiting::WaitingView>,
    seen_calls: mpsc::Sender<String>,
) -> HaroldService {
    HaroldService {
        monitor,
        store,
        snapshots,
        messaging_switch: tokio::sync::Mutex::new(None),
        tts_switch: tokio::sync::Mutex::new(None),
        shutdown,
        restart_reminder,
        current_steps,
        recaps,
        host_agents,
        window_names,
        local_icon,
        state_view,
        watch_counts,
        waiting,
        seen_calls,
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

impl slave::registry::TurnSink for TurnPath {
    fn record(&self, request: TurnCompleteRequest) -> slave::registry::TurnFuture<'_> {
        Box::pin(async move {
            TurnPath::record(self, request, None)
                .await
                .map_err(|status| status.message().to_owned())
        })
    }
}

/// The master's turn-end path: what a turn end does once its pane id is accepted. A hook's
/// `TurnComplete` call and a slave's `Turn` both go through `record`.
#[derive(Clone)]
struct TurnPath {
    monitor: agent::runtime::AgentMonitorHandle,
    snapshots: agent::snapshot::AgentSnapshotHub,
    recaps: agent::recap::Recaps,
    /// Told of each turn end once it is stored. `None` when `[restart_reminder]` is off.
    restart_reminder: Option<Arc<dyn restart_reminder::TurnEnds>>,
}

impl TurnPath {
    /// Keeps the session and transcript of the pane's agent for the recap reader. A pane
    /// that is not in this machine's snapshot, such as a slave's agent, gets no recap.
    fn note_recap_turn_end(&self, pane_id: &str, session_id: &str, transcript_path: &str) {
        if session_id.is_empty() || transcript_path.is_empty() {
            return;
        }
        let snapshot = self.snapshots.current();
        let Some(pane) = snapshot
            .panes
            .iter()
            .find(|pane| pane.pane.incarnation.pane_id == pane_id)
        else {
            return;
        };
        let incarnation = &pane.pane.incarnation;
        self.recaps.note_turn_end(
            pane_id,
            incarnation.agent_pid,
            &incarnation.provider_id,
            session_id,
            transcript_path,
        );
    }

    /// Warns once for each pane when a Claude turn end carries no session id: the Stop hook
    /// in `~/.claude/settings.json` is not Harold's `claude_turn_complete.py`.
    fn warn_when_claude_sends_no_session_id(&self, pane_id: &str, pane_id_log: &str) {
        let is_claude = self.snapshots.current().panes.iter().any(|pane| {
            pane.pane.incarnation.pane_id == pane_id
                && pane.pane.incarnation.provider_id == CLAUDE_PROVIDER_ID
        });
        // Panes already warned, for the life of the process.
        static WARNED: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
            std::sync::OnceLock::new();
        if is_claude
            && WARNED
                .get_or_init(Default::default)
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(pane_id.to_string())
        {
            tracing::warn!(pane_id = %pane_id_log, "turn hook sends no session id");
        }
    }

    /// Records one turn end. The caller has checked the pane id: a hook's pane id has a local
    /// form, a slave's has the host form. `upstream` is the turn that a paired slave sends
    /// on to its master after it keeps the turn here.
    async fn record(
        &self,
        req: TurnCompleteRequest,
        upstream: Option<TurnCompleteRequest>,
    ) -> Result<(), Status> {
        let trace_id = uuid::Uuid::new_v4().to_string();
        let span = info_span!("grpc_turn_complete", trace_id = %trace_id);
        let hook_reached = time::OffsetDateTime::now_utc();
        // Read before `req` moves into the stored event.
        let identity = pod_stats::pane_identity(&req.pane_id);
        let tokens = telemetry::TurnTokens {
            input: req.input_tokens,
            output: req.output_tokens,
            cache_read: req.cache_read_tokens,
            cache_write: req.cache_write_tokens,
        };
        let model = non_empty(&req.model).unwrap_or("none").to_owned();
        let trace = telemetry::begin_turn_complete(
            &req.pane_id,
            non_empty(&req.session_id),
            non_empty(&req.turn_started_at),
            hook_reached,
            identity.role.as_str(),
            identity.product.as_deref(),
        );
        telemetry::record_turn_tokens(
            &tokens,
            &model,
            identity.role.as_str(),
            identity.product.as_deref(),
            identity.spec_id.as_deref(),
        );
        let turn_cx = trace.turn_complete.context();
        // Kept by the pane before the monitor stores the turn end: the event handler may
        // deliver it before the monitor answers with its event id.
        telemetry::keep_pending_turn(&req.pane_id, turn_cx.clone());

        async {
            let pane_id = req.pane_id.clone();
            let pane_id_log = pane_id_for_log(&pane_id);
            info!(pane_id = %pane_id_log, "turn complete received");
            if req.session_id.is_empty() {
                self.warn_when_claude_sends_no_session_id(&pane_id, pane_id_log);
            }

            let turn_end = (
                req.pane_id.clone(),
                req.session_id.clone(),
                req.transcript_path.clone(),
            );
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
            if let Some(reminder) = &self.restart_reminder {
                reminder.turn_ended(&turn_end.0, &turn_end.1, &turn_end.2);
            }
            self.note_recap_turn_end(&turn_end.0, &turn_end.1, &turn_end.2);
            if let Some(upstream) = upstream {
                slave::slave::report_turn(upstream).await;
            }

            Ok(())
        }
        // `harold.turn_complete` is entered inside `grpc_turn_complete`, so the lines
        // written along the way carry the turn's trace.
        .instrument(trace.turn_complete)
        .instrument(span)
        .await
    }
}

impl HaroldService {
    /// The turn-end path of this service, shared with the slave link.
    fn turn_path(&self) -> TurnPath {
        TurnPath {
            monitor: self.monitor.clone(),
            snapshots: self.snapshots.clone(),
            recaps: self.recaps.clone(),
            restart_reminder: self.restart_reminder.clone(),
        }
    }

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

    /// Starts the task that builds the agent state stream and sends it on `sender`. The
    /// `WatchAgentStates` stream and the slave feed both use it.
    fn spawn_snapshot_forwarder(&self, sender: mpsc::Sender<Result<AgentStateSnapshot, Status>>) {
        let snapshots = self.snapshots.subscribe();
        let shutdown = self.shutdown.clone();
        let steps = self.current_steps.clone();
        let recaps = self.recaps.clone();
        let hosts = self.host_agents.clone();
        let window_names = self.window_names.clone();
        tokio::spawn(forward_agent_snapshots(
            snapshots,
            steps,
            recaps,
            hosts,
            window_names,
            self.local_icon.clone(),
            self.state_view.roles.clone(),
            self.watch_counts.clone(),
            self.waiting.clone(),
            sender,
            shutdown,
        ));
    }
}

#[tonic::async_trait]
impl Harold for HaroldService {
    type WatchAgentStatesStream = ReceiverStream<Result<AgentStateSnapshot, Status>>;

    async fn get_remote_order(
        &self,
        _: Request<GetRemoteOrderRequest>,
    ) -> Result<Response<GetRemoteOrderResponse>, Status> {
        let registry = self.state_view.roles.product_names();
        let products = match &self.state_view.remote_order {
            Some(order) => order.get(&registry),
            None => registry,
        };
        Ok(Response::new(GetRemoteOrderResponse { products }))
    }

    async fn set_remote_order(
        &self,
        request: Request<SetRemoteOrderRequest>,
    ) -> Result<Response<SetRemoteOrderResponse>, Status> {
        let products = request.into_inner().products;
        let saved = match &self.state_view.remote_order {
            Some(order) => order.set(&products),
            None => Err(std::io::Error::other("no data folder")),
        };
        if let Err(error) = saved {
            warn!(%error, "remote order write failed");
            return Err(Status::internal("remote order write failed"));
        }
        Ok(Response::new(SetRemoteOrderResponse {}))
    }

    /// A control-tower jumped to a pane: the waiting task clears its mark. The call never
    /// fails. For a pane that does not wait or is not known, the task does nothing.
    async fn agent_seen(
        &self,
        request: Request<AgentSeenRequest>,
    ) -> Result<Response<AgentSeenResponse>, Status> {
        let pane_id = request.into_inner().pane_id;
        // A send fails only when the waiting task stopped at shutdown: then there is no
        // mark to clear.
        let _ = self.seen_calls.send(pane_id).await;
        Ok(Response::new(AgentSeenResponse {}))
    }

    /// A hook tells Harold which agent holds a pod role. See `pod_roles`.
    async fn pod_agent_started(
        &self,
        request: Request<PodAgentStartedRequest>,
    ) -> Result<Response<PodAgentStartedResponse>, Status> {
        let request = request.into_inner();
        let start = pod_roles::PodStart {
            role: request.pod_role,
            product: request.pod_product,
            pane_id: request.pane_id,
            session_id: request.session_id,
            transcript_path: request.transcript_path,
            folder: request.cwd,
            agent_pid: request.agent_pid,
            start_file: request.start_file,
            source: request.source,
            spec: request.pod_spec,
        };
        let now_ms =
            i64::try_from(time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000)
                .unwrap_or(i64::MAX);
        match pod_roles::record_start(start, now_ms) {
            None => Err(Status::unavailable("pod tool")),
            Some(Err(_)) => Err(Status::invalid_argument("pod role or product")),
            Some(Ok(_)) => Ok(Response::new(PodAgentStartedResponse {})),
        }
    }

    async fn get_harold_state(
        &self,
        _: Request<GetHaroldStateRequest>,
    ) -> Result<Response<GetHaroldStateResponse>, Status> {
        let view = &self.state_view;
        let (call_open, call_where, call_opened_at_ms) = call_fields(
            voice_input::daemon_calls().and_then(|calls| calls.call_opened_at_ms()),
            voice_input::phone_call_opened_at_ms(),
        );
        Ok(Response::new(GetHaroldStateResponse {
            tts_muted: Some(self.snapshots.tts_muted()),
            messaging_paused: Some(self.snapshots.messaging_paused()),
            release_version: HAROLD_VERSION.to_owned(),
            machine_name: view.machine_name.clone(),
            multiplexer: view.multiplexer.clone(),
            rex_hosts: view
                .rex_hosts
                .iter()
                .map(|(label, host_icon)| RexHostIcon {
                    label: label.clone(),
                    host_icon: host_icon.clone(),
                })
                .collect(),
            machine_host_icon: view.machine_host_icon.clone(),
            tts_engine: view.tts_engine.clone(),
            chief_session: view.chief_session.clone(),
            call_open: Some(call_open),
            call_where: call_where.to_owned(),
            call_opened_at_ms: i64::try_from(call_opened_at_ms).unwrap_or(0),
        }))
    }

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
        // A hook report from a pane of the other multiplexer is dropped without an error.
        if agent::runtime::pane_id_is_other_multiplexer(&req.pane_id) {
            return Ok(Response::new(TurnCompleteResponse::default()));
        }
        // A report from a pane that has no local form is dropped before any store or log write.
        if let Err(refusal) = agent::pane_route::route_pane(&req.pane_id) {
            agent::pane_route::refuse_write(&req.pane_id, "turn_complete", refusal);
            return Ok(Response::new(TurnCompleteResponse::default()));
        }
        // A paired slave also sends the turn up to the master, after it keeps it here.
        let upstream = slave::slave::linked().then(|| req.clone());
        self.turn_path().record(req, upstream).await?;
        Ok(Response::new(TurnCompleteResponse { accepted: true }))
    }

    async fn report_agent_state(
        &self,
        request: Request<ReportAgentStateRequest>,
    ) -> Result<Response<ReportAgentStateResponse>, Status> {
        let req = request.into_inner();
        let state = match AgentState::try_from(req.state) {
            Ok(AgentState::Working) => agent::domain::ObservedAgentState::Working,
            Ok(AgentState::Blocked) => agent::domain::ObservedAgentState::Blocked,
            Ok(AgentState::Done) => agent::domain::ObservedAgentState::Done,
            Ok(AgentState::Idle) => agent::domain::ObservedAgentState::Idle,
            Ok(AgentState::Error) => agent::domain::ObservedAgentState::Error,
            Ok(AgentState::Unspecified | AgentState::Unknown) | Err(_) => {
                return Err(Status::invalid_argument("invalid agent state report"));
            }
        };
        let summary_update = match req.work_summary {
            None => agent::domain::WorkSummaryUpdate::Unchanged,
            Some(value) if value.trim().is_empty() => agent::domain::WorkSummaryUpdate::Clear,
            Some(value) => agent::domain::WorkSummaryUpdate::Set(value),
        };

        if agent::runtime::pane_id_is_other_multiplexer(&req.pane_id) {
            return Ok(Response::new(ReportAgentStateResponse { accepted: true }));
        }
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
        let (sender, receiver) = mpsc::channel(1);
        self.spawn_snapshot_forwarder(sender);
        Ok(Response::new(ReceiverStream::new(receiver)))
    }

    async fn send_message_to_away_device(
        &self,
        request: Request<SendMessageToAwayDeviceRequest>,
    ) -> Result<Response<SendMessageToAwayDeviceResponse>, Status> {
        let req = request.into_inner();
        if let Err(refusal @ agent::pane_route::PaneRefusal::OnRexHost) =
            agent::pane_route::route_pane(&req.src_id)
        {
            return Err(Status::failed_precondition(
                agent::pane_route::refuse_write(
                    &req.src_id,
                    "send_message_to_away_device",
                    refusal,
                ),
            ));
        }
        if !agent::runtime::valid_pane_id(&req.src_id) {
            return Err(Status::invalid_argument(away_message::INVALID_SOURCE_ID));
        }
        if !agent::runtime::pane_id_fits_multiplexer(&req.src_id) {
            return Err(Status::failed_precondition(
                away_message::wrong_multiplexer_message(),
            ));
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
        if let Err(refusal @ agent::pane_route::PaneRefusal::OnRexHost) =
            agent::pane_route::route_pane(&req.src_id)
        {
            return Err(Status::failed_precondition(
                agent::pane_route::refuse_write(&req.src_id, "speak_line", refusal),
            ));
        }
        if !agent::runtime::valid_pane_id(&req.src_id) {
            return Err(Status::invalid_argument(away_message::INVALID_SOURCE_ID));
        }
        if !agent::runtime::pane_id_fits_multiplexer(&req.src_id) {
            return Err(Status::failed_precondition(
                away_message::wrong_multiplexer_message(),
            ));
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
                model_percent: None,
                opened_at_ms: None,
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

/// The call fields of the state answer, from the start of the open desk call and of the open
/// phone call: whether a call is open, where, and since when. A phone call ends a desk call,
/// so both open is a moment between, and the phone's wins.
fn call_fields(desk: Option<u64>, phone: Option<u64>) -> (bool, &'static str, u64) {
    match (desk, phone) {
        (_, Some(opened)) => (true, "phone", opened),
        (Some(opened), None) => (true, "desk", opened),
        (None, None) => (false, "", 0),
    }
}

/// A call's status as the `Call` RPC answers it.
fn call_response(status: &voice_input::CallStatus) -> CallResponse {
    use voice_input::{CallState, TurnResult};
    let state = match status.state {
        CallState::NotSetUp => harold::CallState::NotSetUp,
        CallState::Off => harold::CallState::Off,
        // The `Call` RPC has no value for a call that is connecting: the contract does not
        // change, so it reads as listening, as the call does once it connects.
        CallState::Connecting | CallState::Listening => harold::CallState::Listening,
        CallState::Transcribing => harold::CallState::Transcribing,
        CallState::Waiting => harold::CallState::Waiting,
        CallState::Speaking => harold::CallState::Speaking,
    };
    let last_result = match status.last_result {
        // The `Call` RPC has no value for a turn the call answerer answered: the contract
        // does not change, so it reads as no turn typed.
        None | Some(TurnResult::Answered | TurnResult::HandedOver) => {
            harold::CallResult::Unspecified
        }
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

#[allow(clippy::too_many_arguments)]
async fn forward_agent_snapshots(
    mut snapshots: watch::Receiver<agent::domain::AgentSnapshot>,
    steps: agent::rex::CurrentSteps,
    recaps: agent::recap::Recaps,
    hosts: agent::rex_host_runner::HostAgents,
    window_names: agent::inventory::WindowNames,
    local_icon: String,
    roles: RoleSources,
    mut counts: watch::Receiver<tools::WatchCounts>,
    mut waiting: watch::Receiver<agent::waiting::WaitingView>,
    sender: mpsc::Sender<Result<AgentStateSnapshot, Status>>,
    mut shutdown: watch::Receiver<()>,
) {
    if shutdown.has_changed().unwrap_or(true) {
        return;
    }

    let mut step_versions = steps.subscribe();
    step_versions.borrow_and_update();
    let mut recap_versions = recaps.subscribe();
    recap_versions.borrow_and_update();
    let mut host_versions = hosts.subscribe();
    host_versions.borrow_and_update();
    let mut name_versions = window_names.subscribe();
    name_versions.borrow_and_update();
    let initial = map_agent_snapshot(
        snapshots.borrow_and_update().clone(),
        &steps,
        &recaps,
        &hosts,
        &window_names,
        &local_icon,
        &roles,
    );
    let initial = with_watch_counts(initial, *counts.borrow_and_update());
    let initial = with_waiting(initial, &waiting.borrow_and_update());
    let mut jobs = tools::job::subscribe();
    let initial = tools::job::with_jobs(initial, &jobs.borrow_and_update());
    let mut counts_open = true;
    let mut waiting_open = true;
    let mut jobs_open = true;
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
                let snapshot =
                    map_agent_snapshot(snapshots.borrow_and_update().clone(), &steps, &recaps, &hosts, &window_names, &local_icon, &roles);
                let snapshot = with_watch_counts(snapshot, *counts.borrow_and_update());
                let snapshot = with_waiting(snapshot, &waiting.borrow_and_update());
                let snapshot = tools::job::with_jobs(snapshot, &jobs.borrow_and_update());
                if !send_agent_snapshot(&sender, snapshot, &mut shutdown).await {
                    break;
                }
            }
            changed = step_versions.changed() => {
                if changed.is_err() {
                    break;
                }
                let snapshot = map_agent_snapshot(snapshots.borrow().clone(), &steps, &recaps, &hosts, &window_names, &local_icon, &roles);
                let snapshot = with_watch_counts(snapshot, *counts.borrow_and_update());
                let snapshot = with_waiting(snapshot, &waiting.borrow_and_update());
                let snapshot = tools::job::with_jobs(snapshot, &jobs.borrow_and_update());
                if !send_agent_snapshot(&sender, snapshot, &mut shutdown).await {
                    break;
                }
            }
            changed = recap_versions.changed() => {
                if changed.is_err() {
                    break;
                }
                let snapshot = map_agent_snapshot(snapshots.borrow().clone(), &steps, &recaps, &hosts, &window_names, &local_icon, &roles);
                let snapshot = with_watch_counts(snapshot, *counts.borrow_and_update());
                let snapshot = with_waiting(snapshot, &waiting.borrow_and_update());
                let snapshot = tools::job::with_jobs(snapshot, &jobs.borrow_and_update());
                if !send_agent_snapshot(&sender, snapshot, &mut shutdown).await {
                    break;
                }
            }
            changed = name_versions.changed() => {
                if changed.is_err() {
                    break;
                }
                let snapshot = map_agent_snapshot(snapshots.borrow().clone(), &steps, &recaps, &hosts, &window_names, &local_icon, &roles);
                let snapshot = with_watch_counts(snapshot, *counts.borrow_and_update());
                let snapshot = with_waiting(snapshot, &waiting.borrow_and_update());
                let snapshot = tools::job::with_jobs(snapshot, &jobs.borrow_and_update());
                if !send_agent_snapshot(&sender, snapshot, &mut shutdown).await {
                    break;
                }
            }
            changed = counts.changed(), if counts_open => {
                if changed.is_err() {
                    // The watch tool is gone: the last counts stay, and the stream goes on.
                    counts_open = false;
                    continue;
                }
                // A count change sends the same agent state again with the new counts,
                // at the same `through_event_version`.
                let snapshot = map_agent_snapshot(snapshots.borrow().clone(), &steps, &recaps, &hosts, &window_names, &local_icon, &roles);
                let snapshot = with_watch_counts(snapshot, *counts.borrow_and_update());
                let snapshot = with_waiting(snapshot, &waiting.borrow_and_update());
                let snapshot = tools::job::with_jobs(snapshot, &jobs.borrow_and_update());
                if !send_agent_snapshot(&sender, snapshot, &mut shutdown).await {
                    break;
                }
            }
            changed = waiting.changed(), if waiting_open => {
                if changed.is_err() {
                    // The waiting task is gone: the last view stays, and the stream goes on.
                    waiting_open = false;
                    continue;
                }
                // A waiting change sends the same agent state again with the new fields,
                // at the same `through_event_version`.
                let snapshot = map_agent_snapshot(snapshots.borrow().clone(), &steps, &recaps, &hosts, &window_names, &local_icon, &roles);
                let snapshot = with_watch_counts(snapshot, *counts.borrow_and_update());
                let snapshot = with_waiting(snapshot, &waiting.borrow_and_update());
                let snapshot = tools::job::with_jobs(snapshot, &jobs.borrow_and_update());
                if !send_agent_snapshot(&sender, snapshot, &mut shutdown).await {
                    break;
                }
            }
            changed = jobs.changed(), if jobs_open => {
                if changed.is_err() {
                    // The job tool is gone: the last jobs stay, and the stream goes on.
                    jobs_open = false;
                    continue;
                }
                // A job change raises `through_event_version` through the revision of the list.
                let snapshot = map_agent_snapshot(snapshots.borrow().clone(), &steps, &recaps, &hosts, &window_names, &local_icon, &roles);
                let snapshot = with_watch_counts(snapshot, *counts.borrow_and_update());
                let snapshot = with_waiting(snapshot, &waiting.borrow_and_update());
                let snapshot = tools::job::with_jobs(snapshot, &jobs.borrow_and_update());
                if !send_agent_snapshot(&sender, snapshot, &mut shutdown).await {
                    break;
                }
            }
            changed = host_versions.changed() => {
                if changed.is_err() {
                    break;
                }
                let snapshot = map_agent_snapshot(snapshots.borrow().clone(), &steps, &recaps, &hosts, &window_names, &local_icon, &roles);
                let snapshot = with_watch_counts(snapshot, *counts.borrow_and_update());
                let snapshot = with_waiting(snapshot, &waiting.borrow_and_update());
                let snapshot = tools::job::with_jobs(snapshot, &jobs.borrow_and_update());
                if !send_agent_snapshot(&sender, snapshot, &mut shutdown).await {
                    break;
                }
            }
        }
    }
}

/// Puts the watch tool's newest counts in `snapshot`. `map_agent_snapshot` leaves them 0:
/// the agent state does not hold them.
fn with_watch_counts(
    mut snapshot: AgentStateSnapshot,
    counts: tools::WatchCounts,
) -> AgentStateSnapshot {
    snapshot.watches_running = counts.running;
    snapshot.watches_paused = counts.paused;
    snapshot.watches_capped = counts.capped;
    snapshot
}

/// Puts the waiting agents and `focus_known` in `snapshot`, by pane id. `map_agent_snapshot`
/// leaves them empty: the agent state does not hold them.
fn with_waiting(
    mut snapshot: AgentStateSnapshot,
    view: &agent::waiting::WaitingView,
) -> AgentStateSnapshot {
    for pane in &mut snapshot.panes {
        let since = view.since_ms.get(&pane.pane_id).copied();
        pane.waiting = since.is_some();
        pane.waiting_since_ms = since.unwrap_or(0);
    }
    snapshot.focus_known = Some(view.focus_known);
    snapshot
}

/// Starts the task that decides the agents that wait for the user. It reads the focused
/// pane at each `interval` off the async threads, and applies each agent snapshot change,
/// each Rex host change and each `AgentSeen` pane id from `seen_calls` with the last focus
/// reading. It sends a new view on `views` only when the view changed.
#[allow(clippy::too_many_arguments)]
fn spawn_waiting_task(
    mut snapshots: watch::Receiver<agent::domain::AgentSnapshot>,
    hosts: agent::rex_host_runner::HostAgents,
    port: Arc<dyn agent::focus::FocusPort>,
    roles: RoleSources,
    interval: Duration,
    clock: fn() -> i64,
    views: watch::Sender<agent::waiting::WaitingView>,
    mut seen_calls: mpsc::Receiver<String>,
    mut shutdown: watch::Receiver<()>,
) -> tokio::task::JoinHandle<()> {
    use agent::waiting::{Focus, WaitingTracker};
    tokio::spawn(async move {
        if shutdown.has_changed().unwrap_or(true) {
            return;
        }
        let mut tracker = WaitingTracker::new();
        let mut focus = Focus::Unknown;
        let mut in_rex = false;
        // Whether the last focus reading failed. No reading yet is not a failed reading.
        let mut read_unknown = false;
        let mut host_versions = hosts.subscribe();
        let mut hosts_open = true;
        let mut seen_open = true;
        // The pane id of the `AgentSeen` call to apply in this pass, if any.
        let mut seen: Option<String> = None;
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                _ = shutdown.changed() => break,
                _ = ticker.tick() => {
                    in_rex = first_local_pane_in_rex(&snapshots.borrow()).unwrap_or(in_rex);
                    let reader = Arc::clone(&port);
                    let reading = tokio::task::spawn_blocking(move || {
                        agent::focus::read_focus(reader.as_ref(), in_rex)
                    })
                    .await;
                    // A reader that panicked keeps the last reading.
                    let Ok(reading) = reading else { continue };
                    focus = match reading {
                        Ok(pane) => {
                            read_unknown = false;
                            Focus::Pane(pane)
                        }
                        Err(reason) => {
                            // One line each time the focus becomes unknown, not one each tick.
                            if !read_unknown {
                                info!(reason = reason.as_str(), "focus unknown");
                            }
                            read_unknown = true;
                            Focus::Unknown
                        }
                    };
                }
                changed = snapshots.changed() => {
                    if changed.is_err() {
                        break;
                    }
                }
                changed = host_versions.changed(), if hosts_open => {
                    if changed.is_err() {
                        // The host agents are gone: their last rows stay.
                        hosts_open = false;
                    }
                    host_versions.borrow_and_update();
                }
                call = seen_calls.recv(), if seen_open => {
                    match call {
                        Some(pane_id) => seen = Some(pane_id),
                        // No sender is left: stop reading so the loop does not spin.
                        None => seen_open = false,
                    }
                }
            }
            let snapshot = snapshots.borrow_and_update().clone();
            let rows = tracked_rows(&snapshot, &hosts);
            observe_waiting(
                &mut tracker,
                &snapshot,
                &rows,
                &focus,
                seen.take().as_deref(),
                clock(),
                &roles,
                &views,
            );
        }
    })
}

/// The wall clock in milliseconds since the Unix epoch.
fn wall_clock_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        })
}

/// Whether the first pane of this machine is a Rex block, as the old control-tower read it.
/// `None` when the snapshot has no pane of this machine.
fn first_local_pane_in_rex(snapshot: &agent::domain::AgentSnapshot) -> Option<bool> {
    snapshot
        .panes
        .iter()
        .map(|projection| projection.pane.incarnation.pane_id.as_str())
        .find(|pane_id| !pane_id.starts_with("rex:"))
        .map(|pane_id| pane_id.starts_with(agent::rex::BLOCK_PREFIX))
}

/// The agents the waiting task follows: the panes of this machine, then the agents of the
/// Rex hosts, with the pane id, pid, start time and state that `map_host_agent` serves.
fn tracked_rows(
    snapshot: &agent::domain::AgentSnapshot,
    hosts: &agent::rex_host_runner::HostAgents,
) -> Vec<agent::waiting::TrackedRow> {
    use agent::waiting::TrackedRow;
    let local = snapshot.panes.iter().map(|projection| {
        let incarnation = &projection.pane.incarnation;
        TrackedRow {
            pane_id: incarnation.pane_id.clone(),
            agent_pid: incarnation.agent_pid,
            agent_started_at_ms: incarnation.agent_started_at_ms,
            state: projection.effective_state,
        }
    });
    let remote = hosts
        .snapshot()
        .into_iter()
        .map(|row| row.agent)
        .filter(|agent| agent.pid != 0)
        .map(|agent| TrackedRow {
            pane_id: agent.host_pane_id,
            agent_pid: agent.pid,
            agent_started_at_ms: agent.first_seen_ms,
            state: agent.state.effective(),
        });
    local.chain(remote).collect()
}

/// Applies the `AgentSeen` pane id `seen`, then the agent rows and the focus, to `tracker`.
/// It logs each agent that begins to wait and each agent the user sees, notes the waiting
/// counts, and sends the view when it changed. A `seen` pane that does not wait writes
/// nothing.
#[allow(clippy::too_many_arguments)]
fn observe_waiting(
    tracker: &mut agent::waiting::WaitingTracker,
    snapshot: &agent::domain::AgentSnapshot,
    rows: &[agent::waiting::TrackedRow],
    focus: &agent::waiting::Focus,
    seen: Option<&str>,
    now_ms: i64,
    roles: &RoleSources,
    views: &watch::Sender<agent::waiting::WaitingView>,
) {
    use agent::waiting::WaitingEvent;
    let chief = web_app::chief::find_chief(
        snapshot,
        roles.chief_folder.as_deref(),
        roles.chief_session.as_deref(),
    );
    let chief_pane = match chief {
        web_app::chief::ChiefPane::One { pane_id, .. } => Some(pane_id),
        _ => None,
    };
    let pod_roles = roles.pod_roles();
    // An agent of a Rex host has no role, as `map_host_agent` serves it.
    let role_and_product = |pane_id: &str| {
        if pane_id.starts_with("rex:") {
            return (String::new(), String::new());
        }
        pane_role(pane_id, chief_pane.as_deref(), &pod_roles)
    };
    let role_of = |pane_id: &str| role_and_product(pane_id).0;

    let seen_event = seen.and_then(|pane_id| tracker.seen(pane_id, now_ms));
    let events = seen_event
        .into_iter()
        .chain(tracker.observe(rows, focus, now_ms, &role_of));
    for event in events {
        match event {
            WaitingEvent::Began { pane_id, role } => {
                info!(pane_id = %pane_id, role = %role, "agent waiting");
            }
            WaitingEvent::Seen {
                pane_id,
                role,
                waited_s,
            } => {
                info!(pane_id = %pane_id, role = %role, waited_s, "agent seen");
                telemetry::record_waiting_duration(&role, waited_s);
            }
            WaitingEvent::Ended { .. } => {}
        }
    }

    let view = tracker.view();
    telemetry::note_agents_waiting(waiting_counts(rows, &view, &role_and_product));

    views.send_if_modified(|current| {
        if *current == view {
            return false;
        }
        *current = view;
        true
    });
}

/// The rows of `harold.agents.waiting`: one row for each role and product of the agent rows,
/// with 0 when no agent of it waits. `role_and_product` maps a pane id to its role and product.
fn waiting_counts(
    rows: &[agent::waiting::TrackedRow],
    view: &agent::waiting::WaitingView,
    role_and_product: &dyn Fn(&str) -> (String, String),
) -> Vec<telemetry::WaitingCount> {
    let mut counts = std::collections::BTreeMap::<(String, String), u64>::new();
    for row in rows {
        let pane_id = row.pane_id.as_str();
        let count = counts.entry(role_and_product(pane_id)).or_default();
        if view.since_ms.contains_key(pane_id) {
            *count += 1;
        }
    }
    counts
        .into_iter()
        .map(|((role, pod_product), count)| telemetry::WaitingCount {
            role,
            pod_product,
            count,
        })
        .collect()
}

/// The screen lock this machine reports to its master. Linux reads it with `pgrep` and
/// `loginctl`; the Mac keeps its own check.
fn slave_screen_locked() -> bool {
    #[cfg(target_os = "linux")]
    {
        agent::screen_lock_linux::system_screen_locked()
    }
    #[cfg(not(target_os = "linux"))]
    {
        outbound::is_screen_locked()
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

/// The stream's snapshot: the Mac's panes (each with `local_icon`), then the agents on the Rex hosts.
fn map_agent_snapshot(
    snapshot: agent::domain::AgentSnapshot,
    steps: &agent::rex::CurrentSteps,
    recaps: &agent::recap::Recaps,
    hosts: &agent::rex_host_runner::HostAgents,
    window_names: &agent::inventory::WindowNames,
    local_icon: &str,
    roles: &RoleSources,
) -> AgentStateSnapshot {
    let chief = web_app::chief::find_chief(
        &snapshot,
        roles.chief_folder.as_deref(),
        roles.chief_session.as_deref(),
    );
    let chief_pane = match chief {
        web_app::chief::ChiefPane::One { pane_id, .. } => Some(pane_id),
        _ => None,
    };
    let pod_roles = roles.pod_roles();
    AgentStateSnapshot {
        focus_known: None,
        jobs: Vec::new(),
        watches_running: 0,
        watches_paused: 0,
        watches_capped: 0,
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
                let current_step = steps.get(&pane.incarnation.pane_id, pane.incarnation.agent_pid);
                let recap = recaps.get(&pane.incarnation.pane_id, pane.incarnation.agent_pid);
                let window_name = window_names
                    .get(&pane.incarnation.pane_id)
                    .unwrap_or_default();
                let (role, pod_product) =
                    pane_role(&pane.incarnation.pane_id, chief_pane.as_deref(), &pod_roles);
                AgentPaneState {
                    waiting: false,
                    waiting_since_ms: 0,
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
                    state: proto_agent_state(projection.effective_state),
                    last_transition_at_ms: projection.last_transition_at_ms,
                    work_summary: projection.work_summary,
                    current_step,
                    recap_at_ms: recap.as_ref().map_or(0, |line| line.at_ms),
                    recap: recap.map(|line| line.text),
                    host: String::new(),
                    host_icon: local_icon.to_owned(),
                    window_name,
                    role,
                    pod_product,
                }
            })
            .chain(hosts.snapshot().into_iter().filter_map(map_host_agent))
            .collect(),
    }
}

/// The role of a pane of this Mac and the product of its pod: `chief`, `coordinator` or
/// `pnem`, or empty for any other pane.
fn pane_role(
    pane_id: &str,
    chief_pane: Option<&str>,
    pod_roles: &[(String, pod_stats::Role, String)],
) -> (String, String) {
    if chief_pane == Some(pane_id) {
        return ("chief".to_owned(), String::new());
    }
    for (pod_pane, role, product) in pod_roles {
        if pod_pane != pane_id {
            continue;
        }
        let name = match role {
            pod_stats::Role::PodCoordination => "coordinator",
            pod_stats::Role::Pnem => "pnem",
            _ => continue,
        };
        return (name.to_owned(), product.clone());
    }
    (String::new(), String::new())
}

/// The API state of an effective state: each of the six states has its own number.
fn proto_agent_state(state: agent::domain::EffectiveAgentState) -> i32 {
    use agent::domain::EffectiveAgentState as Effective;
    match state {
        Effective::Working => AgentState::Working,
        Effective::Blocked => AgentState::Blocked,
        Effective::Done => AgentState::Done,
        Effective::Idle => AgentState::Idle,
        Effective::Error => AgentState::Error,
        Effective::Unknown => AgentState::Unknown,
    }
    .into()
}

/// One agent on a Rex host as a pane of the stream. Its working directory is empty, so the
/// control-tower shows the session name. The pane pid is the agent pid on the Rex host: the
/// stream needs a nonzero pane pid, and Harold does not read the pane process of a host.
/// An agent with a zero pid has no row.
fn map_host_agent(row: agent::rex_host_runner::HostAgentRow) -> Option<AgentPaneState> {
    let agent = row.agent;
    if agent.pid == 0 {
        return None;
    }
    Some(AgentPaneState {
        waiting: false,
        waiting_since_ms: 0,
        pane_id: agent.host_pane_id,
        tmux_target: agent.session_name.clone(),
        session_name: agent.session_name,
        window_index: agent.window_index,
        pane_index: agent.pane_index,
        pane_pid: agent.pid,
        agent_pid: agent.pid,
        agent_started_at_ms: agent.first_seen_ms,
        provider_id: agent.provider_id,
        provider_display_name: agent.provider_display_name,
        working_directory: String::new(),
        state: proto_agent_state(agent.state.effective()),
        last_transition_at_ms: row.last_transition_at_ms,
        work_summary: None,
        current_step: agent.current_step,
        recap: None,
        recap_at_ms: 0,
        host: agent.host,
        host_icon: row.icon,
        window_name: agent.window_name,
        role: String::new(),
        pod_product: String::new(),
    })
}

/// `value` trimmed, or `None` when that is empty: an old hook sends an empty string for a
/// field it does not know.
fn non_empty(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

fn pane_id_for_log(value: &str) -> &str {
    let valid = value.len() <= 64
        && (agent::runtime::valid_pane_id(value)
            || agent::pane_route::parse_host_pane_id(value).is_some());
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
    use outbound::is_screen_locked;
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
    #[cfg(feature = "imessage")]
    println!(
        "iMessage      : recipient={} handle_ids={:?}",
        cfg.imessage.recipient.as_deref().unwrap_or("(not set)"),
        cfg.imessage.handle_ids,
    );
    #[cfg(not(feature = "imessage"))]
    println!("iMessage      : not built into this Harold");
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
    diagnose_away_notification(turn, locked);
}

/// Sends a test away notification through iMessage when the screen is locked.
#[cfg(feature = "imessage")]
fn diagnose_away_notification(turn: store::TurnCompleted, locked: bool) {
    if !locked {
        println!("The screen is unlocked: a turn-end update is not spoken or sent.");
        return;
    }

    println!("Sending away notification via iMessage...");
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

/// A build without `imessage` has no away channel to test.
#[cfg(not(feature = "imessage"))]
fn diagnose_away_notification(_turn: store::TurnCompleted, _locked: bool) {
    println!("This Harold has no iMessage: no away notification is sent.");
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
    println!("  harold get THING        Print one thing of the Harold state as JSON: voice,");
    println!(
        "                          messaging, version, machine, multiplexer, hosts, chief, call"
    );
    println!("  harold accept EVENT --pane ID --adapter NAME");
    println!(
        "                          Report a pane working (turn-started, still-working) or done"
    );
    println!("                          (turn-completed, subagents-finished); 3s timeout");
    println!("  harold start-control-tower [--remote] [--endpoint URI]");
    println!("                          Start the control-tower in this terminal");
    println!("  harold send-message-to-away-device --src-id ID TEXT");
    println!("                          Send TEXT to the away device from tmux or Rex pane ID;");
    println!("                          exit 0 once the daemon has stored it; 3s timeout");
    println!("  harold speak --src-id ID TEXT");
    println!(
        "                          Say TEXT from tmux or Rex pane ID at the desk, or send it as text;"
    );
    println!("                          exit 0 once the daemon has stored it; 3s timeout");
    println!("  harold app pair [--unlock]");
    println!(
        "                          Say whether web app pairing is set up and locked; --unlock"
    );
    println!("                          clears the lock and the wrong tries. Never shows the code");
    println!("  harold app devices      List the devices paired with the web app");
    println!("  harold app unpair NAME  Remove a paired device; its next request is refused");
    println!("  harold slaves pair NAME   Pair the slave NAME; prints its secret once");
    println!("  harold slaves unpair NAME Remove the secret of NAME; its next connect is refused");
    println!("  harold master pair SECRET --address HOST:PORT");
    println!("                          On a slave: keep the secret and write [master] address");
    println!("  harold tool questions put");
    println!(
        "                          Add a question from JSON on stdin; prints its id and version"
    );
    println!("  harold tool questions dismiss ID --reason TEXT");
    println!(
        "                          Withdraw a question, with a reason the control-tower shows"
    );
    println!("  harold tool questions order ID...   Put the listed questions first, in order");
    println!("  harold tool questions list          List the waiting questions");
    println!("  harold tool questions show ID       Print a question's picks");
    println!(
        "  harold tool tts models              List the TTS models, their state and the default"
    );
    println!(
        "  harold tool tts add REPO@REVISION   Ask for a TTS model; it downloads only when it is"
    );
    println!("                          on the approved list (tts-models.toml)");
    println!("  harold tool tts remove ID           Remove a TTS model (not the default)");
    println!("  harold tool tts default MODEL VOICE Set the default TTS model and voice");
    println!("  harold tool tts speak TEXT          Say TEXT at the desk through the TTS server");
    println!("  harold tool knowledge search WORDS [--limit N]");
    println!(
        "                          Search the vault by meaning: score, name, kind, description"
    );
    println!("  harold tool knowledge recent [--limit N]   The vault notes changed most recently");
    println!("  harold tool knowledge related NOTE [--limit N]");
    println!("                          The vault notes closest in meaning, linked or not");
    println!("  harold tool knowledge links NOTE   Suggested links: related, not yet linked");
    println!("  harold tool knowledge show NOTE    The vault note's text and links, as stored");
    println!("  harold tool knowledge status       Vault notes, passages, last pass, worker");
    println!("  harold tool knowledge rebuild      Clear the index and build it again");
    println!("  harold tool knowledge relate FROM RELATION TO");
    println!("                          Record a typed relation: relates_to, supersedes, part_of,");
    println!("                          inspired_by or contradicts");
    println!("  harold tool knowledge unrelate FROM RELATION TO   Remove a typed relation");
    println!("  harold tool knowledge relations NOTE   The vault note's links and typed relations");
    println!(
        "  harold tool knowledge graph status     The graph position, log ends and records waiting"
    );
    println!(
        "  harold tool knowledge graph rebuild    Drop the graph and replay it from its two logs"
    );
    println!("  harold tool knowledge graph void SEQUENCE --reason WORD");
    println!("                          Pass the one record the graph refused");
    println!(
        "  harold tool watch start NAME --skill SKILL (--every MINUTES | --cron \"EXPR\") --for MINUTES"
    );
    println!("                          [--report-to chief|self] [--run-budget USD] [--every-run]");
    println!(
        "                          Run a skill from ./.claude/skills on a schedule and report"
    );
    println!("  harold tool watch remind NAME (--in TIME | --at HH:MM | --cron \"EXPR\") \"TEXT\"");
    println!("                          Type one line once, at the time or at the next cron match");
    println!("  harold tool watch stop NAME   Stop a watch at once; types no closing line");
    println!(
        "  harold tool watch list        List the watches running or ended today, with the spend"
    );
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
    println!(
        "  harold tool telemetry declare NAME counter|gauge|event \"MEANING\" [--attr KEY=V1,V2]"
    );
    println!("                          Declare an agent signal; NAME is PREFIX.REST");
    println!("  harold tool telemetry count NAME [N] [--attr KEY=VALUE]   Add N (default 1)");
    println!("  harold tool telemetry gauge NAME VALUE [--attr KEY=VALUE]   Set a gauge");
    println!("  harold tool telemetry event NAME [--attr KEY=VALUE]   Write one event line");
    println!("  harold tool telemetry list [PREFIX]   List the declared signals");
    println!("  harold tool telemetry remove NAME   Remove a declaration; its data stays");
    println!("  harold tool work list [--all]       List the open work records, focus first");
    println!("                          --all adds the closed ones; names long waits for the user");
    println!("  harold tool work show NAME          Print a work record, its decisions and links");
    println!(
        "  harold tool work put NAME           Add or replace a work record from JSON on stdin"
    );
    println!("  harold tool work set NAME [--priority P] [--status S] [--waits-for T] [--next T]");
    println!("                          Change some fields of a work record; give at least one");
    println!("  harold tool work decide NAME --mark M --who W TEXT   Add a decision, dated today");
    println!("  harold tool work mark NAME N MARK   Change the mark of decision N");
    println!("  harold tool work link|unlink NAME LINK   Add or remove a link");
    println!(
        "  harold tool work check              Print each mismatch and exit 1; silent when clean"
    );
    println!("  harold tool work import FOLDER [--queue ID,ID] [--apply]");
    println!("                          Load the chief's *.md files, dry run unless --apply");
    println!("  harold tool work rotate --dry-run   Show what a rotation would copy now");
    println!("  harold tool work history NAME       Print a work record from the newest file");
    println!("  harold tool work search WORD        List the work records that hold WORD");
    println!("  harold --diagnostics [--delay [N]]  Test screen lock, TTS, and iMessage config");
    println!("                                      --delay defaults to 10s if no value given");
    println!("  harold --build-features Print the Cargo features of this build, one per line");
    println!("  harold --help           Show this help\n");
    println!("ENVIRONMENT:");
    println!("  HAROLD_CONFIG_DIR       Config directory (default: config/ next to the binary)");
    println!("  HAROLD_ENV              Config environment overlay (default: local)");
    println!("  HAROLD__*               Override any config key via env var");
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args = cli::with_caller_pane(args);
    let mode = match cli::Mode::parse(&args) {
        Ok(mode) => mode,
        Err(line) if args.first().map(String::as_str) == Some(cli::GET_COMMAND) => {
            eprintln!("{line}");
            std::process::exit(cli::UNKNOWN_THING_EXIT)
        }
        Err(line)
            if matches!(
                args.first().map(String::as_str),
                Some(
                    cli::AWAY_MESSAGE_COMMAND
                        | cli::SPEAK_COMMAND
                        | cli::TOOL_COMMAND
                        | cli::APP_COMMAND
                        | cli::START_CONTROL_TOWER_COMMAND
                )
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
    if mode == cli::Mode::BuildFeatures {
        print!("{}", build_features::listing(build_features::FEATURES));
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

/// Sets up the call answerer, the speech models and the desk's calls, and returns the phone's
/// calls for the web app, which exist only with `[web_app]`.
#[cfg(feature = "calls")]
fn start_calls(
    cfg: &settings::Settings,
    store: &Arc<store::HaroldStore>,
    store_path: &str,
    snapshots: &agent::snapshot::AgentSnapshotHub,
) -> Option<Arc<dyn web_app::call::PhoneCallApi>> {
    // The call answerer, before any call can start.
    if let Some(answering) =
        voice_input::daemon_call_answering(cfg, Arc::clone(store), snapshots.clone())
    {
        voice_input::init_call_answering(answering);
    }
    // The speech models: a missing one is downloaded in the background, pinned and checked
    // (ADR 0011), and a call waits for them.
    if let Some(voice_input) = cfg.voice_input.as_ref() {
        let folder = std::path::Path::new(store_path)
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .join("speech-models");
        voice_input::init_speech_models(voice_input::SpeechModels::new(
            voice_input,
            folder,
            voice_input::ModelsEnv::from_process(),
            Arc::new(voice_input::SystemHost),
            tts::server(),
        ));
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
            voice_input::PhoneCalls::new(ports, set_up).keeping_worker_for(Duration::from_secs(
                60 * cfg
                    .voice_input
                    .as_ref()
                    .map_or(10, |v| v.keep_worker_minutes),
            )),
            Arc::clone(store),
            end_desk,
        ))
    });
    if let Some(calls) = &phone_calls {
        voice_input::init_phone_calls(Arc::clone(calls) as Arc<dyn voice_input::LineTaker>);
    }
    phone_calls.map(|calls| calls as Arc<dyn web_app::call::PhoneCallApi>)
}

/// A build without `calls` has no call answerer, speech models or calls.
#[cfg(not(feature = "calls"))]
fn start_calls(
    _cfg: &settings::Settings,
    _store: &Arc<store::HaroldStore>,
    _store_path: &str,
    _snapshots: &agent::snapshot::AgentSnapshotHub,
) -> Option<Arc<dyn web_app::call::PhoneCallApi>> {
    None
}

async fn async_main(mode: cli::Mode) -> Result<(), Box<dyn std::error::Error>> {
    let is_probe = matches!(
        mode,
        cli::Mode::CheckConfig
            | cli::Mode::CheckReady
            | cli::Mode::Accept(_)
            | cli::Mode::Get(_)
            | cli::Mode::SendMessageToAwayDevice(_)
            | cli::Mode::Speak(_)
            | cli::Mode::Tool(_)
            | cli::Mode::App(_)
            | cli::Mode::StartControlTower(_)
            | cli::Mode::Slaves(_)
            | cli::Mode::Master(_)
    );
    let launchd = started_by_launchd(&mode);
    // Telemetry starts before the config loads, so a refused start reaches the collector.
    // Only the modes that run on past the config start it: the other modes start none.
    let telemetry = if matches!(mode, cli::Mode::Daemon | cli::Mode::Diagnostics { .. }) {
        let (early, machine, level) = settings::early_telemetry();
        let guard = init_telemetry(&level, &early, &machine);
        if let Some(flush) = guard.flusher() {
            telemetry::install_panic_hook(flush);
        }
        Some(guard)
    } else {
        None
    };
    // Before `load`, which would deserialise (or reject) the settings this names.
    let removed = settings::removed_setting_messages();
    if !removed.is_empty() {
        if is_probe {
            for sentence in &removed {
                eprintln!("{sentence}");
            }
            return Err("invalid configuration".into());
        }
        let message = format!("invalid configuration: {}", removed.join("; "));
        let errors = refused_errors_count(&removed);
        if launchd {
            refuse_invalid_config(&message, errors, telemetry).await;
        }
        return Err(refuse_by_hand(message, errors, telemetry).await);
    }
    let settings = match settings::Settings::load() {
        Ok(settings) => settings,
        Err(_) if is_probe => return Err("invalid configuration".into()),
        Err(error) if launchd => {
            refuse_invalid_config(&error.to_string(), refused_errors_count(&[]), telemetry).await
        }
        Err(error) => {
            let errors = refused_errors_count(&[]);
            return Err(refuse_by_hand(error.to_string(), errors, telemetry).await);
        }
    };

    let errors = settings.validate();
    if !errors.is_empty() {
        if is_probe {
            return Err("invalid configuration".into());
        }
        let message = format!("invalid configuration: {}", errors.join("; "));
        let count = refused_errors_count(&errors);
        if launchd {
            refuse_invalid_config(&message, count, telemetry).await;
        }
        return Err(refuse_by_hand(message, count, telemetry).await);
    }

    match mode {
        cli::Mode::CheckConfig => return cli::check_config(&settings),
        cli::Mode::CheckReady => return cli::check_ready(&settings).await,
        cli::Mode::Accept(ref args) => return cli::accept(&settings, args).await,
        cli::Mode::Get(thing) => match cli::get(&settings, thing).await {
            Ok(json) => {
                println!("{json}");
                return Ok(());
            }
            Err(failure) => {
                eprintln!("{}", failure.line);
                std::process::exit(failure.exit_code)
            }
        },
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
        cli::Mode::StartControlTower(ref args) => {
            // `exec` replaces this process, so only a failure comes back.
            let line = match cli::start_control_tower(&settings, args) {
                Ok(never) => match never {},
                Err(line) => line,
            };
            exit_with_line(&line);
        }
        cli::Mode::Slaves(ref command) => {
            let data = std::path::PathBuf::from(settings.store.resolved_path());
            let data = data
                .parent()
                .unwrap_or(std::path::Path::new(""))
                .to_path_buf();
            let names = settings
                .slaves
                .as_ref()
                .map(|slaves| slaves.names.clone())
                .unwrap_or_default();
            match slave::pairing::run_slaves(&data, &names, command) {
                Ok(text) => println!("{text}"),
                Err(line) => exit_with_line(&line),
            }
            return Ok(());
        }
        cli::Mode::Master(slave::pairing::MasterCommand::Pair {
            ref secret,
            ref address,
        }) => {
            let data = std::path::PathBuf::from(settings.store.resolved_path());
            let data = data
                .parent()
                .unwrap_or(std::path::Path::new(""))
                .to_path_buf();
            match slave::pairing::run_master(&data, &settings::local_toml_path(), secret, address) {
                Ok(text) => println!("{text}"),
                Err(line) => exit_with_line(&line),
            }
            return Ok(());
        }
        cli::Mode::App(ref command) => {
            let address = settings.grpc.addr()?.to_string();
            // Unlocking is the command's own act on the data folder, never a call to the daemon.
            if matches!(command, cli::AppCommand::Pair { unlock: true })
                && let Err(line) =
                    web_app::pair::unlock(std::path::Path::new(&settings.store.resolved_path()))
            {
                exit_with_line(&line);
            }
            match cli::app(&address, command, telemetry::local_offset()).await {
                Ok(text) => println!("{text}"),
                Err(line) => exit_with_line(&line),
            }
            return Ok(());
        }
        cli::Mode::Tool(cli::ToolCommand::Work(ref command)) => {
            let address = settings.grpc.addr()?.to_string();
            match cli::work(&address, command, std::io::stdin()).await {
                Ok(output) => match cli::write_work_output(&output, &mut std::io::stdout()) {
                    0 => {}
                    code => std::process::exit(code),
                },
                Err(line) => exit_with_line(&line),
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

    // Only the daemon and the diagnostics mode reach this line, and both started telemetry.
    let Some(telemetry) = telemetry else {
        return Err("telemetry did not start".into());
    };
    if telemetry.exporting() {
        crate::telemetry::register_phone_counters();
        crate::telemetry::register_turn_token_counters();
        crate::telemetry::register_work_counters();
        crate::telemetry::register_attention_rate(
            settings.attention.hourly_rate,
            &settings.attention.currency,
        );
        crate::telemetry::register_prices(&settings.prices);
        crate::telemetry::register_recap_counters(&agent::recap::recap_provider_ids(
            &settings.agents.0,
        ));
        crate::telemetry::register_blocked_long_counters(
            &settings
                .agents
                .0
                .iter()
                .map(|provider| provider.id.as_str())
                .collect::<Vec<_>>(),
        );
        if let Some(slaves) = &settings.slaves {
            crate::telemetry::register_slave_link(&slaves.names);
        }
        crate::telemetry::register_multiplexer_counters(&settings.agent_monitor.multiplexer);
        crate::telemetry::register_host_counters(
            &settings.agent_monitor.multiplexer,
            &settings.agent_monitor.rex_hosts,
        );
    }
    if settings.notify.old_key_applied {
        tracing::warn!(
            "notify.away_messages_only_from is replaced by notify.chief and the channel lists"
        );
    }
    if settings::away_channel_still_set() {
        tracing::warn!(
            "setting no longer read: notify.away_channel; remove it from config/local.toml"
        );
    }
    settings::warn_unread_settings();
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
            Ok(crash_guard::Start::Run(started)) => {
                log_unclean_start(&started);
                guard = Some(started);
            }
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
    let log_trim = start_log_trim(launchd, &shutdown_rx);

    agent::rex::log_multiplexer(cfg);
    let window_names = agent::inventory::WindowNames::default();
    let inventory = agent::rex::inventory_for(cfg, window_names.clone());
    let screen = agent::rex::screen_for(cfg);
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
    // Each Rex host has its own task, so the Mac's scan never waits for a host.
    let host_agents = agent::rex_host_runner::start(cfg, shutdown_rx.clone());
    // Stops the `rex events` children on every exit path, an early `?` return too.
    let status_guard = StatusStreamsGuard {
        rex: cfg.agent_monitor.multiplexer() == settings::Multiplexer::Rex,
    };

    telemetry::record_install(
        HAROLD_VERSION,
        HAROLD_RELEASE_TIME,
        time::OffsetDateTime::now_utc(),
        std::path::Path::new(&cfg.store.resolved_path()),
    );
    let telemetry_tasks = start_telemetry_tasks(cfg, &snapshots, &shutdown_rx).await;
    if let Some(queue) = &telemetry_tasks.alert_queue {
        store.set_alert_queue(queue.clone());
    }
    // The TTS server comes before the calls: a call waits for its default model.
    let tts_server = tts::start(cfg, Arc::clone(&store), &store_path, shutdown_rx.clone()).await;
    tts::init_server(Arc::clone(&tts_server));
    let phone_calls = start_calls(cfg, &store, &store_path, &snapshots);
    let browser_collector = telemetry
        .exporting()
        .then(|| cfg.telemetry.otlp_http_endpoint.clone())
        .flatten();
    let pod_registry = cfg.telemetry.pod_registry_path();
    let web_app = web_app::start_if_set(
        cfg.web_app.as_ref(),
        Arc::clone(&store),
        std::path::Path::new(&store_path),
        shutdown_rx.clone(),
        phone_calls,
        web_app::Agents {
            snapshots: &snapshots,
            chief_folder: cfg.notify.chief.as_deref(),
            chief_session: cfg.notify.chief_session.as_deref(),
            grpc: &cfg.grpc,
            browser_collector: browser_collector.as_deref(),
            knowledge: cfg.knowledge.enabled(),
            pod_registry: pod_registry.as_deref(),
            factory_flow: &cfg.factory_flow,
        },
    )
    .await;
    projector::remove_old_notes_at_start();
    let event_handler_handle = tokio::spawn(projector::run_event_handler_with_app(
        Arc::clone(&store),
        snapshots.clone(),
        telemetry_tasks.alert_queue.clone(),
        // A paired slave has no phone route: only the master tells the user.
        web_app
            .as_ref()
            .filter(|_| slave::delivers_locally(cfg))
            .map(|running| Arc::clone(&running.route)),
        slave::delivers_locally(cfg),
        shutdown_rx.clone(),
    ));
    #[cfg(feature = "imessage")]
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
    // One channel: the watch tool sends the counts, the snapshot stream reads them.
    let (watch_counts_tx, watch_counts_rx) = watch::channel(tools::WatchCounts::default());
    let tools = tools::start(
        cfg,
        &store_path,
        snapshots.clone(),
        web_app.as_ref().and_then(|running| running.specs_addr),
        Arc::new(watch_counts_tx),
        shutdown_rx.clone(),
    )
    .await;

    info!(address = %addr, "Harold listening");
    let restart_reminder = cfg.restart_reminder.enabled.then(|| {
        let ports = Arc::new(
            timed_prompts::TmuxPorts::new(snapshots.clone(), cfg.agents.0.clone())
                .with_chief(&cfg.notify),
        );
        let data_folder = std::path::Path::new(&store_path)
            .parent()
            .unwrap_or(std::path::Path::new("."));
        Arc::new(restart_reminder::RestartReminder::new(
            ports,
            cfg.restart_reminder.clone(),
            data_folder,
        )) as Arc<dyn restart_reminder::TurnEnds>
    });
    // At a turn end of the chief, the evidence tool types the lines that wait. It hands
    // each turn end on to the restart reminder.
    let restart_reminder = Some(Arc::new(tools::evidence::EvidenceTurnEnds::new(
        Arc::new(
            timed_prompts::TmuxPorts::new(snapshots.clone(), cfg.agents.0.clone())
                .with_chief(&cfg.notify),
        ),
        cfg.telemetry.pod_registry_path(),
        restart_reminder,
    )) as Arc<dyn restart_reminder::TurnEnds>);
    // Only Rex has status records. tmux never creates the shared status.
    let current_steps = if cfg.agent_monitor.multiplexer() == settings::Multiplexer::Rex {
        agent::rex::status().steps()
    } else {
        agent::rex::CurrentSteps::default()
    };
    let recaps = agent::recap::Recaps::default();
    agent::recap::spawn_recap_reader(
        recaps.clone(),
        snapshots.subscribe(),
        cfg.agents.0.clone(),
        Duration::from_secs(10),
        shutdown_rx.clone(),
    );
    // One channel: the waiting task sends the waiting agents, the snapshot stream reads them.
    let (waiting_tx, waiting_rx) = watch::channel(agent::waiting::WaitingView::default());
    // One channel: each `AgentSeen` call sends its pane id, the waiting task reads it.
    let (seen_tx, seen_rx) = mpsc::channel(64);
    spawn_waiting_task(
        snapshots.subscribe(),
        host_agents.clone(),
        Arc::new(agent::focus::CommandFocusPort::new(
            cfg.agent_monitor.rex.clone(),
        )),
        RoleSources::from_settings(cfg),
        Duration::from_secs(1),
        wall_clock_ms,
        waiting_tx,
        seen_rx,
        shutdown_rx.clone(),
    );
    let service = harold_service(
        monitor,
        Arc::clone(&store),
        snapshots,
        shutdown_rx.clone(),
        restart_reminder,
        current_steps,
        recaps,
        host_agents,
        window_names,
        cfg.agent_monitor
            .mark_for_machine(&cfg.machine_name)
            .unwrap_or_default()
            .to_owned(),
        HaroldStateView::from_settings(cfg),
        watch_counts_rx,
        waiting_rx,
        seen_tx,
    );
    let (slave_hosts, slave_turns) = (service.host_agents.clone(), service.turn_path());
    // The slave link listener runs only when `[slaves]` exists. It serves `SlaveLink` only.
    let slave_link = cfg.slaves.as_ref().map(|slaves| {
        let data = std::path::Path::new(&store_path)
            .parent()
            .unwrap_or(std::path::Path::new("."));
        // The registry keeps what the slaves report. It takes each turn through the same code
        // as a hook's turn end, and makes the Rex inventory skip a linked host.
        let registry = Arc::new(slave::registry::SlaveRegistry::new(
            slave_hosts,
            Arc::new(slave_turns),
        ));
        slave::registry::install(Arc::clone(&registry));
        slave::master::serve_slave_link(slaves, data, registry)
    });
    // The slave link starts only when `[master]` exists. The agent snapshot feed and the lock
    // feed publish into `source`.
    if let Some(master) = cfg.master.as_ref() {
        let data = std::path::Path::new(&store_path)
            .parent()
            .unwrap_or(std::path::Path::new("."));
        let source = slave::slave::LocalSource::new();
        // The feeds use the same builder as `WatchAgentStates`, and start before the link so
        // the first snapshot and the first lock are ready when it connects.
        let (feed_sender, feed_receiver) = mpsc::channel(1);
        service.spawn_snapshot_forwarder(feed_sender);
        let sink: Arc<dyn agent::slave_feed::FeedSink> = source.clone();
        tokio::spawn(agent::slave_feed::feed_snapshots(
            feed_receiver,
            sink.clone(),
        ));
        tokio::spawn(agent::slave_feed::feed_lock(
            sink,
            agent::slave_feed::LOCK_POLL,
            slave_screen_locked,
        ));
        if let Err(error) = slave::slave::install(master, &cfg.machine_name, data, source).await {
            warn!(error = %error, "slave link not started");
        }
    }
    let server_result = Server::builder()
        .add_routes(tools.routes.clone())
        .add_service(HaroldServer::new(service))
        .add_service(
            harold_api::tts::v1::tts_service_server::TtsServiceServer::new(
                tts::service::TtsApi::new(Arc::clone(&tts_server)),
            ),
        )
        .add_service(harold::harold_app_server::HaroldAppServer::new(
            web_app::pair::AppService {
                pairing: web_app.as_ref().map(|running| Arc::clone(&running.pairing)),
            },
        ))
        .serve_with_shutdown(addr, async {
            shutdown_signal().await;
            info!("shutting down");
            // Drop the sender to signal all receivers.
            drop(shutdown_tx);
        })
        .await;

    if let Err(error) = &server_result {
        log_grpc_listener_failure(error, addr.port());
    }

    if let Some(slave_link) = slave_link {
        slave_link.abort();
    }
    slave::slave::stop();

    // An open call ends with reason `shutdown`, and its folder and child programs go.
    if let Some(calls) = voice_input::daemon_calls() {
        calls.shutdown().await;
    }
    tts_server.supervisor().stop().await;

    // Wait for the handler, the listener, the timed-prompt task, the tools, the web app, the
    // pod facts and the alert address to stop before returning.
    let _ = event_handler_handle.await;
    #[cfg(feature = "imessage")]
    let _ = listener_handle.await;
    let _ = timed_prompts_handle.await;
    if let Some(handle) = log_trim {
        let _ = handle.await;
    }
    tools.stopped().await;
    if let Some(running) = web_app {
        let _ = running.handle.await;
    }
    telemetry_tasks.stopped().await;
    stop_agent_monitor(monitor_task, activity_provider, Duration::from_secs(1)).await;
    drop(status_guard);
    if server_result.is_ok()
        && let Some(guard) = guard
    {
        mark_clean(guard);
    }
    telemetry.shutdown().await;
    server_result?;
    Ok(())
}

/// Logs `listener not started` when the gRPC server ended on an I/O error, such as a port in
/// use. The line goes out before the telemetry flush.
fn log_grpc_listener_failure(error: &(dyn std::error::Error + 'static), port: u16) {
    let mut source = Some(error);
    while let Some(current) = source {
        if let Some(io_error) = current.downcast_ref::<std::io::Error>() {
            telemetry::log_listener_failed("grpc", port, io_error);
            return;
        }
        source = current.source();
    }
}

/// Whether launchd started this daemon: its plist sets `HAROLD_LAUNCHD=1`. Then a start that
/// must not be retried exits 0, so `KeepAlive` does not restart it; any other way, it exits 1.
fn started_by_launchd(mode: &cli::Mode) -> bool {
    *mode == cli::Mode::Daemon
        && std::env::var_os("HAROLD_LAUNCHD").is_some_and(|value| value == "1")
}

/// Starts the hourly trim of `harold.log` to its last 24 hours, only when launchd started
/// Harold: run any other way, standard output is not that file.
fn start_log_trim(
    launchd: bool,
    shutdown: &watch::Receiver<()>,
) -> Option<tokio::task::JoinHandle<()>> {
    launchd.then(|| log_trim::start(shutdown.clone()))
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

/// The number of problems a refused configuration reports: one for a load error, which
/// has no list.
fn refused_errors_count(problems: &[String]) -> usize {
    problems.len().max(1)
}

/// Refuses launchd's start of a configuration that does not validate. The refusal goes
/// through the normal subscriber, and telemetry is shut down first so the line is exported
/// when the exporter is on. The problem text goes to standard error, which the plist sends
/// to the log.
async fn refuse_invalid_config(
    message: &str,
    errors: usize,
    telemetry: Option<telemetry::TelemetryGuard>,
) -> ! {
    eprintln!("Error: {message}");
    log_refusal_and_flush(errors, telemetry).await;
    std::process::exit(0)
}

/// Logs `startup refused`, then stops telemetry so that the line is exported when this
/// function returns. A test calls it, because `refuse_invalid_config` ends the process.
async fn log_refusal_and_flush(errors: usize, telemetry: Option<telemetry::TelemetryGuard>) {
    tracing::warn!(reason = "invalid_config", errors, "startup refused");
    if let Some(telemetry) = telemetry {
        telemetry.shutdown().await;
    }
}

/// Logs `start after an unclean exit` with the count when the last run did not exit
/// cleanly. A test calls it, because the check sits in the start path of `main`.
fn log_unclean_start(started: &crash_guard::CrashGuard) {
    if started.unclean_starts() > 0 {
        tracing::warn!(
            count = started.unclean_starts(),
            "start after an unclean exit"
        );
    }
}

/// Refuses a start by hand, outside launchd: logs `startup refused`, stops telemetry so the
/// line is exported, and only then gives the error that `main` returns. Without telemetry,
/// it only logs the line.
async fn refuse_by_hand(
    message: String,
    errors: usize,
    telemetry: Option<telemetry::TelemetryGuard>,
) -> Box<dyn std::error::Error> {
    log_refusal_and_flush(errors, telemetry).await;
    message.into()
}

fn mark_clean(guard: crash_guard::CrashGuard) {
    if let Err(error) = guard.mark_clean() {
        tracing::warn!(error = %error, "crash guard could not record a clean exit");
    }
}

/// The machine stats, the pod facts and the alert address, each started only when its
/// `[telemetry]` key is set. With none set, nothing starts: no machine is read, no `git`
/// command runs and no address is opened.
struct TelemetryTasks {
    machine_stats: Option<tokio::task::JoinHandle<()>>,
    pod_stats: Option<tokio::task::JoinHandle<()>>,
    alerts: Option<tokio::task::JoinHandle<()>>,
    alert_queue: Option<alerts::AlertQueue>,
}

impl TelemetryTasks {
    /// Waits for the pod facts and the alert address to stop, after shutdown.
    async fn stopped(self) {
        if let Some(handle) = self.machine_stats {
            let _ = handle.await;
        }
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
    // The machine stats are read only when the telemetry is exported: nothing else reads them.
    let machine_stats =
        (cfg.telemetry.export && cfg.telemetry.otlp_endpoint.is_some()).then(|| {
            machine_stats::start(
                machine_stats::hourly_use_file(std::path::Path::new(&cfg.store.resolved_path())),
                shutdown_rx.clone(),
            )
        });
    // The pod facts are read only when the chief's registry is named.
    let pod_stats = cfg.telemetry.pod_registry_path().map(|registry| {
        pod_stats::start(
            registry,
            pod_stats::tips_file(std::path::Path::new(&cfg.store.resolved_path())),
            snapshots.clone(),
            cfg.notify.chief.clone(),
            shutdown_rx.clone(),
        )
    });
    // The alert address is opened only when `alert_listen` is set.
    let (alerts, alert_queue) = match cfg.telemetry.alert_addr() {
        Some(alert_addr) => alerts::start(
            alert_addr,
            cfg.notify.chief.clone(),
            timed_prompts::TmuxPorts::new(snapshots.clone(), cfg.agents.0.clone())
                .with_chief(&cfg.notify),
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
        machine_stats,
        pod_stats,
        alerts,
        alert_queue,
    }
}

/// Stops the Rex status streams when dropped. It never creates them.
struct StatusStreamsGuard {
    rex: bool,
}

impl Drop for StatusStreamsGuard {
    fn drop(&mut self) {
        if self.rex {
            agent::rex::stop_started_status();
        }
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
mod test_logs;

#[cfg(test)]
mod test_support;

#[cfg(test)]
#[path = "main_tests.rs"]
mod tests;
