//! The release tool: what each product's next release holds. Outside this module only
//! `NAME` and `start` are used. The tool only reads, so it keeps no store.

use harold_api::tools::release::v1::release_service_server::ReleaseServiceServer;
use tonic::service::Routes;

use crate::tools::{ToolContext, ToolError, ToolHandle};

mod classify;
mod gather;
mod model;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the CLI prints with it in a later task")
)]
mod render;
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
