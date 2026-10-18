//! Each product's backlog and the chief's queue, Harold's second tool. Outside
//! this module only `NAME` and `start` are used.

// Until the service serves the tool (Task 7).
#![allow(dead_code)]

use tonic::service::Routes;

use crate::tools::{ToolContext, ToolError, ToolHandle};

mod guard;
mod markdown;
mod schema;
mod store;
#[cfg(test)]
mod test_support;

pub(crate) const NAME: &str = "backlog";

/// The reason `start` gives when `backlog.db` cannot be opened or migrated. The
/// error's own text names the data folder's path, so it is not passed on.
const STORE_OPEN_FAILED: &str = "store_open_failed";

/// Opens `backlog.db` in the tool's data folder and holds it until shutdown.
/// The tool types nothing, so `ctx.typing` is never used.
pub(crate) async fn start(ctx: ToolContext) -> Result<ToolHandle, ToolError> {
    let store = store::TursoBacklogStore::open(&ctx.data_folder)
        .await
        .map_err(|_| ToolError {
            reason: STORE_OPEN_FAILED,
        })?;
    let mut shutdown = ctx.shutdown;
    let task = tokio::spawn(async move {
        // Returns on a shutdown signal and when the sender is dropped.
        let _ = shutdown.changed().await;
        drop(store);
    });
    Ok(ToolHandle {
        routes: Routes::default(),
        task,
    })
}

fn now_ms() -> i64 {
    (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
}
