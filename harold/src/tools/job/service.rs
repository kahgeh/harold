//! The job tool's gRPC service. It trusts local callers (ADR 0002) and checks every
//! field itself.

use std::sync::Arc;

use harold_api::tools::job::v1 as pb;
use harold_api::tools::job::v1::job_service_server::JobService;
use tonic::{Request, Response, Status};

use super::awake::KeepAwake;
use super::feed::JobsFeed;
use super::obs;
use super::process::ProcessPort;
use super::store::{JobRow, JobState, JobStore, NewJob};
use super::unit::UnitPort;
use super::validate;

/// Milliseconds since the epoch, now.
pub(super) type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

pub(super) struct JobToolService {
    store: Arc<JobStore>,
    processes: Arc<dyn ProcessPort>,
    units: Arc<dyn UnitPort>,
    /// This machine runs systemd. A unit job is refused where it does not.
    systemd: bool,
    now_ms: Clock,
    feed: Arc<JobsFeed>,
    awake: Option<Arc<KeepAwake>>,
}

/// What `add` tells the agent when the unit does not stay loaded after it ends.
const REMAIN_WARNING: &str =
    "the exit code will be unknown: start the unit with --remain-after-exit";

impl JobToolService {
    pub(super) fn new(
        store: Arc<JobStore>,
        processes: Arc<dyn ProcessPort>,
        units: Arc<dyn UnitPort>,
        systemd: bool,
        now_ms: Clock,
        feed: Arc<JobsFeed>,
    ) -> Self {
        Self {
            store,
            processes,
            units,
            systemd,
            now_ms,
            feed,
            awake: None,
        }
    }

    /// Lets `list` show `keeps awake`, and `remove` delete the keep-awake file.
    pub(super) fn with_awake(mut self, awake: Option<Arc<KeepAwake>>) -> Self {
        self.awake = awake;
        self
    }

    fn keeps_awake(&self, name: &str) -> bool {
        self.awake
            .as_ref()
            .is_some_and(|awake| awake.has_file(name))
    }
}

fn unavailable() -> Status {
    Status::unavailable("store_failed")
}

pub(super) fn to_pb(job: &JobRow, keeps_awake: bool) -> pb::Job {
    pb::Job {
        name: job.name.clone(),
        what: job.what.clone(),
        pane_id: job.pane_id.clone(),
        pid: job.pid,
        started_at_ms: job.started_at_ms,
        due_at_ms: job.due_at_ms,
        ended_at_ms: job.ended_at_ms,
        state: match job.state {
            JobState::Running => "running",
            JobState::Ended | JobState::Removed => "ended",
        }
        .into(),
        end_line: job.end_line.clone().unwrap_or_default(),
        unit: job.unit.clone(),
        keeps_awake,
        exit_code: job.exit_code.unwrap_or(0),
        exit_known: job.exit_code.is_some(),
    }
}

#[tonic::async_trait]
impl JobService for JobToolService {
    async fn add(
        &self,
        request: Request<pb::AddRequest>,
    ) -> Result<Response<pb::AddResponse>, Status> {
        let req = request.into_inner();
        validate::check_add(
            &req.name,
            req.pid,
            &req.unit,
            req.for_secs,
            &req.pane_id,
            &req.what,
        )
        .map_err(|field| Status::invalid_argument(format!("invalid {field}")))?;
        let mut warning = String::new();
        let process_start_s = if req.unit.is_empty() {
            let Some(started) = self.processes.start_time(req.pid) else {
                return Err(Status::not_found("no_such_process"));
            };
            started
        } else {
            if !self.systemd {
                return Err(Status::failed_precondition("no_systemd"));
            }
            match self.units.show(&req.unit, req.user).await {
                Ok(state) if state.exists() => {}
                Ok(_) => return Err(Status::not_found("no_such_unit")),
                Err(_) => return Err(Status::unavailable("systemctl_failed")),
            }
            if self.units.remain_after_exit(&req.unit, req.user).await == Some(false) {
                warning = REMAIN_WARNING.into();
            }
            0
        };
        let now = (self.now_ms)();
        let job = NewJob {
            name: req.name.clone(),
            what: req.what,
            pane_id: req.pane_id,
            pid: req.pid,
            process_start_s,
            started_at_ms: now,
            due_at_ms: now + i64::from(req.for_secs) * 1_000,
            unit: req.unit,
            user: req.user,
        };
        match self.store.add(&job).await {
            Ok(true) => {
                crate::telemetry::feature_used("agent-jobs");
                if !job.unit.is_empty() {
                    crate::telemetry::feature_used("agent-jobs-hosts");
                }
                obs::job_added(&job.name, &job.pane_id, i64::from(req.for_secs));
                if let Some(awake) = &self.awake {
                    awake.begin(&job.name, now);
                }
                self.feed.refresh(&self.store, now).await;
                Ok(Response::new(pb::AddResponse {
                    name: job.name,
                    started_at_ms: job.started_at_ms,
                    due_at_ms: job.due_at_ms,
                    warning,
                }))
            }
            Ok(false) => Err(Status::failed_precondition("name_in_use")),
            Err(_) => Err(unavailable()),
        }
    }

    async fn list(
        &self,
        _request: Request<pb::ListRequest>,
    ) -> Result<Response<pb::ListResponse>, Status> {
        let jobs = self
            .store
            .shown((self.now_ms)())
            .await
            .map_err(|_| unavailable())?;
        Ok(Response::new(pb::ListResponse {
            jobs: jobs
                .iter()
                .map(|job| to_pb(job, self.keeps_awake(&job.name)))
                .collect(),
        }))
    }

    async fn remove(
        &self,
        request: Request<pb::RemoveRequest>,
    ) -> Result<Response<pb::RemoveResponse>, Status> {
        let name = request.into_inner().name;
        match self.store.remove(&name, (self.now_ms)()).await {
            Ok(Some(job)) => {
                if let Some(awake) = &self.awake {
                    awake.clear(&job.name);
                }
                crate::telemetry::count_job_end("removed", "none");
                self.feed.refresh(&self.store, (self.now_ms)()).await;
                Ok(Response::new(pb::RemoveResponse { name: job.name }))
            }
            Ok(None) => Err(Status::not_found("unknown_job")),
            Err(_) => Err(unavailable()),
        }
    }
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;
