//! Every log line of the job tool. None takes a command line, an output or the agent's
//! text: the fields are the job name, the pane id, seconds and fixed reason words.

use tracing::{info, warn};

use super::store::JobRow;

/// A job was accepted.
pub(super) fn job_added(name: &str, pane_id: &str, due_in_s: i64) {
    info!(job.name = %name, pane_id = %pane_id, due_in_s, "job added");
}

/// A job ended: its process is gone or has a new start time.
pub(super) fn job_ended(job: &JobRow, run_s: i64, past_due_s: i64) {
    info!(job.name = %job.name, pane_id = %job.pane_id, run_s, past_due_s, "job ended");
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
