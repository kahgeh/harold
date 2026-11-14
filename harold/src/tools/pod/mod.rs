//! The pod tool: which agent holds a pod role now. It answers `Who` from the pod role
//! record (`crate::pod_roles`). Outside this module only `NAME`, `start` and the `command`
//! text are used.

use std::path::PathBuf;
use std::sync::Arc;

use harold_api::tools::pod::v1::pod_service_server::PodServiceServer;
use tonic::service::Routes;

use crate::pod_roles::PodRoles;
use crate::tools::job::process::{ProcessPort, SystemProcesses};
use crate::tools::{ToolContext, ToolError, ToolHandle};

pub(crate) mod command;
mod obs;
mod service;
mod session_file;

pub(crate) const NAME: &str = "pod";

/// The reason `start` gives when the record folder cannot be made.
const FOLDER_FAILED: &str = "store_open_failed";

use service::PodToolService;
use session_file::SessionFiles;

/// Reads the pod role record from the tool's data folder, makes it the record that the RPC
/// `PodAgentStarted` and `pick_chief` use, and serves the pod service.
pub(crate) async fn start(ctx: ToolContext) -> Result<ToolHandle, ToolError> {
    std::fs::create_dir_all(&ctx.data_folder).map_err(|_| ToolError {
        reason: FOLDER_FAILED,
    })?;
    let processes: Arc<dyn ProcessPort> = Arc::new(SystemProcesses);
    let roles = Arc::new(PodRoles::open(&ctx.data_folder, Arc::clone(&processes)));
    crate::pod_roles::install(Arc::clone(&roles));
    crate::telemetry::register_pod_role_instruments();
    let sessions = SessionFiles::new(session_folder());
    let service = PodToolService::new(roles, processes, sessions);
    let mut shutdown = ctx.shutdown;
    let task = tokio::spawn(async move {
        // The tool has no background work: the task only ends when Harold stops.
        let _ = shutdown.changed().await;
    });
    Ok(ToolHandle {
        routes: Routes::new(PodServiceServer::new(service)),
        task,
    })
}

/// The folder of the agent's own session files. See `session_file`.
fn session_folder() -> PathBuf {
    session_file::default_folder()
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;
