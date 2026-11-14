//! The due check: each open record whose check date has come becomes due. It runs at start
//! and then once a day at 00:05 local time.

use std::sync::Arc;
use std::time::Duration;

use time::{OffsetDateTime, Time, UtcOffset};

use super::obs;
use super::service::Clock;
use super::store::{DecisionRow, DecisionStore};

const DAY_MS: i64 = 86_400_000;

pub(super) struct Checker {
    store: Arc<DecisionStore>,
    now_ms: Clock,
    organisation: String,
    /// The local time zone: the check date is a local date.
    offset: UtcOffset,
}

impl Checker {
    pub(super) fn new(
        store: Arc<DecisionStore>,
        now_ms: Clock,
        organisation: String,
        offset: UtcOffset,
    ) -> Self {
        Self {
            store,
            now_ms,
            organisation,
            offset,
        }
    }

    /// Sets each open record whose check date is today or before to due, logs each one, and
    /// sets the gauge. Returns the records that changed.
    pub(super) async fn pass(&self) -> Vec<DecisionRow> {
        let now = (self.now_ms)();
        let Some(moment) = local(now, self.offset) else {
            return Vec::new();
        };
        let today = format!(
            "{:04}-{:02}-{:02}",
            moment.year(),
            u8::from(moment.month()),
            moment.day()
        );
        let turned = self.store.mark_due(&today).await.unwrap_or_default();
        for row in &turned {
            obs::due(row, (now - row.added_at_ms).max(0) / DAY_MS);
        }
        if let Ok((open, due)) = self.store.counts().await {
            crate::telemetry::set_decisions_open(&self.organisation, open, due);
        }
        turned
    }
}

fn local(now_ms: i64, offset: UtcOffset) -> Option<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp(now_ms.div_euclid(1000))
        .ok()
        .map(|moment| moment.to_offset(offset))
}

/// The time until the next 00:05 local time after `now_ms`: never zero.
pub(super) fn next_run_delay(now_ms: i64, offset: UtcOffset) -> Duration {
    let Some(moment) = local(now_ms, offset) else {
        return Duration::from_secs(3600);
    };
    let run_time = Time::from_hms(0, 5, 0).unwrap_or(Time::MIDNIGHT);
    let mut next = moment.replace_time(run_time);
    if next <= moment {
        next += time::Duration::days(1);
    }
    let wait = next - moment;
    Duration::from_secs(u64::try_from(wait.whole_seconds()).unwrap_or(3600))
}

/// Runs one pass at once, then one a day at 00:05 local time. Ends on the shutdown signal,
/// and when its sender is dropped.
pub(super) async fn run(checker: Checker, mut shutdown: tokio::sync::watch::Receiver<()>) {
    loop {
        checker.pass().await;
        let wait = next_run_delay((checker.now_ms)(), checker.offset);
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            _ = shutdown.changed() => return,
        }
    }
}

#[cfg(test)]
#[path = "check_tests.rs"]
mod tests;
