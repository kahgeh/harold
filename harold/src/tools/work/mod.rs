//! The chief's work records, kept in Turso files of their own, Harold's work
//! tool. Outside this module only `NAME` and `start` are used.

use std::sync::Arc;

use harold_api::tools::work::v1::work_service_server::WorkServiceServer;
use tonic::service::Routes;

use crate::tools::backlog::guard::Guard;
use crate::tools::{ToolContext, ToolError, ToolHandle};

mod check;
mod files;
mod import;
mod metrics;
mod rotation;
mod schema;
mod service;
mod store;
#[cfg(test)]
mod test_support;
mod timer;

pub(crate) const NAME: &str = "work";

/// The reason `start` gives when the current work file cannot be opened or
/// made. The error's own text names the data folder's path, so it is not
/// passed on.
const STORE_OPEN_FAILED: &str = "store_open_failed";

/// Opens the current work file in the tool's data folder, or makes the file of
/// today when there is none. Then it serves the work records, and runs the
/// daily check at start and once a day, until shutdown. The tool types nothing,
/// so `ctx.typing` is never used. The service trusts local callers (ADR 0002).
pub(crate) async fn start(ctx: ToolContext) -> Result<ToolHandle, ToolError> {
    let files = files::WorkFiles::new(ctx.data_folder);
    let clock: Arc<dyn rotation::Clock> = Arc::new(rotation::SystemClock);
    let rotator = Arc::new(rotation::Rotator::new(files.clone(), Arc::clone(&clock)));
    // The guard is let go at once: no write or rotation is held back.
    drop(rotator.current().await.map_err(|_| ToolError {
        reason: STORE_OPEN_FAILED,
    })?);
    let guard = Guard::new(ctx.pane_folder, ctx.products, ctx.chief_pane);
    let questions = Arc::new(service::LoopbackQuestions::new(ctx.grpc_addr));
    let queue = Arc::new(import::LoopbackQueue::new(ctx.grpc_addr));
    let service = service::WorkToolService::new(
        Arc::clone(&rotator),
        files,
        guard,
        questions,
        queue,
        Arc::clone(&clock),
    );
    // Returns on a shutdown signal and when the sender is dropped.
    let task = tokio::spawn(timer::run(
        rotator,
        clock,
        Box::new(timer::read_rotation_days),
        ctx.shutdown,
    ));
    Ok(ToolHandle {
        routes: Routes::new(WorkServiceServer::new(service)),
        task,
    })
}
