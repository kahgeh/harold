use std::os::unix::fs::PermissionsExt as _;

use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
use time::UtcOffset;

use super::*;
use crate::outbound::fake::CapturedLogs;
use crate::telemetry::test_support;

// --- helpers ---

fn offset(hours: i8) -> UtcOffset {
    UtcOffset::from_hms(hours, 0, 0).unwrap()
}

/// The local time `year-month-day hour:minute` at the UTC offset of `hours`.
fn at((year, month, day): (i32, u8, u8), (hour, minute): (u8, u8), hours: i8) -> OffsetDateTime {
    Date::from_calendar_date(year, Month::try_from(month).unwrap(), day)
        .unwrap()
        .with_hms(hour, minute, 0)
        .unwrap()
        .assume_offset(offset(hours))
}

/// A reading with the given memory share, swap bytes and CPU share. The disk is half full.
fn reading(memory: f64, swap: u64, cpu: f64) -> Reading {
    Reading {
        disk_used: 500,
        disk_free: 500,
        memory_used: (memory * 1000.0).round() as u64,
        memory_free: 1000 - (memory * 1000.0).round() as u64,
        swap_used: swap,
        cpu,
        load_1m: 1.5,
        process_memory: 42,
        process_cpu: 0.01,
    }
}

fn calm() -> Reading {
    reading(0.4, 0, 0.2)
}

type Current = Arc<Mutex<Result<Reading, &'static str>>>;

struct FakeReader {
    current: Current,
    asked: Arc<Mutex<Vec<Metric>>>,
}

impl Reader for FakeReader {
    fn read(&mut self) -> Result<Reading, &'static str> {
        *lock(&self.current)
    }

    fn top_processes(&mut self, metric: Metric) -> String {
        lock(&self.asked).push(metric);
        "chrome 31%, rustc 12%, node 4%".into()
    }
}

/// A folder removed when dropped.
struct Folder(PathBuf);

impl Folder {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("harold-machine-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn file(&self) -> PathBuf {
        self.0.join("data").join("machine-hourly-use.json")
    }
}

impl Drop for Folder {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Rig {
    stats: MachineStats<FakeReader>,
    current: Current,
    asked: Arc<Mutex<Vec<Metric>>>,
    snapshot: Arc<Mutex<Snapshot>>,
    folder: Folder,
}

impl Rig {
    fn new(now: OffsetDateTime) -> Self {
        Self::with_snapshot(now, Arc::new(Mutex::new(Snapshot::default())))
    }

    /// Harold stops and starts again at `now`, with the same file.
    fn restart(&mut self, now: OffsetDateTime) {
        let reader = FakeReader {
            current: Arc::clone(&self.current),
            asked: Arc::clone(&self.asked),
        };
        self.stats = MachineStats::new(
            reader,
            self.folder.file(),
            Arc::new(Mutex::new(Snapshot::default())),
            now,
        );
    }

    fn with_snapshot(now: OffsetDateTime, snapshot: Arc<Mutex<Snapshot>>) -> Self {
        let folder = Folder::new();
        let current: Current = Arc::new(Mutex::new(Ok(calm())));
        let asked = Arc::new(Mutex::new(Vec::new()));
        let reader = FakeReader {
            current: Arc::clone(&current),
            asked: Arc::clone(&asked),
        };
        let stats = MachineStats::new(reader, folder.file(), Arc::clone(&snapshot), now);
        Self {
            stats,
            current,
            asked,
            snapshot,
            folder,
        }
    }

    /// Reads once, at `now`, with `result` as the machine's answer.
    async fn poll(&mut self, now: OffsetDateTime, result: Result<Reading, &'static str>) {
        *lock(&self.current) = result;
        self.stats.poll(now).await;
    }

