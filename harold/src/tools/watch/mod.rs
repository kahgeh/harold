//! The watch tool: an agent asks Harold to run one of its skills with a model on a
//! schedule, and Harold types `WATCH` lines to the chief or to the agent. Outside this
//! module only `NAME` and `start` are used.
//!
//! Not the agent monitor's watching of agents, and not the `WatchAgentStates` stream.

use std::sync::Arc;

use harold_api::tools::watch::v1::watch_service_server::WatchServiceServer;
use tonic::service::Routes;

use crate::tools::{ToolContext, ToolError, ToolHandle};

mod answer;
mod clock;
mod facts;
mod obs;
mod report;
mod runner;
mod scheduler;
mod service;
mod store;
#[cfg(test)]
mod test_support;
mod validate;

pub(crate) const NAME: &str = "watch";

/// The reason `start` gives when `watch.db` cannot be opened or migrated. The error's
/// own text names the data folder's path, so it is not passed on.
const STORE_OPEN_FAILED: &str = "store_open_failed";

/// The reason `start` gives when the host has no way to type into a pane.
const NO_TYPING: &str = "no_typing";

/// Opens `watch.db` in the tool's data folder, resumes the watches that were running,
/// and serves the watch service until shutdown.
pub(crate) async fn start(ctx: ToolContext) -> Result<ToolHandle, ToolError> {
    let typing = ctx.typing.ok_or(ToolError { reason: NO_TYPING })?;
    let store = store::WatchStore::open(&ctx.data_folder)
        .await
        .map_err(|_| ToolError {
            reason: STORE_OPEN_FAILED,
        })?;
    let scheduler = scheduler::Scheduler::new(scheduler::Deps {
        store: Arc::new(store),
        runner: Arc::new(runner::ClaudeRunner::new(
            ctx.cli_path,
            ctx.model_environment,
        )),
        typing,
        agents: ctx.agents,
        pods: ctx.pods,
        chief_pane: ctx.chief_pane,
        settings: ctx.watch_settings.clone(),
        offset: crate::telemetry::local_offset(),
        shutdown: ctx.shutdown.clone(),
    });
    let service = service::WatchToolService::new(Arc::clone(&scheduler), ctx.watch_settings);
    let task = tokio::spawn(scheduler::run(scheduler, ctx.shutdown));
    Ok(ToolHandle {
        routes: Routes::new(WatchServiceServer::new(service)),
        task,
    })
}
