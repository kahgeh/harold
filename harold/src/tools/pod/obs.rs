//! The log line and the counter of the pod tool. The line holds the product, the role and
//! the outcome word: never a session name, a folder or a path.

use tracing::info;

/// One `Who` lookup ended with `outcome`.
pub(super) fn lookup(product: &str, role: &str, outcome: &str) {
    info!(product = %product, role = %role, outcome = %outcome, "pod role lookup");
    crate::telemetry::record_pod_role_lookup(role, outcome, "who");
}
