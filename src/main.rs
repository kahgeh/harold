mod notify;
mod projector;
mod routing;
mod settings;
mod store;
mod telemetry;

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

        let event = store::TurnCompleted {
            pane_id: req.pane_id.clone(),
            pane_label: req.pane_label.clone(),
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

        routing::set_last_notified_pane(routing::PaneInfo {
            pane_id: req.pane_id,
            label: req.pane_label,
        });

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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let settings = settings::Settings::load()?;
    init_settings(settings);

    let cfg = get_settings();
    init_telemetry(&cfg.log.level);

    let store_path = cfg.store.resolved_path();
    let store = store::open_store(&store_path).await?;

    let addr = cfg.grpc.addr()?;
    info!(address = %addr, "Harold listening");

    // Shutdown channel: sender closes on signal, receivers see the channel close.
    let (shutdown_tx, shutdown_rx) = watch::channel(());

    let projector_store = Arc::clone(&store);
    tokio::spawn(projector::run_projector(
        projector_store,
        shutdown_rx.clone(),
    ));
    tokio::spawn(routing::run_reply_router(shutdown_rx));

    Server::builder()
        .add_service(HaroldServer::new(HaroldService { store }))
        .serve_with_shutdown(addr, async {
            shutdown_signal().await;
            info!("shutting down");
            // Drop the sender to signal all receivers.
            drop(shutdown_tx);
        })
        .await?;

    Ok(())
}
