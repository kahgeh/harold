//! The watch tool's clock: wall-clock milliseconds, and the local day around them.

use std::future::Future;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use time::{Date, OffsetDateTime, Time, UtcOffset};

use crate::tools::BoxFuture;

const MS_PER_DAY: i64 = 86_400_000;

/// A waiting task wakes at least this often. Tokio's clock stops while the computer
/// sleeps, so a task reads the wall clock again at each wake-up and sees the jump. A
/// reminder also reads the local offset again, so a `--at` time follows a change of
/// daylight saving time.
pub(super) const WAKE_EVERY: Duration = Duration::from_secs(60);

#[cfg(not(test))]
pub(super) fn clock_ms() -> i64 {
    (OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
}

#[cfg(test)]
thread_local! {
    /// Milliseconds the test wall clock is ahead of tokio's clock: a computer sleep.
    static WALL_JUMP: std::cell::Cell<i64> = const { std::cell::Cell::new(0) };
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
    }) + WALL_JUMP.with(std::cell::Cell::get)
}

/// Moves the test wall clock forward by `ms` and leaves tokio's clock where it is, as a
/// computer sleep does on macOS.
#[cfg(test)]
pub(super) fn jump_wall_clock(ms: i64) {
    WALL_JUMP.with(|jump| jump.set(jump.get() + ms));
}

/// The local zone offset at this moment. It is read at each use, so a change of daylight
/// saving time takes effect without a restart.
pub(super) trait LocalOffset: Send + Sync {
    fn offset(&self) -> BoxFuture<'_, UtcOffset>;

    /// The start of the local day after the day that holds `day_ms`, in milliseconds. It
    /// is the real instant: on a day with a daylight saving change it is 23 or 25 hours
    /// after the day's start.
    fn midnight_after(&self, day_ms: i64) -> BoxFuture<'_, Result<i64, ClockReason>>;
}

/// Why the clock could not give the next local midnight: the `reason` of the
/// `watch clock unreadable` log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ClockReason {
    /// `/bin/date` did not start.
    Missing,
    /// `/bin/date` ran and failed, or took too long.
    Refused,
    /// `/bin/date` printed text that is not a whole number.
    Unparsable,
}

impl ClockReason {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            ClockReason::Missing => "missing",
            ClockReason::Refused => "refused",
            ClockReason::Unparsable => "unparsable",
        }
    }
}

/// The whole number in the text of `date +%s`, in seconds. `None` for any other text.
pub(super) fn parse_epoch(text: &str) -> Option<i64> {
    text.trim().parse().ok()
}

/// The program that prints a local midnight, and the longest it may take.
const DATE_PROGRAM: &str = "/bin/date";
const DATE_TIMEOUT: Duration = Duration::from_secs(5);

/// The arguments that make `/bin/date` print the instant of `00:00:00` on `date`.
fn midnight_args(date: Date) -> Vec<String> {
    vec![
        "-j".into(),
        "-f".into(),
        "%Y-%m-%d %H:%M:%S".into(),
        format!(
            "{:04}-{:02}-{:02} 00:00:00",
            date.year(),
            u8::from(date.month()),
            date.day()
        ),
        "+%s".into(),
    ]
}

/// What `program` prints. A program that cannot start is `Missing`. A program that fails,
/// runs out of time (then it is killed) or prints text that is not UTF-8 is `Refused`.
async fn bounded_text(
    program: &str,
    args: &[String],
    timeout: Duration,
) -> Result<String, ClockReason> {
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(timeout, command.output())
        .await
        .map_err(|_| ClockReason::Refused)?
        .map_err(|_| ClockReason::Missing)?;
    if !output.status.success() {
        return Err(ClockReason::Refused);
    }
    String::from_utf8(output.stdout).map_err(|_| ClockReason::Refused)
}

async fn run_date(args: Vec<String>) -> Result<String, ClockReason> {
    bounded_text(DATE_PROGRAM, &args, DATE_TIMEOUT).await
}

type Reader = Box<dyn Fn() -> BoxFuture<'static, Option<UtcOffset>> + Send + Sync>;
type EpochRunner =
    Box<dyn Fn(Vec<String>) -> BoxFuture<'static, Result<String, ClockReason>> + Send + Sync>;

