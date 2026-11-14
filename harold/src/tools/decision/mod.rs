//! The decision tool: the records of decisions that a role made from evidence, each with a
//! check date, so the coach can grade them later. Outside this module only `NAME`,
//! `configure`, `start` and the `command` text are used.
//!
//! A decision record is not a work record decision (ADR 0028): it never rotates.

use std::path::PathBuf;
use std::sync::Arc;

use harold_api::tools::decision::v1::decision_service_server::DecisionServiceServer;
use tonic::service::Routes;

use crate::settings::DecisionSettings;
use crate::tools::{ToolContext, ToolError, ToolHandle};

mod check;
pub(crate) mod command;
mod details;
mod obs;
mod service;
mod store;
#[cfg(test)]
mod test_support;
mod validate;

pub(crate) const NAME: &str = "decision";

static SETTINGS: std::sync::OnceLock<(DecisionSettings, Option<PathBuf>)> =
    std::sync::OnceLock::new();

/// Sets the `[decision]` settings of this run of Harold and the vault folder from
/// `[knowledge] vault`. The host calls it before `start`. Without a call, the tool uses the
/// shipped defaults and has no vault.
pub(crate) fn configure(settings: DecisionSettings, vault: Option<PathBuf>) {
    let _ = SETTINGS.set((settings, vault));
}

/// The reason `start` gives when `decisions.db` cannot be opened. The error's own text
/// names the data folder's path, so it is not passed on.
const STORE_OPEN_FAILED: &str = "store_open_failed";

fn now_ms() -> i64 {
    i64::try_from(time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000)
        .unwrap_or(i64::MAX)
}

/// Opens `decisions.db` in the tool's data folder, runs the due check, and serves the
/// decision service until shutdown.
pub(crate) async fn start(ctx: ToolContext) -> Result<ToolHandle, ToolError> {
    let (settings, vault) = SETTINGS.get().cloned().unwrap_or_default();
    let store = store::DecisionStore::open(&ctx.data_folder)
        .await
        .map(Arc::new)
        .map_err(|_| ToolError {
            reason: STORE_OPEN_FAILED,
        })?;
    crate::telemetry::register_decision_instruments(&settings.organisation, &settings.kinds);
    let clock: service::Clock = Arc::new(now_ms);
    let details = details::FsDetails::new(
        ctx.pod_registry.is_some(),
        ctx.products,
        vault,
        settings.personal_repository_path(),
    );
    let checker = check::Checker::new(
        Arc::clone(&store),
        Arc::clone(&clock),
        settings.organisation.clone(),
        crate::telemetry::local_offset(),
    );
    let service = service::DecisionToolService::new(
        store,
        ctx.chief_pane,
        Arc::new(details),
        clock,
        settings,
    );
    let task = tokio::spawn(check::run(checker, ctx.shutdown));
    Ok(ToolHandle {
        routes: Routes::new(DecisionServiceServer::new(service)),
        task,
    })
}
