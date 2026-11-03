//! The watch tool's gRPC service. It trusts local callers (ADR 0002) and checks every
//! field itself.

use std::sync::Arc;

use harold_api::tools::watch::v1 as pb;
use harold_api::tools::watch::v1::watch_service_server::WatchService;
use tonic::{Request, Response, Status};

use super::scheduler::{
    PauseRefusal, RemindRefusal, ResumeRefusal, Scheduler, StartRefusal, StopRefusal,
};
use super::store::WatchState;
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

/// The status for a request that failed its checks.
fn invalid_status(refusal: Refusal) -> Status {
    match refusal {
        Refusal::Invalid(field) => Status::invalid_argument(format!("invalid {field}")),
        Refusal::RunLimitAboveMax(max) => Status::invalid_argument(format!(
            "run_limit_secs is above [watch] max_run_limit_secs ({max})"
        )),
        Refusal::UnknownSkill => Status::not_found("unknown_skill"),
        Refusal::EveryOrCron => {
            Status::invalid_argument("give exactly one of every_minutes and cron")
        }
        Refusal::Cron(error) => Status::invalid_argument(error.to_string()),
        Refusal::TooOften => {
            Status::invalid_argument("this schedule runs more often than every 2 minutes")
        }
    }
}

fn start_status(refusal: StartRefusal) -> Status {
    match refusal {
        StartRefusal::TooManyWatches => Status::failed_precondition("too_many_watches"),
        StartRefusal::DailyBudgetSpent => Status::failed_precondition("daily_budget_spent"),
        StartRefusal::DailyBudgetUnset => Status::failed_precondition("daily_budget_unset"),
        StartRefusal::NameInUse => Status::failed_precondition("name_in_use"),
        StartRefusal::Unavailable => unavailable(),
    }
}

fn pause_status(refusal: PauseRefusal) -> Status {
    match refusal {
        PauseRefusal::UnknownWatch => Status::not_found("unknown_watch"),
        PauseRefusal::NotRunning => Status::failed_precondition("not_running"),
        PauseRefusal::BudgetDay => Status::failed_precondition("budget_day"),
        PauseRefusal::Unavailable => unavailable(),
    }
}

fn resume_status(refusal: ResumeRefusal) -> Status {
    match refusal {
        ResumeRefusal::UnknownWatch => Status::not_found("unknown_watch"),
        ResumeRefusal::NotPaused => Status::failed_precondition("not_paused"),
        ResumeRefusal::BudgetDay => Status::failed_precondition("budget_day"),
        ResumeRefusal::Unavailable => unavailable(),
    }
}

fn remind_status(refusal: RemindRefusal) -> Status {
    match refusal {
        RemindRefusal::NameInUse => Status::failed_precondition("name_in_use"),
        RemindRefusal::Unavailable => unavailable(),
    }
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
        let checked = validate::check_start(&request, &self.settings).map_err(invalid_status)?;
        let name = checked.name.clone();
        let ends_at_ms = self.scheduler.start(checked).await.map_err(start_status)?;
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
                paused: row.state == WatchState::Paused,
                paused_until_ms: row.paused_until_ms,
                pause_how: row.pause_how.unwrap_or_default(),
                cron: row.cron.unwrap_or_default(),
                checked_local: row.checked_local.unwrap_or_default(),
            })
            .collect();
        Ok(Response::new(pb::ListWatchesResponse {
            watches,
            spent_usd: micros_to_usd(listing.spent_usd_micros),
            reminders: listing
                .reminders
                .into_iter()
                .map(|reminder| pb::Reminder {
                    name: reminder.name,
                    report_to: reminder.report_to,
                    due_at_ms: reminder.due_at_ms,
                    cron: reminder.cron.unwrap_or_default(),
                })
                .collect(),
        }))
    }

    async fn pause(
        &self,
        request: Request<pb::PauseRequest>,
    ) -> Result<Response<pb::PauseResponse>, Status> {
        let checked = validate::check_pause(&request.into_inner()).map_err(invalid_status)?;
        let changed = self.scheduler.pause(checked).await.map_err(pause_status)?;
        Ok(Response::new(pb::PauseResponse {
            paused: changed.done,
            skipped: changed.skipped,
        }))
    }

    async fn resume(
        &self,
        request: Request<pb::ResumeRequest>,
    ) -> Result<Response<pb::ResumeResponse>, Status> {
        let checked = validate::check_resume(&request.into_inner()).map_err(invalid_status)?;
        let changed = self
            .scheduler
            .resume_watches(checked)
            .await
            .map_err(resume_status)?;
        Ok(Response::new(pb::ResumeResponse {
            resumed: changed.done,
            skipped: changed.skipped,
        }))
    }

    async fn remind(
        &self,
        request: Request<pb::RemindRequest>,
    ) -> Result<Response<pb::RemindResponse>, Status> {
        let now = self.scheduler.local_now().await;
        let checked =
            validate::check_remind(&request.into_inner(), &now).map_err(invalid_status)?;
        let due_at_ms = self
            .scheduler
            .remind(checked)
            .await
            .map_err(remind_status)?;
        Ok(Response::new(pb::RemindResponse { due_at_ms }))
    }
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;
