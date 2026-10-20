//! The watch tool's clock: wall-clock milliseconds, and the local day around them.

use time::{Date, OffsetDateTime, Time, UtcOffset};

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