/// The production offset: it asks `/bin/date` each time. When `/bin/date` fails, it gives
/// the last good offset.
pub(super) struct DateClock {
    reader: Reader,
    epoch: EpochRunner,
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
            epoch: Box::new(|args| Box::pin(run_date(args))),
            last_good: Mutex::new(crate::telemetry::local_offset()),
        }
    }

    /// The same clock, with `runner` in place of `/bin/date` for the next local midnight.
    /// `runner` gets the arguments of `/bin/date` and gives what it prints.
    #[cfg(test)]
    fn with_date_runner<F, Fut>(mut self, runner: F) -> Self
    where
        F: Fn(Vec<String>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<String, ClockReason>> + Send + 'static,
    {
        self.epoch = Box::new(move |args| Box::pin(runner(args)));
        self
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

    fn midnight_after(&self, day_ms: i64) -> BoxFuture<'_, Result<i64, ClockReason>> {
        Box::pin(async move {
            let offset = self.offset().await;
            let next = at(day_ms, offset)
                .date()
                .next_day()
                .ok_or(ClockReason::Unparsable)?;
            let text = (self.epoch)(midnight_args(next)).await?;
            let seconds = parse_epoch(&text).ok_or(ClockReason::Unparsable)?;
            seconds.checked_mul(1000).ok_or(ClockReason::Unparsable)
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

/// The instant, in milliseconds, when the local clock at `offset` reads `text`
/// (`YYYY-MM-DD HH:MM`). `None` when the text is not such a time.
///
/// The caller passes the offset in force now and asks again at each wake-up, so a
/// change of daylight saving time moves the instant. The rule that follows: a local time
/// in a spring-forward gap (02:30 when 02:00 becomes 03:00) is already past after the
/// change, so it is due at the first instant after the gap. A local time that occurs
/// twice (fall-back) is due at its first occurrence, because the old offset is still in
/// force then.
pub(super) fn local_ms(text: &str, offset: UtcOffset) -> Option<i64> {
    let (date, clock) = text.split_once(' ')?;
    let number = |part: &str, len: usize| {
        (part.len() == len && part.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| part.parse::<i32>().ok())
            .flatten()
    };
    let mut date_parts = date.split('-');
    let (year, month, day) = (
        number(date_parts.next()?, 4)?,
        number(date_parts.next()?, 2)?,
        number(date_parts.next()?, 2)?,
    );
    let (hour, minute) = clock.split_once(':')?;
    let (hour, minute) = (number(hour, 2)?, number(minute, 2)?);
    if date_parts.next().is_some() {
        return None;
    }
    let month = time::Month::try_from(u8::try_from(month).ok()?).ok()?;
    let date = Date::from_calendar_date(year, month, u8::try_from(day).ok()?).ok()?;
    let time = Time::from_hms(u8::try_from(hour).ok()?, u8::try_from(minute).ok()?, 0).ok()?;
    let moment = OffsetDateTime::new_in_offset(date, time, offset);
    Some((moment.unix_timestamp_nanos() / 1_000_000) as i64)
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
        // Two offsets that differ from each other, so no machine offset (the first
        // value) can make every check pass when the reader is not used.
        let plus_ten = UtcOffset::from_hms(10, 0, 0).unwrap();
        let plus_five_thirty = UtcOffset::from_hms(5, 30, 0).unwrap();
        let reads = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&reads);
        let clock = DateClock::with_reader(move || {
            let read = match counter.fetch_add(1, Ordering::SeqCst) {
                0 => Some(plus_ten),
                1 => Some(plus_five_thirty),
                _ => None,
            };
            async move { read }
        });

        assert_eq!(clock.offset().await, plus_ten);
        assert_eq!(clock.offset().await, plus_five_thirty);
        assert_eq!(
            clock.offset().await,
            plus_five_thirty,
            "the last good offset"
        );
        assert_eq!(
            clock.offset().await,
            plus_five_thirty,
            "the last good offset"
        );
        assert_eq!(reads.load(Ordering::SeqCst), 4);
    }
}

#[cfg(test)]
mod local_ms_tests {
    use super::*;

    #[test]
    fn a_local_time_moves_one_hour_later_when_the_offset_drops_an_hour() {
        let plus_eleven = UtcOffset::from_hms(11, 0, 0).unwrap();
        let plus_ten = UtcOffset::from_hms(10, 0, 0).unwrap();
        let at_eleven = local_ms("2026-04-05 02:30", plus_eleven).unwrap();
        let at_ten = local_ms("2026-04-05 02:30", plus_ten).unwrap();
        assert_eq!(at_ten - at_eleven, 3_600_000);
        assert_eq!(local_text(at_eleven, plus_eleven), "2026-04-05 02:30:00");
        assert_eq!(local_text(at_ten, plus_ten), "2026-04-05 02:30:00");
    }

    #[test]
    fn a_garbled_local_time_has_no_instant() {
        let utc = UtcOffset::UTC;
        for text in [
            "",
            "2026-04-05",
            "2026-04-05 2:30",
            "2026-13-05 02:30",
            "2026-02-30 02:30",
            "2026-04-05 24:00",
            "2026-04-05 02:60",
            "x026-04-05 02:30",
        ] {
            assert_eq!(local_ms(text, utc), None, "{text}");
        }
    }
}

#[cfg(test)]
mod midnight_tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[test]
    fn a_whole_number_is_an_epoch_and_other_text_is_not() {
        assert_eq!(parse_epoch("1791036000\n"), Some(1_791_036_000));
        for text in ["", "abc", "12.5", "12 13", "\n"] {
            assert_eq!(parse_epoch(text), None, "{text:?}");
        }
    }

    #[test]
    fn the_date_arguments_name_the_next_date_at_midnight() {
        let date = Date::from_calendar_date(2026, time::Month::October, 5).unwrap();
        assert_eq!(
            midnight_args(date),
            [
                "-j",
                "-f",
                "%Y-%m-%d %H:%M:%S",
                "2026-10-05 00:00:00",
                "+%s"
            ]
        );
    }

    fn clock_with(
        reply: Result<&'static str, ClockReason>,
    ) -> (DateClock, Arc<Mutex<Vec<Vec<String>>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&seen);
        let clock =
            DateClock::with_reader(|| async { Some(UtcOffset::from_hms(10, 0, 0).unwrap()) })
                .with_date_runner(move |args| {
                    record.lock().unwrap().push(args);
                    let reply = reply.map(str::to_owned);
                    async move { reply }
                });
        (clock, seen)
    }

    #[tokio::test]
    async fn the_next_midnight_is_the_printed_instant_in_milliseconds() {
        let (clock, _) = clock_with(Ok("1791036000\n"));
        let day = local_ms("2026-10-20 23:30", UtcOffset::from_hms(10, 0, 0).unwrap()).unwrap();
        assert_eq!(clock.midnight_after(day).await, Ok(1_791_036_000_000));
    }

    #[tokio::test]
    async fn the_date_asked_for_is_the_day_after_the_local_day() {
        let plus_ten = UtcOffset::from_hms(10, 0, 0).unwrap();
        for (local, next) in [
            // 00:00 at +10:00 is 14:00 UTC of the day before: the UTC date is a day behind.
            ("2026-10-20 00:00", "2026-10-21 00:00:00"),
            ("2026-10-20 23:30", "2026-10-21 00:00:00"),
            ("2026-10-31 08:00", "2026-11-01 00:00:00"),
            ("2026-12-31 00:00", "2027-01-01 00:00:00"),
        ] {
            let (clock, seen) = clock_with(Ok("1791036000"));
            let day = local_ms(local, plus_ten).unwrap();
            clock.midnight_after(day).await.unwrap();
            assert_eq!(seen.lock().unwrap()[0][3], next, "{local}");
        }
    }

    #[tokio::test]
    async fn each_failure_of_the_date_read_has_its_reason() {
        for (reply, reason) in [
            (Err(ClockReason::Missing), ClockReason::Missing),
            (Err(ClockReason::Refused), ClockReason::Refused),
            (Ok("abc"), ClockReason::Unparsable),
            (Ok(""), ClockReason::Unparsable),
        ] {
            let (clock, _) = clock_with(reply);
            assert_eq!(clock.midnight_after(0).await, Err(reason));
        }
    }

    #[tokio::test]
    async fn a_command_that_does_not_start_fails_or_runs_out_of_time_has_its_reason() {
        let second = Duration::from_secs(1);
        assert_eq!(
            bounded_text("/bin/echo", &["safe".into()], second).await,
            Ok("safe\n".into())
        );
        assert_eq!(
            bounded_text("/nonexistent/harold-test-command", &[], second).await,
            Err(ClockReason::Missing)
        );
        assert_eq!(
            bounded_text("/usr/bin/false", &[], second).await,
            Err(ClockReason::Refused)
        );
        assert_eq!(
            bounded_text("/bin/sleep", &["5".into()], Duration::from_millis(50)).await,
            Err(ClockReason::Refused)
        );
    }
}
