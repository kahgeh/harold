//! Every log line of the decision tool. None takes a title or a details text: the fields
//! are the id, fixed words, the place word and numbers.

use tracing::{info, warn};

use super::store::DecisionRow;

/// A decision record was added. `decide_s` is left out when the question time is not known.
pub(super) fn added(row: &DecisionRow, place: &str, decide_s: Option<i64>) {
    match decide_s {
        Some(decide_s) => info!(
            decision.id = %row.id, role = %row.role, kind = %row.kind,
            details_owner = %row.details_owner, accountable = %row.accountable,
            place = %place, link = %row.link, decide_s, "decision added"
        ),
        None => info!(
            decision.id = %row.id, role = %row.role, kind = %row.kind,
            details_owner = %row.details_owner, accountable = %row.accountable,
            place = %place, link = %row.link, "decision added"
        ),
    }
}

/// A decision record was evaluated.
pub(super) fn evaluated(row: &DecisionRow, to_evaluate_days: i64) {
    info!(
        decision.id = %row.id, process_grade = %row.process_grade,
        outcome_grade = %row.outcome_grade, to_evaluate_days, "decision evaluated"
    );
}

/// An open record became due.
pub(super) fn due(row: &DecisionRow, days_open: i64) {
    info!(decision.id = %row.id, days_open, "decision due");
}

/// A write was refused.
pub(super) fn refused(reason: &str) {
    warn!(reason = %reason, "decision refused");
}
