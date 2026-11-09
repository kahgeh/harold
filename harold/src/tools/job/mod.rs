//! The job tool: an agent records a long command that it started outside its pane.
//! Harold checks the process, and types one end line into the agent's pane when the
//! process ends. Outside this module only `NAME` and `start` are used.
//!
//! An agent job is not a background job of Harold's own threads.

use std::sync::Arc;

use harold_api::tools::job::v1::job_service_server::JobServiceServer;
use tonic::service::Routes;

use crate::settings::JobSettings;
use crate::tools::{ToolContext, ToolError, ToolHandle};

mod awake;
mod check;
mod feed;
mod obs;
pub(crate) mod process;
mod service;
mod store;
#[cfg(test)]
mod test_support;
mod unit;
mod validate;

pub(crate) use feed::{subscribe, with_jobs};

pub(crate) const NAME: &str = "job";

static SETTINGS: std::sync::OnceLock<JobSettings> = std::sync::OnceLock::new();

/// Sets the `[job]` settings of this run of Harold. The host calls it before `start`.
/// Without a call, the job tool keeps no file awake.
pub(crate) fn configure(settings: JobSettings) {
    let _ = SETTINGS.set(settings);
}

/// The reason `start` gives when `jobs.db` cannot be opened. The error's own text names
/// the data folder's path, so it is not passed on.
const STORE_OPEN_FAILED: &str = "store_open_failed";

/// The reason `start` gives when the host has no way to type into a pane.
const NO_TYPING: &str = "no_typing";

fn now_ms() -> i64 {
    i64::try_from(time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000)
        .unwrap_or(i64::MAX)
}

/// Opens `jobs.db` in the tool's data folder, checks the jobs that were running, and
/// serves the job service until shutdown.
pub(crate) async fn start(ctx: ToolContext) -> Result<ToolHandle, ToolError> {
    let typing = ctx.typing.ok_or(ToolError { reason: NO_TYPING })?;
    let store = store::JobStore::open(&ctx.data_folder)
        .await
        .map_err(|_| ToolError {
            reason: STORE_OPEN_FAILED,
        })?;
    crate::telemetry::register_job_instruments();
    let store = Arc::new(store);
    let processes: Arc<dyn process::ProcessPort> = Arc::new(process::SystemProcesses);
    let units: Arc<dyn unit::UnitPort> = Arc::new(unit::SystemUnits);
    let clock: service::Clock = Arc::new(now_ms);
    let feed = Arc::clone(feed::JobsFeed::global());
    let awake = SETTINGS.get().and_then(awake::KeepAwake::new).map(Arc::new);
    if let Some(awake) = &awake {
        let running = store.running().await.unwrap_or_default();
        let names: Vec<String> = running.into_iter().map(|job| job.name).collect();
        awake.sweep(&names);
    }
    let checker = check::Checker::new(
        Arc::clone(&store),
        Arc::clone(&processes),
        Arc::clone(&units),
        typing,
        Arc::clone(&clock),
        Arc::clone(&feed),
    )
    .with_awake(awake.clone());
    let service = service::JobToolService::new(
        store,
        processes,
        units,
        cfg!(target_os = "linux"),
        clock,
        feed,
    )
    .with_awake(awake);
    let task = tokio::spawn(run(checker, ctx.shutdown));
    Ok(ToolHandle {
        routes: Routes::new(JobServiceServer::new(service)),
        task,
    })
}

/// Checks the jobs at once, which is the check after a restart, and then every 15
/// seconds. Ends on the shutdown signal, and when its sender is dropped.
async fn run(checker: check::Checker, mut shutdown: tokio::sync::watch::Receiver<()>) {
    let mut tick = tokio::time::interval(check::PERIOD);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = tick.tick() => {
                let report = checker.pass().await;
                crate::telemetry::set_jobs_past_due(u64::from(report.past_due));
            }
            _ = shutdown.changed() => return,
        }
    }
}
