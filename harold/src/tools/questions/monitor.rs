//! Watches the checked line's rule once a minute, so the chief hears of a lapse without
//! anyone looking at the page.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tracing::warn;

use super::NAME;
use super::batch::clock_ms;
use super::store::QuestionStore;

/// How often the rule is tested.
const EVERY: Duration = Duration::from_secs(60);
/// The checked line turns red, and the rule holds, past this long after the last check.
pub(super) const STALE_AFTER_MS: i64 = 2 * 60 * 60 * 1000;

/// Whether the checked line is red: a todo waits, and the last check was over 2 hours ago
/// or there was none.
pub(super) fn is_stale(now_ms: i64, checked_at_ms: i64, waiting: usize) -> bool {
    waiting > 0 && (checked_at_ms == 0 || now_ms.saturating_sub(checked_at_ms) > STALE_AFTER_MS)
}

/// Logs `todos not checked` once when the rule first holds, and again only after a check or
/// an empty list has cleared it.
#[derive(Default)]
pub(super) struct Lapse {
    logged: bool,
}

impl Lapse {
    pub(super) fn test(&mut self, now_ms: i64, checked_at_ms: i64, waiting: usize) {
        if !is_stale(now_ms, checked_at_ms, waiting) {
            self.logged = false;
            return;
        }
        if std::mem::replace(&mut self.logged, true) {
            return;
        }
        let never = checked_at_ms == 0;
        let minutes = if never {
            0
        } else {
            (now_ms - checked_at_ms) / 60_000
        };
        warn!(tool = NAME, waiting, minutes, never, "todos not checked");
    }
}

pub(super) async fn run<S: QuestionStore>(store: Arc<S>, mut shutdown: watch::Receiver<()>) {
    let mut lapse = Lapse::default();
    let mut every = tokio::time::interval(EVERY);
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => return,
            _ = every.tick() => {}
        }
        let now = clock_ms();
        // A failed read is the service's to report; the rule is tested again next minute.
        if let Ok(listing) = store.list(now).await {
            lapse.test(now, listing.checked_at_ms, listing.questions.len());
        }
    }
}

#[cfg(test)]
#[path = "monitor_tests.rs"]
mod tests;
