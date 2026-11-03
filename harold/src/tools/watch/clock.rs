//! The watch tool's clock: wall-clock milliseconds, and the local day around them.

use std::future::Future;
use std::sync::{Mutex, PoisonError};

use time::{Date, OffsetDateTime, Time, UtcOffset};

use crate::tools::BoxFuture;

const MS_PER_DAY: i64 = 86_400_000;

#[cfg(not(test))]
pub(super) fn clock_ms() -> i64 {
    (OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
}

/// Under test the wall clock is read once per test thread and tokio's clock moves it, so
/// paused time ages a watch the way real time does.
#[cfg(test)]
pub(super) fn clock_ms() -> i64 {
    thread_local! {
        static BASE: (i64, tokio::time::Instant) = (
            (OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64,
            tokio::time::Instant::now(),
        );
    }
    BASE.with(|(wall, start)| {
        wall + tokio::time::Instant::now()
            .duration_since(*start)
            .as_millis() as i64
    })
}

/// The local zone offset at this moment. It is read at each use, so a change of daylight
/// saving time takes effect without a restart.
pub(super) trait LocalOffset: Send + Sync {
    fn offset(&self) -> BoxFuture<'_, UtcOffset>;
}

type Reader = Box<dyn Fn() -> BoxFuture<'static, Option<UtcOffset>> + Send + Sync>;

/// The production offset: it asks `/bin/date` each time. When `/bin/date` fails, it gives
/// the last good offset.
pub(super) struct DateClock {
    reader: Reader,
    last_good: Mutex<UtcOffset>,
}

impl DateClock {
    pub(super) fn new() -> Self {
        Self::with_reader(crate::timed_prompts::read_local_offset)
    }

    /// A clock that gets its offset from `reader`.
    pub(super) fn with_reader<F, Fut>(reader: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Option<UtcOffset>> + Send + 'static,
    {
        Self {
            reader: Box::new(move || Box::pin(reader())),
            last_good: Mutex::new(crate::telemetry::local_offset()),
        }
    }
}

impl LocalOffset for DateClock {
    fn offset(&self) -> BoxFuture<'_, UtcOffset> {
        Box::pin(async move {
            let read = (self.reader)().await;
            let mut last = self
                .last_good
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if let Some(offset) = read {
                *last = offset;
            }
            *last
        })
    }
}

fn at(ms: i64, offset: UtcOffset) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
        .unwrap_or(OffsetDateTime::UNIX_EPOCH)
        .to_offset(offset)
}

/// The start of the local day holding `now_ms` and the start of the next one.
pub(super) fn day_bounds(now_ms: i64, offset: UtcOffset) -> (i64, i64) {
    let date: Date = at(now_ms, offset).date();
    let start = OffsetDateTime::new_in_offset(date, Time::MIDNIGHT, offset);
    let start_ms = (start.unix_timestamp_nanos() / 1_000_000) as i64;
    (start_ms, start_ms + MS_PER_DAY)
}

/// `YYYY-MM-DD HH:MM:SS` local time.
pub(super) fn local_text(ms: i64, offset: UtcOffset) -> String {
    let moment = at(ms, offset);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        moment.year(),
        u8::from(moment.month()),
        moment.day(),
        moment.hour(),
        moment.minute(),
        moment.second()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_local_day_runs_from_local_midnight_to_the_next() {
        let offset = UtcOffset::from_hms(10, 0, 0).unwrap();
        // 2026-10-20 23:30 at +10:00 is 13:30 UTC.
        let now = OffsetDateTime::new_utc(
            Date::from_calendar_date(2026, time::Month::October, 20).unwrap(),
            Time::from_hms(13, 30, 0).unwrap(),
        )
        .unix_timestamp()
            * 1000;
        let (start, end) = day_bounds(now, offset);
        assert_eq!(end - start, MS_PER_DAY);
        assert_eq!(local_text(start, offset), "2026-10-20 00:00:00");
        assert_eq!(local_text(end, offset), "2026-10-21 00:00:00");
        assert!(start <= now && now < end);
        // One minute later is the next local day.
        assert_eq!(day_bounds(now + 30 * 60_000, offset).0, end);
    }
}

#[cfg(test)]
mod offset_tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[tokio::test]
    async fn an_unreadable_date_keeps_the_last_good_offset() {
        let plus_eleven = UtcOffset::from_hms(11, 0, 0).unwrap();
        let reads = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&reads);
        let clock = DateClock::with_reader(move || {
            let first = counter.fetch_add(1, Ordering::SeqCst) == 0;
            async move { first.then_some(plus_eleven) }
        });

        assert_eq!(clock.offset().await, plus_eleven);
        assert_eq!(clock.offset().await, plus_eleven);
        assert_eq!(clock.offset().await, plus_eleven);
        assert_eq!(reads.load(Ordering::SeqCst), 3);
    }
}