    fn past(&self) -> Option<[bool; 3]> {
        lock(&self.snapshot).past_normal
    }
}

/// The same value at the hour of `now` on each of the 7 days before, except `missing` days.
fn flat_history(hourly: &mut HourlyUse, now: OffsetDateTime, means: [f64; 3], missing: &[i64]) {
    for day in 1..=7 {
        if missing.contains(&day) {
            continue;
        }
        hourly.add(HourMean {
            hour: hour_number(now) - 24 * day,
            minutes: 60,
            means,
        });
    }
}

// --- the normal range ---

#[test]
fn the_top_is_the_median_plus_three_times_the_median_absolute_deviation() {
    let now = at((2026, 10, 25), (10, 5), 11);
    let mut hourly = HourlyUse::default();
    // Median 0.4. The deviations are 0.2, 0, 0.2, 0, 0.1, 0.1, 0, so their median is 0.1.
    for (day, value) in [0.2, 0.4, 0.6, 0.4, 0.5, 0.3, 0.4].into_iter().enumerate() {
        hourly.add(HourMean {
            hour: hour_number(now) - 24 * (day as i64 + 1),
            minutes: 60,
            means: [value, value * 1e10, value],
        });
    }
    for (metric, median, deviation) in [
        (Metric::Memory, 0.4, 0.1),
        (Metric::Cpu, 0.4, 0.1),
        (Metric::Swap, 4e9, 1e9),
    ] {
        let range = hourly.normal_range(metric, hour_number(now)).unwrap();
        let expected = median + 3.0 * deviation;
        assert!(
            (range.top - expected).abs() < 1e-6 * expected.max(1.0),
            "{metric:?}: {} is not {expected}",
            range.top
        );
        assert_eq!(range.days, 7);
    }
}

#[test]
fn a_flat_history_gets_the_floor_of_each_metric() {
    let now = at((2026, 10, 25), (10, 5), 11);
    let mut hourly = HourlyUse::default();
    flat_history(&mut hourly, now, [0.5, 2e9, 0.3], &[]);
    let top = |metric| hourly.normal_range(metric, hour_number(now)).unwrap().top;
    assert!((top(Metric::Memory) - 0.6).abs() < 1e-9, "10 points");
    assert!((top(Metric::Cpu) - 0.4).abs() < 1e-9, "10 points");
    assert!((top(Metric::Swap) - 3e9).abs() < 1.0, "1 GB");
}

#[test]
fn a_small_spread_is_lifted_to_the_floor_and_a_wide_one_is_not() {
    let now = at((2026, 10, 25), (10, 5), 11);
    let mut hourly = HourlyUse::default();
    for (day, value) in [0.40, 0.42, 0.38, 0.41, 0.40, 0.39, 0.43]
        .into_iter()
        .enumerate()
    {
        hourly.add(HourMean {
            hour: hour_number(now) - 24 * (day as i64 + 1),
            minutes: 60,
            means: [value, 0.0, value],
        });
    }
    // Median 0.40, median absolute deviation 0.01: the 3 deviations give 0.43, under the floor.
    let range = hourly
        .normal_range(Metric::Memory, hour_number(now))
        .unwrap();
    assert!((range.top - 0.50).abs() < 1e-9, "{}", range.top);
}

#[test]
fn four_days_of_seven_judge_nothing_and_five_judge() {
    let now = at((2026, 10, 25), (10, 5), 11);
    let mut hourly = HourlyUse::default();
    flat_history(&mut hourly, now, [0.4, 0.0, 0.2], &[5, 6, 7]);
    for metric in Metric::ALL {
        assert_eq!(hourly.normal_range(metric, hour_number(now)), None);
    }
    let mut hourly = HourlyUse::default();
    flat_history(&mut hourly, now, [0.4, 0.0, 0.2], &[6, 7]);
    for metric in Metric::ALL {
        assert_eq!(
            hourly.normal_range(metric, hour_number(now)).unwrap().days,
            5
        );
    }
}

#[test]
fn only_the_same_hour_of_the_last_seven_days_counts() {
    let now = at((2026, 10, 25), (10, 5), 11);
    let mut hourly = HourlyUse::default();
    flat_history(&mut hourly, now, [0.4, 0.0, 0.2], &[]);
    // The day 8 days back, the next hour and today's hour are not part of the range.
    for hour in [
        hour_number(now) - 24 * 8,
        hour_number(now) + 1,
        hour_number(now),
    ] {
        hourly.add(HourMean {
            hour,
            minutes: 60,
            means: [0.99, 0.0, 0.99],
        });
    }
    let range = hourly
        .normal_range(Metric::Memory, hour_number(now))
        .unwrap();
    assert_eq!(range.days, 7);
    assert!((range.top - 0.5).abs() < 1e-9, "{}", range.top);
}

// --- the local hour on the clock-change days ---

#[test]
fn the_skipped_hour_has_no_mean_and_is_still_judged_with_six_days() {
    // New York, 8 March 2026: 01:59 -05:00 is followed by 03:00 -04:00.
    let mut hourly = HourlyUse::default();
    let skip_day = at((2026, 3, 8), (2, 30), -5);
    let next_day = at((2026, 3, 9), (2, 10), -4);
    // The hour 2 of the 6 days before the skip day, and none for the skip day itself.
    for back in 1..=6 {
        hourly.add(HourMean {
            hour: hour_number(skip_day) - 24 * back,
            minutes: 60,
            means: [0.4, 0.0, 0.2],
        });
    }
    let range = hourly
        .normal_range(Metric::Cpu, hour_number(next_day))
        .unwrap();
    assert_eq!(range.days, 6);
}

#[tokio::test]
async fn nothing_is_written_for_the_skipped_hour() {
    let mut rig = Rig::new(at((2026, 3, 8), (1, 0), -5));
    rig.poll(at((2026, 3, 8), (1, 30), -5), Ok(calm())).await;
    rig.poll(at((2026, 3, 8), (3, 30), -4), Ok(calm())).await;
    rig.poll(at((2026, 3, 8), (4, 5), -4), Ok(calm())).await;
    let hours: Vec<i64> = rig.stats.hourly().hours.keys().copied().collect();
    let day = hour_number(at((2026, 3, 8), (0, 0), -5));
    assert_eq!(hours, [day + 1, day + 3], "hours 1 and 3, not 2");
}

#[tokio::test]
async fn the_hour_that_occurs_twice_gets_one_mean_of_both() {
    // New York, 1 November 2026: 01:xx -04:00 and then 01:xx -05:00.
    let mut rig = Rig::new(at((2026, 11, 1), (0, 30), -4));
    rig.poll(at((2026, 11, 1), (1, 10), -4), Ok(reading(0.4, 0, 0.2)))
        .await;
    rig.poll(at((2026, 11, 1), (1, 50), -4), Ok(reading(0.4, 0, 0.2)))
        .await;
    rig.poll(at((2026, 11, 1), (1, 10), -5), Ok(reading(0.4, 0, 0.8)))
        .await;
    assert!(
        rig.stats.hourly().hours.is_empty(),
        "the hour is still going"
    );
    rig.poll(at((2026, 11, 1), (2, 10), -5), Ok(calm())).await;
    let hour = hour_number(at((2026, 11, 1), (1, 0), -5));
    let hours = &rig.stats.hourly().hours;
    assert_eq!(hours.len(), 1);
    let mean = hours[&hour];
    assert_eq!(mean.minutes, 3);
    assert!((mean.means[2] - 0.4).abs() < 1e-9, "{:?}", mean.means);
}

#[test]
fn a_second_mean_of_the_same_hour_is_weighted_by_its_readings() {
    let mut hourly = HourlyUse::default();
    let hour = 1_000_000;
    hourly.add(HourMean {
        hour,
        minutes: 30,
        means: [0.2, 0.0, 0.2],
    });
    hourly.add(HourMean {
        hour,
        minutes: 10,
        means: [0.6, 0.0, 0.6],
    });
    let mean = hourly.hours[&hour];
    assert_eq!(mean.minutes, 40);
    assert!((mean.means[0] - 0.3).abs() < 1e-9);
}

// --- past_normal ---

/// A rig at 10:05 with a flat CPU history of 0.2, whose top is 0.3, and a calm machine.
fn judged_rig() -> (Rig, OffsetDateTime) {
    let start = at((2026, 10, 25), (10, 0), 11);
    let mut rig = Rig::new(start);
    flat_history(&mut rig.stats.hourly, start, [0.4, 0.0, 0.2], &[]);
    (rig, start)
}

fn minutes(start: OffsetDateTime, minutes: i64) -> OffsetDateTime {
    start + time::Duration::minutes(minutes)
}

#[tokio::test]
async fn past_normal_changes_to_1_only_past_the_top_and_logs_once() {
    let logs = CapturedLogs::start();
    let (mut rig, start) = judged_rig();
    for minute in 1..=3 {
        rig.poll(minutes(start, minute), Ok(calm())).await;
        assert_eq!(rig.past(), Some([false, false, false]));
    }
    // The mean of the 4 readings is 0.375, over the top 0.3.
    rig.poll(minutes(start, 4), Ok(reading(0.4, 0, 0.9))).await;
    assert_eq!(rig.past(), Some([false, false, true]));
    // Still past: no second line.
    rig.poll(minutes(start, 5), Ok(reading(0.4, 0, 0.9))).await;
    assert_eq!(rig.past(), Some([false, false, true]));

    let text = logs.text();
    assert_eq!(text.matches("machine use past normal").count(), 1, "{text}");
    let line = text
        .lines()
        .find(|line| line.contains("machine use past normal"))
        .unwrap();
    assert!(line.trim_start().starts_with("WARN"), "{line}");
    for piece in [
        "metric=\"cpu\"",
        "days=7",
        "top_processes=chrome 31%, rustc 12%, node 4%",
        "normal_top=0.3",
        "value=0.375",
    ] {
        assert!(line.contains(piece), "{piece} in {line}");
    }
    assert_eq!(
        *lock(&rig.asked),
        [Metric::Cpu],
        "the processes of cpu only"
    );
}

#[tokio::test]
async fn past_normal_goes_back_to_0_once_the_fifteen_minute_mean_is_at_or_under_the_top() {
    let logs = CapturedLogs::start();
    let (mut rig, start) = judged_rig();
    rig.poll(minutes(start, 1), Ok(reading(0.4, 0, 0.9))).await;
    assert_eq!(rig.past(), Some([false, false, true]));
    // One calm reading does not bring the mean down: it is (0.9 + 0.2) / 2.
    rig.poll(minutes(start, 2), Ok(calm())).await;
    assert_eq!(rig.past(), Some([false, false, true]));
    let mut back_at = None;
    for minute in 3..=20 {
        rig.poll(minutes(start, minute), Ok(calm())).await;
        if rig.past() == Some([false, false, false]) {
            back_at = Some(minute);
            break;
        }
    }
    // (0.9 + 0.2 * (k - 1)) / k is at or under 0.3 from k = 7 readings.
    assert_eq!(back_at, Some(7));
    rig.poll(minutes(start, 21), Ok(calm())).await;

    let text = logs.text();
    assert_eq!(text.matches("machine use past normal").count(), 1, "{text}");
    assert_eq!(
        text.matches("machine use back to normal").count(),
        1,
        "{text}"
    );
    let line = text
        .lines()
        .find(|line| line.contains("machine use back to normal"))
        .unwrap();
    assert!(line.trim_start().starts_with("INFO"), "{line}");
    assert!(line.contains("metric=\"cpu\""), "{line}");
    assert!(line.contains("minutes=6"), "{line}");
}

#[tokio::test]
async fn a_mean_exactly_at_the_top_is_not_past_it() {
    let (mut rig, start) = judged_rig();
    // The top is 0.2 + 0.10 floor = 0.3, up to rounding.
    let top = lock(&rig.snapshot).normal_top;
    assert_eq!(top, [None; 3], "nothing is judged before the first reading");
    rig.poll(start, Ok(reading(0.4, 0, 0.2))).await;
    let top = lock(&rig.snapshot).normal_top[Metric::Cpu.index()].unwrap();
    rig.poll(minutes(start, 1), Ok(reading(0.4, 0, top))).await;
    rig.poll(minutes(start, 2), Ok(reading(0.4, 0, 2.0 * top - 0.2)))
        .await;
    assert_eq!(rig.past(), Some([false, false, false]));
}

#[tokio::test]
async fn an_hour_with_fewer_than_five_days_sends_no_top_and_ends_the_spell() {
    let logs = CapturedLogs::start();
    let (mut rig, start) = judged_rig();
    rig.poll(minutes(start, 1), Ok(reading(0.4, 0, 0.9))).await;
    assert_eq!(rig.past(), Some([false, false, true]));
    // Three of the seven days go: the hour can no longer be judged.
    for day in 1..=3 {
        rig.stats
            .hourly
            .hours
            .remove(&(hour_number(start) - 24 * day));
    }
    rig.poll(minutes(start, 2), Ok(reading(0.4, 0, 0.9))).await;
    assert_eq!(rig.past(), Some([false, false, false]));
    assert_eq!(lock(&rig.snapshot).normal_top, [None; 3]);
    let text = logs.text();
    assert_eq!(
        text.matches("machine use back to normal").count(),
        1,
        "{text}"
    );
}

#[tokio::test]
async fn a_failed_reading_leaves_past_normal_as_it_was() {
    let logs = CapturedLogs::start();
    let (mut rig, start) = judged_rig();
    rig.poll(minutes(start, 1), Ok(reading(0.4, 0, 0.9))).await;
    assert_eq!(rig.past(), Some([false, false, true]));
    for minute in 2..=4 {
        rig.poll(minutes(start, minute), Err("no_home_disk")).await;
        assert_eq!(rig.past(), Some([false, false, true]));
        assert_eq!(lock(&rig.snapshot).reading, None, "no old reading is sent");
    }
    let text = logs.text();
    assert_eq!(
        text.matches("machine use back to normal").count(),
        0,
        "{text}"
    );
    // The failure is logged once until a reading works again.
    assert_eq!(text.matches("machine stats failed").count(), 1, "{text}");
    let line = text
        .lines()
        .find(|line| line.contains("machine stats failed"))
        .unwrap();
    assert!(line.trim_start().starts_with("WARN"), "{line}");
    assert!(line.contains("reason=\"no_home_disk\""), "{line}");
}

#[tokio::test]
async fn memory_and_swap_are_judged_in_their_own_units() {
    let start = at((2026, 10, 25), (10, 0), 11);
    let mut rig = Rig::new(start);
    flat_history(&mut rig.stats.hourly, start, [0.4, 1e9, 0.2], &[]);
    // The tops are 0.5 of memory and 2 GB of swap.
    rig.poll(start, Ok(reading(0.45, 1_900_000_000, 0.2))).await;
    assert_eq!(rig.past(), Some([false, false, false]));
    rig.poll(minutes(start, 1), Ok(reading(0.7, 4_000_000_000, 0.2)))
        .await;
    assert_eq!(rig.past(), Some([true, true, false]));
    assert_eq!(*lock(&rig.asked), [Metric::Memory, Metric::Swap]);
}

#[tokio::test]
async fn disk_use_has_no_normal_range_and_no_past_normal() {
    let (mut rig, start) = judged_rig();
    // A full disk, and nothing else out of the ordinary.
    let mut full = calm();
    full.disk_used = 990;
    full.disk_free = 10;
    for minute in 0..5 {
        rig.poll(minutes(start, minute), Ok(full)).await;
    }
    assert_eq!(rig.past(), Some([false, false, false]));
    assert!(Metric::ALL.iter().all(|metric| metric.as_str() != "disk"));
    assert!(lock(&rig.asked).is_empty());
}

// --- the hourly use file ---

#[tokio::test]
async fn the_file_is_written_once_at_the_end_of_each_local_hour_and_keeps_a_part_hour() {
    let mut rig = Rig::new(at((2026, 10, 25), (10, 40), 11));
    let file = rig.folder.file();
    // Harold started at 10:40 and read only 20 minutes of the hour.
    for minute in 41..=59 {
        rig.poll(at((2026, 10, 25), (10, minute), 11), Ok(calm()))
            .await;
    }
    assert!(!file.exists(), "nothing is written inside the hour");
    rig.poll(at((2026, 10, 25), (11, 0), 11), Ok(reading(0.4, 0, 0.5)))
        .await;
    let written = std::fs::read_to_string(&file).unwrap();
    let content: serde_json::Value = serde_json::from_str(&written).unwrap();
    let hours = content["hours"].as_array().unwrap();
    assert_eq!(hours.len(), 1, "{written}");
    assert_eq!(hours[0]["date"], "2026-10-25");
    assert_eq!(hours[0]["hour"], 10);
    assert_eq!(hours[0]["minutes"], 19, "the part hour keeps its mean");
    assert!((hours[0]["cpu"].as_f64().unwrap() - 0.2).abs() < 1e-9);

    // The hour of 11:00 is written when it ends, not before.
    let before = std::fs::metadata(&file).unwrap().modified().unwrap();
    rig.poll(at((2026, 10, 25), (11, 30), 11), Ok(calm())).await;
    assert_eq!(
        std::fs::metadata(&file).unwrap().modified().unwrap(),
        before
    );
    rig.poll(at((2026, 10, 25), (12, 0), 11), Ok(calm())).await;
    let content: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(content["hours"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn the_file_is_mode_0600_and_replaces_the_old_one_through_a_rename() {
    let mut rig = Rig::new(at((2026, 10, 25), (10, 0), 11));
    rig.poll(at((2026, 10, 25), (10, 30), 11), Ok(calm())).await;
    rig.poll(at((2026, 10, 25), (11, 5), 11), Ok(calm())).await;
    let file = rig.folder.file();
    let mode = std::fs::metadata(&file).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    assert!(
        !file.with_extension("tmp").exists(),
        "the temporary file is renamed"
    );
}

#[tokio::test]
async fn shutdown_writes_the_part_hour_and_a_start_in_the_same_hour_merges_with_it() {
    let mut rig = Rig::new(at((2026, 10, 25), (10, 0), 11));
    let file = rig.folder.file();
    for minute in 1..=30 {
        rig.poll(
            at((2026, 10, 25), (10, minute), 11),
            Ok(reading(0.4, 0, 0.2)),
        )
        .await;
    }
    rig.stats.finish(at((2026, 10, 25), (10, 31), 11));
    let content: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    let hours = content["hours"].as_array().unwrap();
    assert_eq!(hours.len(), 1);
    assert_eq!(hours[0]["hour"], 10);
    assert_eq!(hours[0]["minutes"], 30);

    // Harold starts again at 10:40 and reads 10 minutes of cpu 0.5 before the hour ends.
    rig.restart(at((2026, 10, 25), (10, 40), 11));
    let again = &mut rig;
    for minute in 41..=50 {
        again
            .poll(
                at((2026, 10, 25), (10, minute), 11),
                Ok(reading(0.4, 0, 0.5)),
            )
            .await;
    }
    again
        .poll(at((2026, 10, 25), (11, 0), 11), Ok(calm()))
        .await;
    let content: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    let hours = content["hours"].as_array().unwrap();
    assert_eq!(hours[0]["minutes"], 40);
    assert!((hours[0]["cpu"].as_f64().unwrap() - 0.275).abs() < 1e-9);
}

#[test]
fn the_offset_text_of_date_gives_the_utc_offset() {
    let offset = |text| parse_offset(text).map(|offset| offset.whole_seconds());
    assert_eq!(offset("+1100\n"), Some(11 * 3600));
    assert_eq!(offset("-0330"), Some(-(3 * 3600 + 30 * 60)));
    assert_eq!(offset("+0000"), Some(0));
    for bad in ["", "1100", "+11", "+11:00", "+ab00", "+9900"] {
        assert_eq!(offset(bad), None, "{bad}");
    }
}

#[tokio::test]
async fn a_failed_reading_adds_nothing_to_the_hour_and_an_hour_without_readings_has_no_mean() {
    let mut rig = Rig::new(at((2026, 10, 25), (10, 0), 11));
    rig.poll(at((2026, 10, 25), (10, 30), 11), Err("no_home_disk"))
        .await;
    rig.poll(at((2026, 10, 25), (11, 30), 11), Ok(calm())).await;
    assert!(rig.stats.hourly().hours.is_empty());
    rig.poll(at((2026, 10, 25), (12, 5), 11), Ok(calm())).await;
    assert_eq!(rig.stats.hourly().len(), 1, "only the hour with a reading");
}

#[test]
fn means_older_than_336_hours_are_dropped_and_the_336th_is_kept() {
    let now = hour_number(at((2026, 10, 25), (10, 0), 11));
    let mut hourly = HourlyUse::default();
    for age in [335, 336, 337, 400] {
        hourly.add(HourMean {
            hour: now - age,
            minutes: 60,
            means: [0.1, 0.0, 0.1],
        });
    }
    hourly.drop_old(now);
    let kept: Vec<i64> = hourly.hours.keys().map(|hour| now - hour).collect();
    assert_eq!(kept, [336, 335]);
}

#[tokio::test]
async fn old_means_are_dropped_when_the_hour_is_written_and_when_harold_starts() {
    let now = at((2026, 10, 25), (10, 0), 11);
    let mut rig = Rig::new(now);
    rig.stats.hourly.add(HourMean {
        hour: hour_number(now) - 337,
        minutes: 60,
        means: [0.1, 0.0, 0.1],
    });
    rig.stats.hourly.add(HourMean {
        hour: hour_number(now) - 100,
        minutes: 60,
        means: [0.1, 0.0, 0.1],
    });
    rig.poll(at((2026, 10, 25), (10, 30), 11), Ok(calm())).await;
    rig.poll(at((2026, 10, 25), (11, 0), 11), Ok(calm())).await;
    assert!(
        !rig.stats
            .hourly()
            .hours
            .contains_key(&(hour_number(now) - 337))
    );
    assert!(
        rig.stats
            .hourly()
            .hours
            .contains_key(&(hour_number(now) - 100))
    );

    // A start two weeks and a day later reads the file and drops what is old by then.
    let later = at((2026, 11, 8), (11, 0), 11);
    let reloaded = HourlyUse::load(&rig.folder.file(), hour_number(later));
    assert_eq!(reloaded.len(), 0);
    let soon = at((2026, 10, 26), (11, 0), 11);
    let reloaded = HourlyUse::load(&rig.folder.file(), hour_number(soon));
    assert_eq!(reloaded.len(), 2);
    assert_eq!(&reloaded, rig.stats.hourly());
}

#[test]
fn a_missing_file_starts_empty_without_a_warning() {
    let logs = CapturedLogs::start();
    let folder = Folder::new();
    let hourly = HourlyUse::load(&folder.file(), 0);
    assert_eq!(hourly.len(), 0);
    assert!(!logs.text().contains("machine stats failed"));
}

#[test]
fn a_file_that_cannot_be_read_starts_empty_with_the_warn_line() {
    for content in [
        "not json".to_owned(),
        r#"{"hours":[{"date":"2026-13-45","hour":3,"minutes":1,"memory":0.1,"swap":0,"cpu":0.1}]}"#
            .to_owned(),
        r#"{"hours":[{"date":"2026-10-25","hour":30,"minutes":1,"memory":0.1,"swap":0,"cpu":0.1}]}"#
            .to_owned(),
    ] {
        let logs = CapturedLogs::start();
        let folder = Folder::new();
        std::fs::create_dir_all(folder.file().parent().unwrap()).unwrap();
        std::fs::write(folder.file(), &content).unwrap();
        let hourly = HourlyUse::load(&folder.file(), 0);
        assert_eq!(hourly.len(), 0, "{content}");
        let text = logs.text();
        let line = text
            .lines()
            .find(|line| line.contains("machine stats failed"))
            .unwrap_or_else(|| panic!("no warning for {content}: {text}"));
        assert!(line.trim_start().starts_with("WARN"), "{line}");
        assert!(line.contains("reason=\"hourly_use_unreadable\""), "{line}");
    }
}

#[tokio::test]
async fn a_file_that_cannot_be_written_logs_and_the_means_stay_in_memory() {
    let logs = CapturedLogs::start();
    let folder = Folder::new();
    // The data folder is a file, so the hourly use file cannot be made in it.
    std::fs::write(folder.0.join("data"), "x").unwrap();
    let now = at((2026, 10, 25), (10, 0), 11);
    let current: Current = Arc::new(Mutex::new(Ok(calm())));
    let reader = FakeReader {
        current,
        asked: Arc::default(),
    };
    let mut stats = MachineStats::new(
        reader,
        folder.file(),
        Arc::new(Mutex::new(Snapshot::default())),
        now,
    );
    stats.poll(at((2026, 10, 25), (10, 30), 11)).await;
    stats.poll(at((2026, 10, 25), (11, 5), 11)).await;
    assert_eq!(stats.hourly().len(), 1);
    let text = logs.text();
    assert!(text.contains("reason=\"hourly_use_unwritable\""), "{text}");
}

// --- the gauges ---

fn f64_gauge_points(name: &str) -> Vec<(f64, BTreeMap<String, String>)> {
    for resource in test_support::metrics().iter().rev() {
        for scope in resource.scope_metrics() {
            for metric in scope.metrics().filter(|metric| metric.name() == name) {
                let AggregatedMetrics::F64(MetricData::Gauge(gauge)) = metric.data() else {
                    continue;
                };
                return gauge
                    .data_points()
                    .map(|point| {
                        let attributes = point
                            .attributes()
                            .map(|kv| (kv.key.to_string(), kv.value.as_str().into_owned()))
                            .collect();
                        (point.value(), attributes)
                    })
                    .collect();
            }
        }
    }
    Vec::new()
}

fn u64_gauge_points(name: &str) -> Vec<(u64, BTreeMap<String, String>)> {
    for resource in test_support::metrics().iter().rev() {
        for scope in resource.scope_metrics() {
            for metric in scope.metrics().filter(|metric| metric.name() == name) {
                let AggregatedMetrics::U64(MetricData::Gauge(gauge)) = metric.data() else {
                    continue;
                };
                return gauge
                    .data_points()
                    .map(|point| {
                        let attributes = point
                            .attributes()
                            .map(|kv| (kv.key.to_string(), kv.value.as_str().into_owned()))
                            .collect();
                        (point.value(), attributes)
                    })
                    .collect();
            }
        }
    }
    Vec::new()
}

fn unit_of(name: &str) -> Option<String> {
    test_support::metrics().iter().rev().find_map(|resource| {
        resource
            .scope_metrics()
            .flat_map(|scope| scope.metrics())
            .find(|metric| metric.name() == name)
            .map(|metric| metric.unit().to_owned())
    })
}

fn attributes(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

/// The one test that uses the process-wide gauges, so no other test changes their snapshot.
#[tokio::test]
async fn each_gauge_has_its_name_unit_and_attributes_and_no_path_leaves() {
    test_support::install();
    let start = at((2026, 10, 25), (10, 0), 11);
    let mut rig = Rig::with_snapshot(start, gauge_snapshot());
    flat_history(&mut rig.stats.hourly, start, [0.4, 1e9, 0.2], &[]);
    let mut read = reading(0.6, 3_000_000_000, 0.9);
    read.disk_used = 700;
    read.disk_free = 300;
    rig.poll(start, Ok(read)).await;

    let used = attributes(&[("state", "used")]);
    let free = attributes(&[("state", "free")]);
    let home_used = attributes(&[("state", "used"), ("volume", "home")]);
    let home_free = attributes(&[("state", "free"), ("volume", "home")]);
    let home = attributes(&[("volume", "home")]);

    let mut usage = u64_gauge_points("system.filesystem.usage");
    usage.sort();
    assert_eq!(usage.len(), 2);
    assert!(usage.contains(&(700, home_used)));
    assert!(usage.contains(&(300, home_free)));
    let disk = f64_gauge_points("system.filesystem.utilization");
    assert_eq!(disk.len(), 1);
    assert!((disk[0].0 - 0.7).abs() < 1e-9);
    assert_eq!(disk[0].1, home);

    let memory = u64_gauge_points("system.memory.usage");
    assert!(memory.contains(&(600, used)));
    assert!(memory.contains(&(400, free)));
    let utilization = f64_gauge_points("system.memory.utilization");
    assert!((utilization[0].0 - 0.6).abs() < 1e-9);
    assert!(utilization[0].1.is_empty());
    assert_eq!(
        u64_gauge_points("system.paging.usage"),
        [(3_000_000_000, BTreeMap::new())]
    );
    assert_eq!(f64_gauge_points("system.cpu.utilization")[0].0, 0.9);
    assert_eq!(f64_gauge_points("system.cpu.load_average.1m")[0].0, 1.5);
    assert_eq!(u64_gauge_points("process.memory.usage")[0].0, 42);
    assert_eq!(f64_gauge_points("process.cpu.utilization")[0].0, 0.01);

    // Memory, swap and cpu are past their tops (0.5, 2 GB and 0.3).
    let past = u64_gauge_points("harold.machine.past_normal");
    let by_metric: BTreeMap<String, u64> = past
        .iter()
        .map(|(value, attributes)| (attributes["metric"].clone(), *value))
        .collect();
    assert_eq!(
        by_metric,
        BTreeMap::from([
            ("cpu".to_owned(), 1),
            ("memory".to_owned(), 1),
            ("swap".to_owned(), 1)
        ]),
        "past_normal is sent for memory, swap and cpu, never for disk"
    );
    let tops = f64_gauge_points("harold.machine.normal_top");
    let top_of = |metric: &str| {
        tops.iter()
            .find(|(_, attributes)| attributes["metric"] == metric)
            .map(|(value, _)| *value)
            .unwrap()
    };
    assert!((top_of("memory") - 0.5).abs() < 1e-9);
    assert!((top_of("swap") - 2e9).abs() < 1.0);
    assert!((top_of("cpu") - 0.3).abs() < 1e-9);

    for (name, unit) in [
        ("system.filesystem.usage", "By"),
        ("system.filesystem.utilization", "1"),
        ("system.memory.usage", "By"),
        ("system.memory.utilization", "1"),
        ("system.paging.usage", "By"),
        ("system.cpu.utilization", "1"),
        ("system.cpu.load_average.1m", "{thread}"),
        ("process.memory.usage", "By"),
        ("process.cpu.utilization", "1"),
        ("harold.machine.past_normal", "1"),
    ] {
        assert_eq!(unit_of(name).as_deref(), Some(unit), "{name}");
    }

    // No path is sent: no attribute value of these gauges holds a slash.
    let text = test_support::exported_text();
    for line in text
        .lines()
        .filter(|line| line.contains("system.filesystem"))
    {
        assert!(
            !line.contains("/System") && !line.contains("/Users"),
            "{line}"
        );
    }

    // The resource of the export names the machine.
    let resource = format!("{:?}", test_support::metrics().last().unwrap().resource());
    assert!(resource.contains("harold.machine"), "{resource}");

    // A failed reading sends no point: the gauges read nothing.
    rig.poll(minutes(start, 1), Err("no_home_disk")).await;
    assert_eq!(lock(&rig.snapshot).reading, None);
}

#[test]
fn the_reader_gives_readings_close_to_the_machine() {
    let mut reader = SystemReader::new();
    let first = reader.read().expect("a reading");
    std::thread::sleep(Duration::from_millis(300));
    let reading = reader.read().expect("a second reading");
    assert!(reading.disk_used > 0 && reading.disk_free > 0);
    assert!(reading.disk_utilization() > 0.0 && reading.disk_utilization() < 1.0);
    assert!(reading.memory_used > 0 && reading.memory_free > 0);
    assert!((0.0..=1.0).contains(&reading.cpu), "{}", reading.cpu);
    assert!(reading.process_memory > 0);
    assert!(reading.load_1m >= 0.0);
    // Print the values for a check against Activity Monitor and `df -h ~`.
    eprintln!("machine stats first read: {first:?}");
    eprintln!("machine stats second read: {reading:?}");
    eprintln!(
        "top memory: {}; top cpu: {}",
        reader.top_processes(Metric::Memory),
        reader.top_processes(Metric::Cpu)
    );
}

#[tokio::test]
async fn the_task_stops_at_shutdown() {
    let folder = Folder::new();
    let (shutdown_tx, shutdown_rx) = watch::channel(());
    let handle = start(folder.file(), shutdown_rx);
    drop(shutdown_tx);
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("the task stops")
        .unwrap();
}
