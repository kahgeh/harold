//! The watch tool's gRPC service. It trusts local callers (ADR 0002) and checks every
//! field itself.

use std::sync::Arc;

use harold_api::tools::watch::v1 as pb;
use harold_api::tools::watch::v1::watch_service_server::WatchService;
use tonic::{Request, Response, Status};

use super::scheduler::{Scheduler, StartRefusal, StopRefusal};
use super::validate::{self, Refusal};
use crate::settings::WatchSettings;

pub(super) struct WatchToolService {
    scheduler: Arc<Scheduler>,
    settings: WatchSettings,
}

impl WatchToolService {
    pub(super) fn new(scheduler: Arc<Scheduler>, settings: WatchSettings) -> Self {
        Self {
            scheduler,
            settings,
        }
    }
}

fn unavailable() -> Status {
    Status::unavailable("store_failed")
}

fn micros_to_usd(micros: i64) -> f64 {
    micros as f64 / 1_000_000.0
}

#[tonic::async_trait]
impl WatchService for WatchToolService {
    async fn start_watch(
        &self,
        request: Request<pb::StartWatchRequest>,
    ) -> Result<Response<pb::StartWatchResponse>, Status> {
        let request = request.into_inner();
        let checked =
            validate::check_start(&request, &self.settings).map_err(|refusal| match refusal {
                Refusal::Invalid(field) => Status::invalid_argument(format!("invalid {field}")),
                Refusal::RunLimitAboveMax(max) => Status::invalid_argument(format!(
                    "run_limit_secs is above [watch] max_run_limit_secs ({max})"
                )),
                Refusal::UnknownSkill => Status::not_found("unknown_skill"),
            })?;
        let name = checked.name.clone();
        let ends_at_ms = self
            .scheduler
            .start(checked)
            .await
            .map_err(|refusal| match refusal {
                StartRefusal::TooManyWatches => Status::failed_precondition("too_many_watches"),
                StartRefusal::DailyBudgetSpent => Status::failed_precondition("daily_budget_spent"),
                StartRefusal::Unavailable => unavailable(),
            })?;
        Ok(Response::new(pb::StartWatchResponse { name, ends_at_ms }))
    }

    async fn stop_watch(
        &self,
        request: Request<pb::StopWatchRequest>,
    ) -> Result<Response<pb::StopWatchResponse>, Status> {
        let name = request.into_inner().name;
        self.scheduler
            .stop(&name)
            .await
            .map_err(|refusal| match refusal {
                StopRefusal::UnknownWatch => Status::not_found("unknown_watch"),
                StopRefusal::NotRunning => Status::failed_precondition("not_running"),
                StopRefusal::Unavailable => unavailable(),
            })?;
        Ok(Response::new(pb::StopWatchResponse {}))
    }

    async fn list_watches(
        &self,
        _request: Request<pb::ListWatchesRequest>,
    ) -> Result<Response<pb::ListWatchesResponse>, Status> {
        let listing = self.scheduler.list().await.map_err(|_| unavailable())?;
        let watches = listing
            .watches
            .into_iter()
            .map(|row| pb::Watch {
                name: row.name,
                folder: row.folder,
                skill: row.skill,
                report_to: row.report_to,
                every_run: row.every_run,
                running: row.running,
                ends_at_ms: row.ends_at_ms,
                every_minutes: row.every_minutes,
                runs: row.runs,
                reports: row.reports,
                cost_usd: micros_to_usd(row.cost_usd_micros),
                last_outcome: row.last_outcome.unwrap_or_default(),
                stop_reason: row.stop_reason.unwrap_or_default(),
                run_limit_secs: row.run_limit_secs,
            })
            .collect();
        Ok(Response::new(pb::ListWatchesResponse {
            watches,
            spent_usd: micros_to_usd(listing.spent_usd_micros),
        }))
    }
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;
