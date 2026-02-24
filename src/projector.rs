use std::sync::Arc;

use events::{EventEnvelope, EventStore, Projector, Result};
use tracing::{info, warn};

use crate::notify::notify;
use crate::store::TurnCompleted;

pub async fn run_projector(store: Arc<EventStore>) {
    // Projector::new takes EventStore by value. Unwrap the Arc — this is the
    // only Arc clone created for the projector, so try_unwrap succeeds unless
    // the caller accidentally clones it again.
    let store = match Arc::try_unwrap(store) {
        Ok(s) => s,
        Err(_) => {
            warn!("projector: could not unwrap store Arc — extra clone detected");
            return;
        }
    };

    let projector = Projector::new(store, "harold.notifier".into());
    info!("projector starting");

    let result: Result<()> = projector
        .run(|events: &[EventEnvelope]| {
            // Clone needed data before the async block — no references may escape.
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
                    tokio::task::spawn_blocking(move || notify(&turn))
                        .await
                        .ok();
                }
                Ok(())
            }
        })
        .await;

    if let Err(e) = result {
        warn!(error = %e, "projector exited with error");
    }
}
