//! The telemetry tool: agents declare and record counters, gauges and events
//! under a prefix. Outside this module only `NAME` and `start` are used.
//!
//! At start the tool seeds `harold.telemetry.calls` for every registry prefix
//! and `chief`, counts 0 on every series of each declared counter, and sends
//! each declared gauge's last value for each series again.

use std::sync::Arc;

use harold_api::tools::telemetry::v1::telemetry_service_server::TelemetryServiceServer;
use tonic::service::Routes;

use crate::tools::{ToolContext, ToolError, ToolHandle};

mod guard;
mod name;
mod schema;
mod service;
mod signals;
mod store;

#[cfg(test)]
mod test_support;

use guard::{CHIEF, Guard, SystemClock};
use name::Kind;
use schema::series_pairs;
use signals::{ExportedSignals, Signals};
use store::{StoreError, TelemetryStore, TursoTelemetryStore};

pub(crate) const NAME: &str = "telemetry";

/// The reason `start` gives when `telemetry.db` cannot be opened, migrated or
/// read. The error's own text names the data folder's path, so it is not
/// passed on.
const STORE_OPEN_FAILED: &str = "store_open_failed";

/// Opens `telemetry.db` in the tool's data folder, seeds the signals, then
/// serves the telemetry calls until shutdown. The tool types nothing, so
/// `ctx.typing` is never used.
pub(crate) async fn start(ctx: ToolContext) -> Result<ToolHandle, ToolError> {
    start_with(ctx, Arc::new(ExportedSignals)).await
}

async fn start_with(ctx: ToolContext, signals: Arc<dyn Signals>) -> Result<ToolHandle, ToolError> {
    let failed = |_| ToolError {
        reason: STORE_OPEN_FAILED,
    };
    let store = Arc::new(
        TursoTelemetryStore::open(&ctx.data_folder)
            .await
            .map_err(failed)?,
    );
    let guard = Guard::new(
        ctx.pane_folder,
        ctx.products,
        ctx.chief_pane,
        Arc::new(SystemClock),
    );
    let mut prefixes = guard.registry_prefixes();
    prefixes.push(CHIEF.to_string());
    signals.seed_calls(&prefixes);
    seed_declared(store.as_ref(), signals.as_ref())
        .await
        .map_err(failed)?;
    let service = service::TelemetryToolService::new(Arc::clone(&store), guard, signals);
    let mut shutdown = ctx.shutdown;
    let task = tokio::spawn(async move {
        // Returns on a shutdown signal and when the sender is dropped.
        let _ = shutdown.changed().await;
        drop(store);
    });
    Ok(ToolHandle {
        routes: Routes::new(TelemetryServiceServer::new(service)),
        task,
    })
}

/// Counts 0 on every series of each declared counter, and sends each declared
/// gauge's last value for each series again. A removed name is not in the
/// store, so it is not seeded or sent.
async fn seed_declared<S: TelemetryStore>(
    store: &S,
    signals: &dyn Signals,
) -> Result<(), StoreError> {
    for declaration in store.list(None).await? {
        match declaration.kind {
            Kind::Counter => service::seed_counter(signals, &declaration),
            Kind::Gauge => {
                for (key, value) in &declaration.last_values {
                    let Some(pairs) = series_pairs(key, &declaration.attrs) else {
                        continue;
                    };
                    signals.gauge_set(&declaration.name, *value, &pairs);
                }
            }
            Kind::Event => {}
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
