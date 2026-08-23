mod agent;
mod channels;
mod inbound;
mod outbound;
mod projector;
mod settings;
mod store;
mod telemetry;
mod tmux;
mod util;

use std::sync::Arc;
use std::time::Duration;

use settings::{get_settings, init_settings};
use telemetry::init_telemetry;
use tokio::sync::{mpsc, watch};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, transport::Server};
use tracing::{Instrument, info, info_span};

pub use harold_api::harold;

use harold::harold_server::{Harold, HaroldServer};
use harold::{
    AgentMonitorHealth, AgentPaneState, AgentState, AgentStateSnapshot, MonitorHealthState,
    ReportAgentStateRequest, ReportAgentStateResponse, TurnCompleteRequest, TurnCompleteResponse,
    WatchAgentStatesRequest,
};

struct HaroldService {
    monitor: agent::runtime::AgentMonitorHandle,
    snapshots: agent::snapshot::AgentSnapshotHub,
    shutdown: watch::Receiver<()>,
}

#[tonic::async_trait]
impl Harold for HaroldService {
    type WatchAgentStatesStream = ReceiverStream<Result<AgentStateSnapshot, Status>>;

    async fn turn_complete(
        &self,
        request: Request<TurnCompleteRequest>,
    ) -> Result<Response<TurnCompleteResponse>, Status> {
        let req = request.into_inner();
        let trace_id = uuid::Uuid::new_v4().to_string();
        let span = info_span!("grpc_turn_complete", trace_id = %trace_id);

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

            self.monitor
                .turn_completed(event)
                .await
                .map_err(|e| {
                    tracing::error!(pane_id = %pane_id_log, result = "append_failed", error = %e, "turn complete rejected");
                    Status::internal("event store write failed")
                })?;

            info!(pane_id = %pane_id_log, result = "accepted", "turn complete persisted");

            Ok(Response::new(TurnCompleteResponse { accepted: true }))
        }
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
        let summary_update = agent::summary::explicit_summary_update(req.work_summary.as_deref());

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
        through_event_version: snapshot.through_event_version.get() as u64,
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

fn print_help() {
    println!("harold — agent notification and inbound message routing daemon\n");
    println!("USAGE:");
    println!("  harold                  Start the Harold daemon");
    println!("  harold --diagnostics [--delay [N]]  Test screen lock, TTS, and iMessage config");
    println!("                                      --delay defaults to 10s if no value given");
    println!("  harold --help           Show this help\n");
    println!("ENVIRONMENT:");
    println!("  HAROLD_CONFIG_DIR       Path to config directory (default: ./config)");
    println!("  HAROLD_ENV              Config environment overlay (default: local)");
    println!("  HAROLD__*               Override any config key via env var");
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();

    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_help();
        return Ok(());
    }

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async_main(args))
}

async fn async_main(args: Vec<String>) -> Result<(), Box<dyn std::error::Error>> {
    let settings = settings::Settings::load()?;
    init_telemetry(&settings.log.level);

    let errors = settings.validate();
    if !errors.is_empty() {
        for e in &errors {
            tracing::error!("{e}");
        }
        return Err("invalid configuration".into());
    }

    init_settings(settings);
    let cfg = get_settings();

    if args
        .iter()
        .any(|a| a == "--diagnostic" || a == "--diagnostics")
    {
        let delay = if let Some(pos) = args.iter().position(|a| a == "--delay") {
            args.get(pos + 1)
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(10)
        } else {
            0
        };
        run_diagnostics(delay);
        return Ok(());
    }

    let store_path = cfg.store.resolved_path();
    let store = store::open_store(&store_path).await?;

    let addr = cfg.grpc.addr()?;

    // Shutdown channel: sender closes on signal, receivers see the channel close.
    let (shutdown_tx, shutdown_rx) = watch::channel(());

    let initial_agent_snapshot = load_startup_agent_snapshot(&store).await?;
    let snapshots = agent::snapshot::AgentSnapshotHub::new(initial_agent_snapshot.clone());

    let inventory: Arc<dyn agent::inventory::AgentInventoryPort> = Arc::new(
        agent::inventory::TmuxAgentInventory::new(cfg.agents.clone()),
    );
    let screen: Arc<dyn agent::screen::VisibleScreenPort> =
        Arc::new(agent::screen::TmuxVisibleScreen::new());
    let providers = match &cfg.agents {
        settings::AgentSettings::Named(providers) => providers.clone(),
        settings::AgentSettings::Legacy { .. } => Vec::new(),
    };
    let (monitor, mut monitor_task) = agent::runtime::spawn_agent_monitor(
        Arc::clone(&store),
        inventory,
        screen,
        providers,
        initial_agent_snapshot,
        agent::runtime::AgentMonitorRuntimeConfig {
            inventory_interval: Duration::from_millis(cfg.agent_monitor.inventory_interval_ms),
            screen_interval: Duration::from_millis(cfg.agent_monitor.screen_interval_ms),
            hook_grace_ms: cfg.agent_monitor.hook_grace_ms,
            acquisition_timeout: Duration::from_millis(500),
        },
        shutdown_rx.clone(),
    );

    let event_handler_handle = tokio::spawn(projector::run_event_handler(
        Arc::clone(&store),
        snapshots.clone(),
        shutdown_rx.clone(),
    ));
    let listener_handle = tokio::spawn(channels::listen_for_inbound_messages(
        Arc::clone(&store),
        shutdown_rx.clone(),
    ));

    info!(address = %addr, "Harold listening");
    Server::builder()
        .add_service(HaroldServer::new(HaroldService {
            monitor,
            snapshots,
            shutdown: shutdown_rx.clone(),
        }))
        .serve_with_shutdown(addr, async {
            shutdown_signal().await;
            info!("shutting down");
            // Drop the sender to signal all receivers.
            drop(shutdown_tx);
        })
        .await?;

    // Wait for the handler and listener to stop before returning.
    let _ = event_handler_handle.await;
    let _ = listener_handle.await;
    if tokio::time::timeout(Duration::from_secs(1), &mut monitor_task)
        .await
        .is_err()
    {
        tracing::warn!("agent monitor did not stop within shutdown deadline; aborting task");
        monitor_task.abort();
        let _ = monitor_task.await;
    }

    Ok(())
}

async fn load_startup_agent_snapshot(
    store: &store::HaroldStore,
) -> events::Result<agent::domain::AgentSnapshot> {
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
