mod listener;
mod notify;
mod projector;
mod route_reply;
mod settings;
mod store;
mod telemetry;
mod util;

use std::sync::Arc;

use settings::{get_settings, init_settings};
use telemetry::init_telemetry;
use tokio::sync::watch;
use tonic::{Request, Response, Status, transport::Server};
use tracing::info;

pub mod harold {
    tonic::include_proto!("harold");
}

use harold::harold_server::{Harold, HaroldServer};
use harold::{TurnCompleteRequest, TurnCompleteResponse};

struct HaroldService {
    store: Arc<events::EventStore>,
}

#[tonic::async_trait]
impl Harold for HaroldService {
    async fn turn_complete(
        &self,
        request: Request<TurnCompleteRequest>,
    ) -> Result<Response<TurnCompleteResponse>, Status> {
        let req = request.into_inner();
        // assistant_message omitted from log — can be large
        info!(
            pane_id = %req.pane_id,
            pane_label = %req.pane_label,
            main_context = %req.main_context,
            "turn complete received"
        );

        let pane = route_reply::PaneInfo {
            pane_id: req.pane_id,
            label: req.pane_label,
        };
        let event = store::TurnCompleted {
            pane_id: pane.pane_id.clone(),
            pane_label: pane.label.clone(),
            last_user_prompt: req.last_user_prompt,
            assistant_message: req.assistant_message,
            main_context: req.main_context,
        };

        // Commit to store first; update routing state only on success.
        store::append_turn_completed(&self.store, &event)
            .await
            .map_err(|e| {
                tracing::error!(error = %e, "failed to append TurnCompleted event");
                Status::internal("event store write failed")
            })?;

        route_reply::set_last_notified_pane(pane);

        Ok(Response::new(TurnCompleteResponse { accepted: true }))
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
    use notify::{is_screen_locked, notify_at_desk, notify_away};
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
    println!(
        "iMessage      : recipient={} handle_id={:?}",
        cfg.imessage.recipient.as_deref().unwrap_or("(not set)"),
        cfg.imessage.handle_id,
    );
    println!(
        "TTS           : command={} voice={:?}",
        cfg.tts.command, cfg.tts.voice,
    );
    println!(
        "AI cli        : {:?}",
        cfg.ai.cli_path.as_deref().unwrap_or("(not set)"),
    );

    println!("\n--- Testing notify path (screen_locked={locked}) ---");
    if !locked {
        println!("Running TTS...");
        notify_at_desk(&turn);
        println!("TTS done");
        return;
    }
    if cfg.imessage.recipient.is_none() {
        println!("iMessage NOT sent: recipient not configured");
        return;
    }
    println!("Sending iMessage...");
    notify_away(&turn);
    println!("iMessage sent (check your phone)");

    println!("\nDone.");
}

fn print_help() {
    println!("harold — agent notification and reply routing daemon\n");
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

    let projector_handle = tokio::spawn(projector::run_projector(
        Arc::clone(&store),
        shutdown_rx.clone(),
    ));
    let listener_handle = tokio::spawn(listener::listen(Arc::clone(&store), shutdown_rx));

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

    // Wait for tasks to stop before checkpointing — checkpoint requires no active connections.
    let _ = projector_handle.await;
    let _ = listener_handle.await;

    // Checkpoint WAL: flushes all WAL pages to the main db files so next open is clean.
    info!("checkpointing WAL");
    if let Err(e) = store.checkpoint().await {
        tracing::warn!(error = %e, "WAL checkpoint failed on shutdown");
        return Ok(());
    }
    info!("WAL checkpoint complete");

    Ok(())
}
