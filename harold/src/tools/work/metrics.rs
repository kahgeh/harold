//! The work tool's metrics: thin calls into `crate::telemetry`. The gauges count
//! the open records of the current file.

use std::collections::BTreeMap;

use tracing::info;

use crate::telemetry::{self, WorkCount};

use super::check::Mismatch;
use super::lead_time;
use super::schema::{Record, Status};
use super::store::{PutOutcome, WorkFile};

/// A record waits long when it is at `waiting for the user` for more than this.
const WAITING_LONG_MS: i64 = 24 * 60 * 60 * 1000;

/// Reads the open records of `file` and sets both gauges from them. A failed
/// read changes nothing, so the gauges keep the last good counts.
pub(super) async fn refresh(file: &WorkFile, now_ms: i64) {
    if let Ok(open) = file.open_records().await {
        note_open(&open, now_ms);
    }
}

/// Sets `harold.work.records` and `harold.work.waiting_long` from the open records.
fn note_open(open: &[Record], now_ms: i64) {
    let mut records: BTreeMap<(&'static str, &'static str), u64> = BTreeMap::new();
    let mut waiting_long: BTreeMap<&'static str, u64> = BTreeMap::new();
    for record in open {
        let priority = record.priority.as_str();
        *records
            .entry((priority, record.status.as_str()))
            .or_default() += 1;
        if record.status == Status::WaitingForTheUser
            && now_ms - record.status_since_ms > WAITING_LONG_MS
        {
            *waiting_long.entry(priority).or_default() += 1;
        }
    }
    telemetry::note_work_records(
        records
            .into_iter()
            .map(|((priority, status), count)| WorkCount {
                priority,
                status,
                count,
            })
            .collect(),
    );
    telemetry::note_work_waiting_long(waiting_long.into_iter().collect());
}

/// Records the time at the old status when a write changed the status of the
/// record `name`. The priority is the record's priority after the write.
pub(super) async fn status_left(file: &WorkFile, name: &str, outcome: PutOutcome, now_ms: i64) {
    let PutOutcome::StatusChanged { from, since_ms } = outcome else {
        return;
    };
    let Ok(Some(record)) = file.get(name).await else {
        return;
    };
    let seconds = (now_ms - since_ms).max(0) as f64 / 1000.0;
    telemetry::record_work_status_duration(from.as_str(), record.priority.as_str(), seconds);
}

/// Records the closing figures when a write moved the record `name` to
/// `checked by the user`: one count of value, the lead time, the four phase
/// times and the slices. A closed record cannot be written to the same status
/// again, so the figures are recorded once.
pub(super) async fn closed(file: &WorkFile, name: &str, outcome: PutOutcome, now_ms: i64) {
    if !matches!(outcome, PutOutcome::StatusChanged { .. }) {
        return;
    }
    let Ok(Some(record)) = file.get(name).await else {
        return;
    };
    if record.status != Status::CheckedByTheUser {
        return;
    }
    let Ok(moves) = file.moves(name).await else {
        return;
    };
    let phases = lead_time::phases(&moves);
    let slices = lead_time::slices(&record.links);
    let lead_s = (now_ms - record.created_at_ms).max(0) / 1000;
    telemetry::count_work_checked(record.priority.as_str());
    telemetry::record_work_lead_time("whole", lead_s as f64);
    let parts = [
        ("discovery", phases.discovery),
        ("spec_and_approval", phases.spec_and_approval),
        ("build_and_merge", phases.build_and_merge),
        ("release_and_check", phases.release_and_check),
    ];
    for (phase, ms) in parts {
        telemetry::record_work_lead_time(phase, ms as f64 / 1000.0);
    }
    telemetry::record_work_slices(slices);
    info!(
        record = name,
        lead_s,
        slices,
        imported = lead_time::is_imported(&moves),
        "work record closed"
    );
}

/// Counts each mismatch of one check by record and kind.
pub(super) fn mismatches(found: &[Mismatch]) {
    for mismatch in found {
        telemetry::count_work_mismatch(&mismatch.record, mismatch.kind);
    }
}

/// Counts one rotation: `done` or `failed`.
pub(super) fn rotation(outcome: &'static str) {
    telemetry::count_work_rotation(outcome);
}
