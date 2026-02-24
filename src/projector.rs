use std::sync::Arc;

use events::{EventEnvelope, EventStore, Projector, Result};
use tokio::sync::watch;
use tracing::{info, warn};

use crate::notify::notify;
use crate::store::TurnCompleted;

pub async fn run_projector(store: Arc<EventStore>, mut shutdown: watch::Receiver<()>) {
    let store = match Arc::try_unwrap(store) {
        Ok(s) => s,
        Err(_) => {
            warn!("projector: could not unwrap store Arc — extra clone detected");
            return;
        }
    };

    let projector = Projector::new(store, "harold.notifier".into());
    info!("projector starting");

    let result: Result<()> = tokio::select! {
        res = projector.run(|events: &[EventEnvelope]| {
            let turns: Vec<TurnCompleted> = events
                .iter()
                .filter(|e| e.r#type == "TurnCompleted")
                .filter_map(|e| serde_json::from_value(e.payload.clone()).ok())
                .collect();

            async move {
                for turn in turns {
                    info!(
                        pane_label = %turn.pane_label,
                        main_context = %turn.main_context,
                        "projector: processing TurnCompleted"
                    );
                    tokio::task::spawn_blocking(move || notify(&turn)).await.ok();
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
