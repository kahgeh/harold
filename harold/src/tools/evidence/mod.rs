//! The evidence tool: after a release, it checks that the evidence queries of each merged
//! spec find their signal in ClickHouse, and writes the result in the pod folder.

use std::path::PathBuf;

use harold_api::tools::evidence::v1::evidence_service_server::EvidenceServiceServer;
use tonic::service::Routes;

use crate::settings::FactoryFlowSettings;
use crate::tools::{ToolError, ToolHandle};

mod check;
mod files;
mod service;
mod state;

pub(crate) const NAME: &str = "evidence";

/// What the host gives the evidence tool: the path of `products.toml`, the `[factory_flow]`
/// settings and the shutdown signal.
pub(crate) struct Launch {
    pub registry: Option<PathBuf>,
    pub settings: FactoryFlowSettings,
    pub shutdown: tokio::sync::watch::Receiver<()>,
}

/// Serves the evidence service until shutdown.
pub(crate) async fn start(launch: Launch) -> Result<ToolHandle, ToolError> {
    crate::telemetry::register_evidence_counters();
    let service = service::EvidenceToolService::new(launch.registry, launch.settings);
    let mut shutdown = launch.shutdown;
    // Ends on a shutdown signal and when the sender is dropped.
    let task = tokio::spawn(async move {
        let _ = shutdown.changed().await;
    });
    Ok(ToolHandle {
        routes: Routes::new(EvidenceServiceServer::new(service)),
        task,
    })
}

#[cfg(test)]
#[path = "state_tests.rs"]
mod state_tests;

#[cfg(test)]
#[path = "files_tests.rs"]
mod files_tests;

#[cfg(test)]
#[path = "check_tests.rs"]
mod check_tests;

#[cfg(test)]
#[path = "service_tests.rs"]
mod service_tests;
