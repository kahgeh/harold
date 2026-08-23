use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tracing::{info, info_span, warn};

use crate::agent::snapshot::AgentSnapshotHub;
use crate::inbound::route_inbound_message;
use crate::outbound::{DeliveryOutcome, notify};
use crate::store::{HaroldStore, InboundMessage, PendingDelivery, ProjectionBatch, TurnCompleted};

pub(crate) trait DeliveryDispatcher: Send + Sync + 'static {
    fn dispatch(&self, delivery: &PendingDelivery) -> Result<DeliveryOutcome, DispatchError>;
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum DispatchError {
    #[error("{0}")]
    Retryable(String),

    #[error("{0}")]
    Permanent(String),
}

struct ProductionDispatcher;

impl DeliveryDispatcher for ProductionDispatcher {
    fn dispatch(&self, delivery: &PendingDelivery) -> Result<DeliveryOutcome, DispatchError> {
        match delivery.event_type.as_str() {
            "TurnCompleted" => {
                let turn = serde_json::from_value::<TurnCompleted>(delivery.payload.clone())
                    .map_err(|error| {
                        DispatchError::Permanent(format!("invalid TurnCompleted payload: {error}"))
                    })?;
                notify(&turn, &delivery.trace_id).map_err(DispatchError::Retryable)
            }
            "InboundMessageReceived" => {
                let message = serde_json::from_value::<InboundMessage>(delivery.payload.clone())
                    .map_err(|error| {
                        DispatchError::Permanent(format!(
                            "invalid InboundMessageReceived payload: {error}"
                        ))
                    })?;
                route_inbound_message(&message.text).map_err(DispatchError::Retryable)
            }
            other => Err(DispatchError::Permanent(format!(
                "unknown event type: {other}"
            ))),
        }
    }
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
    let span = info_span!(
        "event",
        trace_id = %trace_id,
        event_version = event_version.get(),
        event_type = %event_type,
    );
    let blocking_dispatcher = Arc::clone(&dispatcher);
    let result = tokio::task::spawn_blocking(move || {
        let _guard = span.enter();
        blocking_dispatcher.dispatch(&delivery)
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

pub(crate) async fn project_and_publish_agent_snapshot(
    store: &HaroldStore,
    snapshots: &AgentSnapshotHub,
    limit: usize,
) -> events::Result<ProjectionBatch> {
    let batch = store.project_unhandled_events(limit).await?;
    if batch.through_event_version.get() > snapshots.through_event_version().get() {
        snapshots.publish_committed(store.load_agent_snapshot().await?);
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
    let dispatcher: Arc<dyn DeliveryDispatcher> = Arc::new(ProductionDispatcher);
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
