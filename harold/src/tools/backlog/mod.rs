//! Each product's backlog and the chief's queue, Harold's second tool. Outside
//! this module only `NAME` and `start` are used.

use std::sync::Arc;

use harold_api::tools::backlog::v1::backlog_service_server::BacklogServiceServer;
use tonic::service::Routes;

use crate::tools::{ToolContext, ToolError, ToolHandle};

mod guard;
mod markdown;
mod schema;
mod service;
mod store;
#[cfg(test)]
mod test_support;

pub(crate) const NAME: &str = "backlog";

/// The reason `start` gives when `backlog.db` cannot be opened or migrated. The
/// error's own text names the data folder's path, so it is not passed on.
const STORE_OPEN_FAILED: &str = "store_open_failed";

/// Opens `backlog.db` in the tool's data folder, then serves the backlogs and
/// the queue until shutdown. The tool types nothing, so `ctx.typing` is never
/// used.
pub(crate) async fn start(ctx: ToolContext) -> Result<ToolHandle, ToolError> {
    let store = store::TursoBacklogStore::open(&ctx.data_folder)
        .await
        .map_err(|_| ToolError {
            reason: STORE_OPEN_FAILED,
        })?;
    let store = Arc::new(store);
    let guard = guard::Guard::new(ctx.pane_folder, ctx.products, ctx.chief_pane);
    let service = service::BacklogToolService::new(Arc::clone(&store), guard);
    let mut shutdown = ctx.shutdown;
    let task = tokio::spawn(async move {
        // Returns on a shutdown signal and when the sender is dropped.
        let _ = shutdown.changed().await;
        drop(store);
    });
    Ok(ToolHandle {
        routes: Routes::new(BacklogServiceServer::new(service)),
        task,
    })
}

fn now_ms() -> i64 {
    (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
}
