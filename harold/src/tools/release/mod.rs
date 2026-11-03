//! The release tool: what each product's next release holds. Outside this module only
//! `NAME`, `start` and `render` are used. The tool only reads, so it keeps no store. It
//! also publishes the `merged_not_released` list of work waiting every 10 minutes.

use harold_api::tools::release::v1::release_service_server::ReleaseServiceServer;
use tonic::service::Routes;

use crate::tools::{ToolContext, ToolError, ToolHandle};

mod classify;
mod gather;
mod inventory;
mod model;
pub(crate) mod render;
mod service;
mod title;

pub(crate) const NAME: &str = "release";

/// Serves the release service until shutdown.
pub(crate) async fn start(ctx: ToolContext) -> Result<ToolHandle, ToolError> {
    // The publisher has its own service value: the service holds no state.
    let publisher = service::ReleaseToolService::new(ctx.pod_registry.clone());
    let service = service::ReleaseToolService::new(ctx.pod_registry);
    inventory::register();
    // Ends on a shutdown signal and when the sender is dropped.
    let task = tokio::spawn(inventory::run(publisher, inventory::PERIOD, ctx.shutdown));
    Ok(ToolHandle {
        routes: Routes::new(ReleaseServiceServer::new(service)),
        task,
    })
}
