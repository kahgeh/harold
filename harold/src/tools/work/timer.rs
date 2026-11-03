//! The daily check: Harold ticks the rotation at start and then once a day,
//! logs and counts what it did, and sets the gauges.

use std::sync::Arc;
use std::time::Duration;

use time::format_description::well_known::Rfc3339;
use tokio::sync::watch;
use tokio::time::MissedTickBehavior;
use tracing::{debug, info, warn};

use super::NAME;
use super::metrics;
use super::rotation::{Clock, Outcome, Rotator};
use crate::settings::{MAX_ROTATION_DAYS, Settings, WorkSettings};

/// The time from one check to the next.
pub(super) const DAY: Duration = Duration::from_secs(24 * 60 * 60);

/// Reads the `[work] rotation_days` setting. `None` when the settings cannot be
/// read.
pub(super) type ReadDays = Box<dyn Fn() -> Option<u32> + Send + Sync>;

/// The reason in the log when the settings cannot be read. The error's own
/// text can name a path, so it is not logged.
const SETTINGS_UNREADABLE: &str = "settings_unreadable";

/// Reads `rotation_days` from the settings files and the environment again.
/// A value above the limit counts as unreadable, like a failed load.
pub(super) fn read_rotation_days() -> Option<u32> {
    accept_days(
        Settings::load()
            .ok()
            .map(|settings| settings.work.rotation_days),
    )
}

/// The value when it is at most `MAX_ROTATION_DAYS`, and `None` otherwise.
fn accept_days(days: Option<u32>) -> Option<u32> {
    days.filter(|days| *days <= MAX_ROTATION_DAYS)
}

/// Runs the check at start and then each `DAY`, until `shutdown` changes or its
/// sender is dropped. Each run reads `rotation_days` again. When the read
/// fails, the run uses the last value it read, or the default at the first run.
pub(super) async fn run(
    rotator: Arc<Rotator>,
    clock: Arc<dyn Clock>,
    read_days: ReadDays,
    mut shutdown: watch::Receiver<()>,
) {
    let mut interval = tokio::time::interval(DAY);
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut rotation_days = WorkSettings::default().rotation_days;
    loop {
        tokio::select! {
            _ = shutdown.changed() => return,
            _ = interval.tick() => {}
        }
        match read_days() {
            Some(days) => rotation_days = days,
            None => debug!(
                tool = NAME,
                reason = SETTINGS_UNREADABLE,
                "work settings not read"
            ),
        }
        check(&rotator, clock.as_ref(), rotation_days).await;
    }
}

/// Ticks the rotation once, logs and counts the outcome, and sets the gauges
/// from the current file.
pub(super) async fn check(rotator: &Rotator, clock: &dyn Clock, rotation_days: u32) {
    let outcome = rotator.tick(rotation_days).await;
    report(&outcome);
    if let Ok(file) = rotator.current().await {
        metrics::refresh(&file, clock.now_ms()).await;
    }
}

/// Logs and counts a rotation that was done or that failed. A failure logs
/// `warn` only when it begins. A retry that fails again logs `debug`.
fn report(outcome: &Outcome) {
    match outcome {
        Outcome::Off | Outcome::NotDue => {}
        Outcome::Done {
            file,
            new_file,
            counts,
        } => {
            metrics::rotation("done");
            info!(
                tool = NAME,
                open_records = counts.open_records,
                decisions = counts.decisions,
                links = counts.links,
                file = %file,
                new_file = %new_file,
                "work rotation done"
            );
        }
        Outcome::Failed {
            file,
            reason,
            first,
            since_ms,
        } => {
            metrics::rotation("failed");
            let since = readable_utc(*since_ms);
            if *first {
                warn!(
                    tool = NAME,
                    file = %file,
                    reason = reason.as_str(),
                    since = %since,
                    "work rotation failed"
                );
            } else {
                debug!(
                    tool = NAME,
                    file = %file,
                    reason = reason.as_str(),
                    since = %since,
                    "work rotation failed again"
                );
            }
        }
    }
}

/// The UTC date and time of `ms`, to the second, as `2026-11-16T12:00:00Z`.
fn readable_utc(ms: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(ms.div_euclid(1000))
        .ok()
        .and_then(|at| at.format(&Rfc3339).ok())
        .unwrap_or_default()
}

#[cfg(test)]
#[path = "timer_tests.rs"]
mod tests;
