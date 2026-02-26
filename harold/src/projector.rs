use std::sync::Arc;

use events::{EventEnvelope, EventStore, Projector, Result};
use tokio::sync::watch;
use tracing::{info, warn};

use crate::notify::notify;
use crate::routing::route_reply;
use crate::store::{ReplyReceived, TurnCompleted};

pub async fn run_projector(store: Arc<EventStore>, mut shutdown: watch::Receiver<()>) {
    let projector = Projector::new(store, "harold.notifier".into());
    info!("projector starting");

    let result: Result<()> = tokio::select! {
        res = projector.run(|events: &[EventEnvelope]| {
            // Clone all needed data before the async block — no references may escape.
            let batch: Vec<(String, serde_json::Value)> = events
                .iter()
                .map(|e| (e.r#type.clone(), e.payload.clone()))
                .collect();

            async move {
                for (event_type, payload) in batch {
                    match event_type.as_str() {
                        "TurnCompleted" => {
                            match serde_json::from_value::<TurnCompleted>(payload) {
                                Ok(turn) => {
                                    info!(
                                        pane_label = %turn.pane_label,
                                        main_context = %turn.main_context,
                                        "projector: TurnCompleted"
                                    );
                                    tokio::task::spawn_blocking(move || notify(&turn)).await.ok();
                                }
                                Err(e) => warn!(error = %e, "projector: failed to deserialise TurnCompleted"),
                            }
                        }
                        "ReplyReceived" => {
                            match serde_json::from_value::<ReplyReceived>(payload) {
                                Ok(reply) => {
                                    info!("projector: ReplyReceived");
                                    tokio::task::spawn_blocking(move || route_reply(&reply.text))
                                        .await
                                        .ok();
                                }
                                Err(e) => warn!(error = %e, "projector: failed to deserialise ReplyReceived"),
                            }
                        }
                        other => {
                            warn!(event_type = %other, "projector: unknown event type");
                        }
                    }
                }
                Ok(())
            }
        }) => res,
        _ = shutdown.changed() => {
            info!("projector shutting down");
            Ok(())
        }
    };

    if let Err(e) = result {
        warn!(error = %e, "projector exited with error");
    }
}
