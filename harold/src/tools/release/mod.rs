//! The release tool: what each product's next release holds. Outside this module only
//! `NAME`, `start` and `render` are used. The tool only reads, so it keeps no store.

use harold_api::tools::release::v1::release_service_server::ReleaseServiceServer;
use tonic::service::Routes;

use crate::tools::{ToolContext, ToolError, ToolHandle};

mod classify;
mod gather;
mod model;
pub(crate) mod render;
mod service;
mod title;

pub(crate) const NAME: &str = "release";

/// Serves the release service until shutdown.
pub(crate) async fn start(ctx: ToolContext) -> Result<ToolHandle, ToolError> {
    let service = service::ReleaseToolService::new(ctx.pod_registry);
    let mut shutdown = ctx.shutdown;
    let task = tokio::spawn(async move {
        // Returns on a shutdown signal and when the sender is dropped.
        let _ = shutdown.changed().await;
    });
    Ok(ToolHandle {
        routes: Routes::new(ReleaseServiceServer::new(service)),
        task,
    })
}
