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

use settings::{get_settings, init_settings};
use telemetry::init_telemetry;
use tokio::sync::watch;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, transport::Server};
use tracing::{Instrument, info, info_span};

pub use harold_api::harold;

use harold::harold_server::{Harold, HaroldServer};
use harold::{
    AgentStateSnapshot, ReportAgentStateRequest, ReportAgentStateResponse, TurnCompleteRequest,
    TurnCompleteResponse, WatchAgentStatesRequest,
};

struct HaroldService {
    store: Arc<store::HaroldStore>,
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
            // assistant_message omitted from log — can be large
            info!(
                pane_id = %req.pane_id,
                pane_label = %req.pane_label,
                main_context = %req.main_context,
                "turn complete received"
            );

            let event = store::TurnCompleted {
                pane_id: req.pane_id,
                pane_label: req.pane_label,
                last_user_prompt: req.last_user_prompt,
                assistant_message: req.assistant_message,
                main_context: req.main_context,
            };

            store::append_turn_completed(&self.store, &event)
                .await
                .map_err(|e| {
                    tracing::error!(error = %e, "failed to append TurnCompleted event");
                    Status::internal("event store write failed")
                })?;

            Ok(Response::new(TurnCompleteResponse { accepted: true }))
        }
        .instrument(span)
        .await
    }

    async fn report_agent_state(
        &self,
        _request: Request<ReportAgentStateRequest>,
    ) -> Result<Response<ReportAgentStateResponse>, Status> {
        Err(Status::unimplemented(
            "agent state reporting is not implemented",
        ))
    }

    async fn watch_agent_states(
        &self,
        _request: Request<WatchAgentStatesRequest>,
    ) -> Result<Response<Self::WatchAgentStatesStream>, Status> {
        Err(Status::unimplemented(
            "agent state streaming is not implemented",
        ))
    }
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
    info!(address = %addr, "Harold listening");

    // Shutdown channel: sender closes on signal, receivers see the channel close.
    let (shutdown_tx, shutdown_rx) = watch::channel(());

    let event_handler_handle = tokio::spawn(projector::run_event_handler(
        Arc::clone(&store),
        shutdown_rx.clone(),
    ));
    let listener_handle = tokio::spawn(channels::listen_for_inbound_messages(
        Arc::clone(&store),
        shutdown_rx,
    ));

    Server::builder()
        .add_service(HaroldServer::new(HaroldService {
            store: Arc::clone(&store),
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

    Ok(())
}

#[cfg(test)]
#[path = "main_tests.rs"]
mod tests;
