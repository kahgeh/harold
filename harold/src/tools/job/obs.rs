//! Every log line of the job tool. None takes a command line, an output or the agent's
//! text: the fields are the job name, the pane id, seconds and fixed reason words.

use tracing::{info, warn};

use super::store::JobRow;
use crate::harold::AgentJob;

/// A job was accepted.
pub(super) fn job_added(name: &str, pane_id: &str, due_in_s: i64) {
    info!(job.name = %name, pane_id = %pane_id, due_in_s, "job added");
}

/// A job ended: its process is gone or has a new start time.
pub(super) fn job_ended(job: &JobRow, exit_code: Option<i32>, run_s: i64, past_due_s: i64) {
    match exit_code {
        Some(exit_code) => {
            info!(job.name = %job.name, pane_id = %job.pane_id, run_s, past_due_s, exit_code, "job ended");
        }
        None => info!(job.name = %job.name, pane_id = %job.pane_id, run_s, past_due_s, "job ended"),
    }
}

/// The master saw the end of a slave's job. `exit_code` is left out when it is unknown.
pub(super) fn slave_job_ended(job: &AgentJob, run_s: i64, past_due_s: i64) {
    let (host, name, pane_id) = (&job.host, &job.name, &job.pane_id);
    if job.exit_known {
        let exit_code = job.exit_code;
        info!(host = %host, job.name = %name, pane_id = %pane_id, run_s, past_due_s, exit_code, "job ended");
    } else {
        info!(host = %host, job.name = %name, pane_id = %pane_id, run_s, past_due_s, "job ended");
    }
}

/// The end line of a slave's job did not reach the agent and cannot.
pub(super) fn slave_end_not_told(host: &str, name: &str, reason: &str) {
    info!(host = %host, job.name = %name, reason = %reason, "job end not told");
}

/// A slave's job has run in `unknown` for 30 minutes: the master cannot see its host.
pub(super) fn host_not_reached(job: &AgentJob) {
    info!(host = %job.host, job.name = %job.name, pane_id = %job.pane_id, "job host not reached");
}

/// The end line did not reach the agent and cannot: `reason` is `no_pane` or `failed`.
pub(super) fn end_not_told(job: &JobRow, reason: &str) {
    info!(job.name = %job.name, pane_id = %job.pane_id, reason = %reason, "job end not told");
}

/// A job wrote its first keep-awake file. `folder` is the setting's text, not a path.
pub(super) fn keeps_machine_awake(name: &str, folder: &str) {
    info!(job.name = %name, folder = %folder, "job keeps machine awake");
}

/// A keep-awake write failed. `reason` is `not_found`, `permission_denied` or `other`.
pub(super) fn keep_awake_failed(name: &str, folder: &str, reason: &str) {
    warn!(job.name = %name, folder = %folder, reason = %reason, "job keep awake failed");
}
