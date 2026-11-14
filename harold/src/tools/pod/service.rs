//! The pod tool's gRPC service. It trusts local callers (ADR 0002).

use std::sync::Arc;

use harold_api::tools::pod::v1 as pb;
use harold_api::tools::pod::v1::pod_service_server::PodService;
use tonic::{Request, Response, Status};

use super::obs;
use super::session_file::SessionFiles;
use crate::pod_roles::{PodRoles, RoleRecord};
use crate::tools::job::process::ProcessPort;

/// The roles an agent holds for a pod.
const POD_ROLES: [&str; 3] = ["chief", "pnem", "pod_coordination"];

pub(super) struct PodToolService {
    roles: Arc<PodRoles>,
    processes: Arc<dyn ProcessPort>,
    sessions: SessionFiles,
}

impl PodToolService {
    pub(super) fn new(
        roles: Arc<PodRoles>,
        processes: Arc<dyn ProcessPort>,
        sessions: SessionFiles,
    ) -> Self {
        Self {
            roles,
            processes,
            sessions,
        }
    }

    /// The record of the role, the live check and the session name, or the reason word.
    fn find(&self, product: &str, role: &str) -> Result<(String, RoleRecord), &'static str> {
        let record = self.roles.get(product, role).ok_or("no_record")?;
        let running = self.processes.start_time(record.agent_pid);
        let same_process = running.is_some_and(|started| {
            record.agent_proc_start == 0 || started == record.agent_proc_start
        });
        if !same_process {
            return Err("agent_gone");
        }
        let name = self
            .sessions
            .name(record.agent_pid)
            .map_err(|_| "session_file_unreadable")?;
        Ok((name, record))
    }
}

fn status_of(reason: &'static str) -> Status {
    match reason {
        "no_record" => Status::not_found(reason),
        "agent_gone" => Status::failed_precondition(reason),
        _ => Status::unavailable(reason),
    }
}

#[tonic::async_trait]
impl PodService for PodToolService {
    async fn who(
        &self,
        request: Request<pb::WhoRequest>,
    ) -> Result<Response<pb::WhoResponse>, Status> {
        let request = request.into_inner();
        if !POD_ROLES.contains(&request.role.as_str()) {
            return Err(Status::invalid_argument("bad_role"));
        }
        let product = match (request.role.as_str(), request.product.as_str()) {
            ("chief", "") => "-",
            (_, product) => product,
        };
        let found = self.find(product, &request.role);
        obs::lookup(
            product,
            &request.role,
            found.as_ref().map_or_else(|reason| *reason, |_| "found"),
        );
        let (session_name, record) = found.map_err(status_of)?;
        Ok(Response::new(pb::WhoResponse {
            session_name,
            record: Some(pb::PodRoleRecord {
                pane_id: record.pane_id,
                session_id: record.session_id,
                agent_pid: record.agent_pid,
                agent_proc_start: record.agent_proc_start,
                folder: record.folder,
                transcript_path: record.transcript_path,
                start_file: record.start_file,
                started_at_ms: record.started_at_ms,
            }),
        }))
    }
}
