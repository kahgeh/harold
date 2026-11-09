//! The job tool's gRPC service. It trusts local callers (ADR 0002) and checks every
//! field itself.

use std::sync::Arc;

use harold_api::tools::job::v1 as pb;
use harold_api::tools::job::v1::job_service_server::JobService;
use tonic::{Request, Response, Status};

use super::obs;
use super::process::ProcessPort;
use super::store::{JobRow, JobState, JobStore, NewJob};
use super::validate;

/// Milliseconds since the epoch, now.
pub(super) type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

pub(super) struct JobToolService {
    store: Arc<JobStore>,
    processes: Arc<dyn ProcessPort>,
    now_ms: Clock,
}

impl JobToolService {
    pub(super) fn new(
        store: Arc<JobStore>,
        processes: Arc<dyn ProcessPort>,
        now_ms: Clock,
    ) -> Self {
        Self {
            store,
            processes,
            now_ms,
        }
    }
}

fn unavailable() -> Status {
    Status::unavailable("store_failed")
}

pub(super) fn to_pb(job: &JobRow) -> pb::Job {
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
    }
}

#[tonic::async_trait]
impl JobService for JobToolService {
    async fn add(
        &self,
        request: Request<pb::AddRequest>,
    ) -> Result<Response<pb::AddResponse>, Status> {
        let req = request.into_inner();
        validate::check_add(&req.name, req.pid, req.for_secs, &req.pane_id, &req.what)
            .map_err(|field| Status::invalid_argument(format!("invalid {field}")))?;
        let Some(process_start_s) = self.processes.start_time(req.pid) else {
            return Err(Status::not_found("no_such_process"));
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
        };
        match self.store.add(&job).await {
            Ok(true) => {
                crate::telemetry::feature_used("agent-jobs");
                obs::job_added(&job.name, &job.pane_id, i64::from(req.for_secs));
                Ok(Response::new(pb::AddResponse {
                name: job.name,
                started_at_ms: job.started_at_ms,
                due_at_ms: job.due_at_ms,
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
            jobs: jobs.iter().map(to_pb).collect(),
        }))
    }

    async fn remove(
        &self,
        request: Request<pb::RemoveRequest>,
    ) -> Result<Response<pb::RemoveResponse>, Status> {
        let name = request.into_inner().name;
        match self.store.remove(&name, (self.now_ms)()).await {
            Ok(Some(job)) => {
                crate::telemetry::count_job_end("removed", "none");
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
