//! The time a work record spent in each phase, worked out from its moves when
//! it is checked by the user.

use super::schema::Status;
use super::store::Move;

/// The `from_status` of the move that brings in a record without history.
const IMPORTED: &str = "imported";

/// The milliseconds a record spent in each phase.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct Phases {
    pub(super) discovery: i64,
    pub(super) spec_and_approval: i64,
    pub(super) build_and_merge: i64,
    pub(super) release_and_check: i64,
}

/// Adds the time of each status to its phase. The time at a status is the gap
/// from its move to the next move. The last move ends no interval. A move back
/// adds to the phase again. A skipped phase stays at 0.
pub(super) fn phases(moves: &[Move]) -> Phases {
    let mut total = Phases::default();
    for pair in moves.windows(2) {
        let spent = (pair[1].at_ms - pair[0].at_ms).max(0);
        let Some(status) = Status::parse(&pair[0].to_status) else {
            continue;
        };
        let slot = match status {
            Status::Idea | Status::Discovery => &mut total.discovery,
            Status::SpecDrafted | Status::ArchitectChecked | Status::WaitingForTheUser => {
                &mut total.spec_and_approval
            }
            Status::Approved | Status::Building => &mut total.build_and_merge,
            Status::Merged | Status::Released => &mut total.release_and_check,
            Status::CheckedByTheUser => continue,
        };
        *slot += spent;
    }
    total
}

/// The number of `spec:` links.
pub(super) fn slices(links: &[String]) -> u64 {
    links.iter().filter(|l| l.starts_with("spec:")).count() as u64
}

/// True when the first move comes from `imported`.
pub(super) fn is_imported(moves: &[Move]) -> bool {
    moves.first().is_some_and(|m| m.from_status == IMPORTED)
}

#[cfg(test)]
#[path = "lead_time_tests.rs"]
mod tests;
