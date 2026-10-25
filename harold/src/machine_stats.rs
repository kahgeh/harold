//! Machine stats: the disk, memory, swap and CPU use of the machine Harold runs on. Every
//! minute Harold reads the machine and shows the values as gauges. It also keeps the
//! hourly use, the mean of each metric for each local hour, for 14 days, in the hourly
//! use file. From the hourly use it computes the normal range of memory, swap and CPU use
//! for the current hour of the day, and sends `harold.machine.normal_top` and
//! `harold.machine.past_normal`. Disk use has no normal range: the `disk-low` alert reads
//! the fixed limit.

use std::collections::{BTreeMap, VecDeque};
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Meter, ObservableGauge};
use serde::{Deserialize, Serialize};
use sysinfo::{
    Disks, MINIMUM_CPU_UPDATE_INTERVAL, Pid, ProcessRefreshKind, ProcessesToUpdate, System,
};
use time::{Date, Month, OffsetDateTime};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use tracing::{Instrument as _, info, info_span, warn};

use crate::telemetry::{self, TRACE_TARGET};

/// How often the machine is read.
const POLL_INTERVAL: Duration = Duration::from_secs(60);

/// How long the readings that make the mean compared with the top of the normal range
/// reach back.
const MEAN_WINDOW: time::Duration = time::Duration::minutes(15);

/// How many hours the hourly use is kept: 14 days.
const KEEP_HOURS: i64 = 336;

/// How many earlier days make the normal range, and how many of them must hold the hour.
const RANGE_DAYS: i64 = 7;
const MIN_DAYS: usize = 5;

/// The top of the normal range is the median plus this many times the median absolute
/// deviation.
const MAD_FACTOR: f64 = 3.0;

/// The least that the top of the normal range is above the median: 10 percentage points
/// of memory and of CPU, and 1 GB of swap.
const FLOOR_MEMORY: f64 = 0.10;
const FLOOR_CPU: f64 = 0.10;
const FLOOR_SWAP: f64 = 1_000_000_000.0;

/// The processes named in the `machine use past normal` line.
const TOP_PROCESSES: usize = 3;

/// The `reason` of `machine stats failed`. Each is fixed text: the field is exported.
const NO_HOME_DISK: &str = "no_home_disk";
const READ_PANICKED: &str = "read_panicked";
const HOURLY_USE_UNREADABLE: &str = "hourly_use_unreadable";
const HOURLY_USE_UNWRITABLE: &str = "hourly_use_unwritable";

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// ---------------------------------------------------------------------------
// The metrics
// ---------------------------------------------------------------------------

/// A metric with a normal range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Metric {
    Memory,
    Swap,
    Cpu,
}

impl Metric {
    pub(crate) const ALL: [Metric; 3] = [Metric::Memory, Metric::Swap, Metric::Cpu];

    /// The `metric` attribute's value.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Metric::Memory => "memory",
            Metric::Swap => "swap",
            Metric::Cpu => "cpu",
        }
    }

    fn index(self) -> usize {
        match self {
            Metric::Memory => 0,
            Metric::Swap => 1,
            Metric::Cpu => 2,
        }
    }

    /// The least that the top of the range is above the median, in the metric's unit.
    fn floor(self) -> f64 {
        match self {
            Metric::Memory => FLOOR_MEMORY,
            Metric::Swap => FLOOR_SWAP,
            Metric::Cpu => FLOOR_CPU,
        }
    }
}

/// One reading of the machine. Memory and CPU use are shares from 0 to 1, swap is bytes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Reading {
    pub disk_used: u64,
    pub disk_free: u64,
    pub memory_used: u64,
    pub memory_free: u64,
    pub swap_used: u64,
    pub cpu: f64,
    pub load_1m: f64,
    pub process_memory: u64,
    pub process_cpu: f64,
}

fn share(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        part as f64 / whole as f64
    }
}

impl Reading {
    pub(crate) fn disk_utilization(&self) -> f64 {
        share(
            self.disk_used,
            self.disk_used.saturating_add(self.disk_free),
        )
    }

    pub(crate) fn memory_utilization(&self) -> f64 {
        share(
            self.memory_used,
            self.memory_used.saturating_add(self.memory_free),
        )
    }

    /// The value that `metric`'s normal range is about.
    pub(crate) fn value(&self, metric: Metric) -> f64 {
        match metric {
            Metric::Memory => self.memory_utilization(),
            Metric::Swap => self.swap_used as f64,
            Metric::Cpu => self.cpu,
        }
    }
}

/// Reads the machine.
pub(crate) trait Reader: Send + 'static {
    /// One reading, or the fixed reason it failed.
    fn read(&mut self) -> Result<Reading, &'static str>;

    /// The 3 processes that use the most of `metric`, as `name share%` separated by
    /// commas. Names only: no arguments and no paths.
    fn top_processes(&mut self, metric: Metric) -> String;
}

// ---------------------------------------------------------------------------
// The hourly use
// ---------------------------------------------------------------------------

/// The local hour of `time` as a number: the hour of the local calendar, counted from the
/// Julian day 0. The hour is the wall clock's, so the hour that the clocks skip has no
/// number of its own and the hour that occurs twice has one.
pub(crate) fn hour_number(time: OffsetDateTime) -> i64 {
    i64::from(time.date().to_julian_day()) * 24 + i64::from(time.hour())
}

/// The mean of each metric over one local hour.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct HourMean {
    pub hour: i64,
    /// The readings behind the means.
    pub minutes: u32,
    pub means: [f64; 3],
}

/// The top of a normal range, and how many of the last 7 days it uses.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct NormalRange {
    pub top: f64,
    pub days: usize,
}

/// The hourly use of the last 14 days.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct HourlyUse {
    hours: BTreeMap<i64, HourMean>,
}

impl HourlyUse {
    /// Adds the mean of an hour. An hour that already has a mean, such as the hour that
    /// occurs twice when the clocks go back, gets one mean of both, weighted by readings.
    pub(crate) fn add(&mut self, new: HourMean) {
        if new.minutes == 0 {
            return;
        }
        let merged = match self.hours.get(&new.hour) {
            Some(old) => {
                let (old_weight, new_weight) = (f64::from(old.minutes), f64::from(new.minutes));
                let mut means = [0.0; 3];
                for (mean, (old_mean, new_mean)) in
                    means.iter_mut().zip(old.means.iter().zip(&new.means))
                {
                    *mean =
                        (old_mean * old_weight + new_mean * new_weight) / (old_weight + new_weight);
                }
                HourMean {
                    hour: new.hour,
                    minutes: old.minutes.saturating_add(new.minutes),
                    means,
                }
            }
            None => new,
        };
        self.hours.insert(merged.hour, merged);
    }

    /// Drops every mean more than 14 days (336 hours) before `now`.
    pub(crate) fn drop_old(&mut self, now: i64) {
        self.hours.retain(|hour, _| now - hour <= KEEP_HOURS);
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.hours.len()
    }

    /// The normal range of `metric` for the hour of the day that `now` is in: from that
    /// hour's means on the last 7 days. None with fewer than 5 of them, which is the one
    /// learning rule.
    pub(crate) fn normal_range(&self, metric: Metric, now: i64) -> Option<NormalRange> {
        let mut values: Vec<f64> = (1..=RANGE_DAYS)
            .filter_map(|day| self.hours.get(&(now - 24 * day)))
            .map(|hour| hour.means[metric.index()])
            .collect();
        if values.len() < MIN_DAYS {
            return None;
        }
        let days = values.len();
        let median = median(&mut values);
        let mut deviations: Vec<f64> = values.iter().map(|value| (value - median).abs()).collect();
        let deviation = self::median(&mut deviations);
        let top = (median + MAD_FACTOR * deviation).max(median + metric.floor());
        Some(NormalRange { top, days })
    }
}

/// The median of `values`, which are sorted in place. The values are not empty.
fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    let middle = values.len() / 2;
    if values.len() % 2 == 1 {
        values[middle]
    } else {
        (values[middle - 1] + values[middle]) / 2.0
    }
}

/// The hourly use file, `<harold's folder>/data/machine-hourly-use.json`, which is beside
/// the store.
pub(crate) fn hourly_use_file(store_path: &Path) -> PathBuf {
    store_path
        .parent()
        .unwrap_or(Path::new("."))
        .join("machine-hourly-use.json")
}

#[derive(Serialize, Deserialize)]
struct FileContent {
    hours: Vec<FileHour>,
}

#[derive(Serialize, Deserialize)]
struct FileHour {
    /// The local date, `YYYY-MM-DD`.
    date: String,
    /// The local hour of that date, 0 to 23.
    hour: u8,
    minutes: u32,
    memory: f64,
    swap: f64,
    cpu: f64,
}

fn date_text(date: Date) -> String {
    format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        u8::from(date.month()),
        date.day()
    )
}

fn parse_date(text: &str) -> Option<Date> {
    let mut parts = text.split('-');
    let year = parts.next()?.parse().ok()?;
    let month = Month::try_from(parts.next()?.parse::<u8>().ok()?).ok()?;
    let day = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Date::from_calendar_date(year, month, day).ok()
}

impl HourlyUse {
    fn to_json(&self) -> serde_json::Result<String> {
        let hours = self
            .hours
            .values()
            .filter_map(|hour| {
                let date =
                    Date::from_julian_day(i32::try_from(hour.hour.div_euclid(24)).ok()?).ok()?;
                Some(FileHour {
                    date: date_text(date),
                    hour: u8::try_from(hour.hour.rem_euclid(24)).ok()?,
                    minutes: hour.minutes,
                    memory: hour.means[0],
                    swap: hour.means[1],
                    cpu: hour.means[2],
                })
            })
            .collect();
        serde_json::to_string(&FileContent { hours })
    }

    /// The hourly use in `text`. A row that does not read, or holds a number that is not
    /// finite, is an error: the whole file is then not used.
    fn from_json(text: &str) -> Result<Self, ()> {
        let content: FileContent = serde_json::from_str(text).map_err(|_| ())?;
        let mut hourly = HourlyUse::default();
        for row in content.hours {
            let date = parse_date(&row.date).ok_or(())?;
            if row.hour > 23
                || ![row.memory, row.swap, row.cpu]
                    .iter()
                    .all(|v| v.is_finite())
            {
                return Err(());
            }
            hourly.add(HourMean {
                hour: i64::from(date.to_julian_day()) * 24 + i64::from(row.hour),
                minutes: row.minutes,
                means: [row.memory, row.swap, row.cpu],
            });
        }
        Ok(hourly)
    }

    /// The hourly use in `file`, without the means more than 14 days before `now`. A file
    /// that is missing gives an empty one. A file that cannot be read also does, after the
    /// warn line `machine stats failed`.
    pub(crate) fn load(file: &Path, now: i64) -> Self {
        let text = match std::fs::read_to_string(file) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Self::default();
            }
            Err(_) => {
                warn!(reason = HOURLY_USE_UNREADABLE, "machine stats failed");
                return Self::default();
            }
        };
        let Ok(mut hourly) = Self::from_json(&text) else {
            warn!(reason = HOURLY_USE_UNREADABLE, "machine stats failed");
            return Self::default();
        };
        hourly.drop_old(now);
        hourly
    }

    /// Writes the hourly use to `file` through a temporary file beside it, so a reader
    /// never sees half a file. Only the user can read it.
    pub(crate) fn save(&self, file: &Path) -> std::io::Result<()> {
        if let Some(folder) = file.parent() {
            std::fs::create_dir_all(folder)?;
        }
        let text = self.to_json().map_err(std::io::Error::other)?;
        let temp = file.with_extension("tmp");
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temp)?;
        out.write_all(text.as_bytes())?;
        out.sync_all()?;
        std::fs::rename(&temp, file)
    }
}

// ---------------------------------------------------------------------------
// The gauges
// ---------------------------------------------------------------------------

/// What the latest poll read, for the gauges.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct Snapshot {
    /// The latest reading. None before the first one and after a reading that failed, so
    /// no gauge reports an old value.
    pub reading: Option<Reading>,
    /// The top of each metric's normal range, when the hour can be judged.
    pub normal_top: [Option<f64>; 3],
    /// Whether each metric is past its normal range. None before the first reading.
    pub past_normal: Option<[bool; 3]>,
}

struct Gauges {
    snapshot: Arc<Mutex<Snapshot>>,
    _gauges: Vec<Box<dyn std::any::Any + Send + Sync>>,
}

static GAUGES: OnceLock<Gauges> = OnceLock::new();

type Points<T> = Vec<(T, Vec<KeyValue>)>;

fn gauge_u64(
    meter: &Meter,
    snapshot: &Arc<Mutex<Snapshot>>,
    name: &'static str,
    unit: &'static str,
    points: fn(&Snapshot) -> Points<u64>,
) -> ObservableGauge<u64> {
    let snapshot = Arc::clone(snapshot);
    meter
        .u64_observable_gauge(name)
        .with_unit(unit)
        .with_callback(move |observer| {
            for (value, attributes) in points(&lock(&snapshot)) {
                observer.observe(value, &attributes);
            }
        })
        .build()
}

fn gauge_f64(
    meter: &Meter,
    snapshot: &Arc<Mutex<Snapshot>>,
    name: &'static str,
    unit: Option<&'static str>,
    points: fn(&Snapshot) -> Points<f64>,
) -> ObservableGauge<f64> {
    let snapshot = Arc::clone(snapshot);
    let builder = meter.f64_observable_gauge(name);
    let builder = match unit {
        Some(unit) => builder.with_unit(unit),
        None => builder,
    };
    builder
        .with_callback(move |observer| {
            for (value, attributes) in points(&lock(&snapshot)) {
                observer.observe(value, &attributes);
            }
        })
        .build()
}

fn volume() -> KeyValue {
    KeyValue::new("volume", "home")
}

fn state(state: &'static str) -> KeyValue {
    KeyValue::new("state", state)
}

fn metric_attribute(metric: Metric) -> KeyValue {
    KeyValue::new("metric", metric.as_str())
}

/// The snapshot that the machine gauges read. The first call registers the gauges.
pub(crate) fn gauge_snapshot() -> Arc<Mutex<Snapshot>> {
    let gauges = GAUGES.get_or_init(|| {
        let snapshot = Arc::new(Mutex::new(Snapshot::default()));
        let meter = telemetry::meter();
        let gauges: Vec<Box<dyn std::any::Any + Send + Sync>> = vec![
            Box::new(gauge_u64(
                &meter,
                &snapshot,
                "system.filesystem.usage",
                "By",
                |snapshot| {
                    snapshot.reading.map_or_else(Vec::new, |reading| {
                        vec![
                            (reading.disk_used, vec![volume(), state("used")]),
                            (reading.disk_free, vec![volume(), state("free")]),
                        ]
                    })
                },
            )),
            Box::new(gauge_f64(
                &meter,
                &snapshot,
                "system.filesystem.utilization",
                Some("1"),
                |snapshot| {
                    snapshot.reading.map_or_else(Vec::new, |reading| {
                        vec![(reading.disk_utilization(), vec![volume()])]
                    })
                },
            )),
            Box::new(gauge_u64(
                &meter,
                &snapshot,
                "system.memory.usage",
                "By",
                |snapshot| {
                    snapshot.reading.map_or_else(Vec::new, |reading| {
                        vec![
                            (reading.memory_used, vec![state("used")]),
                            (reading.memory_free, vec![state("free")]),
                        ]
                    })
                },
            )),
            Box::new(gauge_f64(
                &meter,
                &snapshot,
                "system.memory.utilization",
                Some("1"),
                |snapshot| {
                    snapshot.reading.map_or_else(Vec::new, |reading| {
                        vec![(reading.memory_utilization(), Vec::new())]
                    })
                },
            )),
            Box::new(gauge_u64(
                &meter,
                &snapshot,
                "system.paging.usage",
                "By",
                |snapshot| {
                    snapshot
                        .reading
                        .map_or_else(Vec::new, |reading| vec![(reading.swap_used, Vec::new())])
                },
            )),
            Box::new(gauge_f64(
                &meter,
                &snapshot,
                "system.cpu.utilization",
                Some("1"),
                |snapshot| {
                    snapshot
                        .reading
                        .map_or_else(Vec::new, |reading| vec![(reading.cpu, Vec::new())])
                },
            )),
            Box::new(gauge_f64(
                &meter,
                &snapshot,
                "system.cpu.load_average.1m",
                Some("{thread}"),
                |snapshot| {
                    snapshot
                        .reading
                        .map_or_else(Vec::new, |reading| vec![(reading.load_1m, Vec::new())])
                },
            )),
            Box::new(gauge_u64(
                &meter,
                &snapshot,
                "process.memory.usage",
                "By",
                |snapshot| {
                    snapshot.reading.map_or_else(Vec::new, |reading| {
                        vec![(reading.process_memory, Vec::new())]
                    })
                },
            )),
            Box::new(gauge_f64(
                &meter,
                &snapshot,
                "process.cpu.utilization",
                Some("1"),
                |snapshot| {
                    snapshot
                        .reading
                        .map_or_else(Vec::new, |reading| vec![(reading.process_cpu, Vec::new())])
                },
            )),
            // The unit is the metric's own: a share for memory and CPU, bytes for swap.
            Box::new(gauge_f64(
                &meter,
                &snapshot,
                "harold.machine.normal_top",
                None,
                |snapshot| {
                    Metric::ALL
                        .into_iter()
                        .filter_map(|metric| {
                            let top = snapshot.normal_top[metric.index()]?;
                            Some((top, vec![metric_attribute(metric)]))
                        })
                        .collect()
                },
            )),
            Box::new(gauge_u64(
                &meter,
                &snapshot,
                "harold.machine.past_normal",
                "1",
                |snapshot| {
                    let Some(past) = snapshot.past_normal else {
                        return Vec::new();
                    };
                    Metric::ALL
                        .into_iter()
                        .map(|metric| {
                            (
                                u64::from(past[metric.index()]),
                                vec![metric_attribute(metric)],
                            )
                        })
                        .collect()
                },
            )),
        ];
        Gauges {
            snapshot: Arc::clone(&snapshot),
            _gauges: gauges,
        }
    });
    Arc::clone(&gauges.snapshot)
}

// ---------------------------------------------------------------------------
// The poll
// ---------------------------------------------------------------------------

/// Where one metric is past its normal range, since when.
#[derive(Debug, Clone, Copy)]
struct Spell {
    since: OffsetDateTime,
}

/// The readings of the local hour that is under way.
struct Running {
    hour: i64,
    sums: [f64; 3],
    count: u32,
}

pub(crate) struct MachineStats<R: Reader> {
    reader: Arc<Mutex<R>>,
    file: PathBuf,
    hourly: HourlyUse,
    running: Option<Running>,
    /// The readings of the last 15 minutes.
    window: VecDeque<(OffsetDateTime, [f64; 3])>,
    spells: [Option<Spell>; 3],
    snapshot: Arc<Mutex<Snapshot>>,
    /// The reason of the reading failure that has been logged since a reading last worked.
    failing: Option<&'static str>,
}

impl<R: Reader> MachineStats<R> {
    /// Reads the hourly use file, dropping the means more than 14 days before `now`.
    pub(crate) fn new(
        reader: R,
        file: PathBuf,
        snapshot: Arc<Mutex<Snapshot>>,
        now: OffsetDateTime,
    ) -> Self {
        let hourly = HourlyUse::load(&file, hour_number(now));
        Self {
            reader: Arc::new(Mutex::new(reader)),
            file,
            hourly,
            running: None,
            window: VecDeque::new(),
            spells: [None; 3],
            snapshot,
            failing: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn hourly(&self) -> &HourlyUse {
        &self.hourly
    }

    /// Reads the machine once, at the local time `now`.
    pub(crate) async fn poll(&mut self, now: OffsetDateTime) {
        self.end_hour(now);
        let span = info_span!(
            target: TRACE_TARGET,
            "harold.machine_stats",
            result = tracing::field::Empty
        );
        let reader = Arc::clone(&self.reader);
        let reading = async move {
            tokio::task::spawn_blocking(move || lock(&reader).read())
                .await
                .unwrap_or(Err(READ_PANICKED))
        }
        .instrument(span.clone())
        .await;
        span.record("result", if reading.is_ok() { "ok" } else { "failed" });
        drop(span);
        match reading {
            Ok(reading) => {
                self.failing = None;
                self.apply(now, reading).await;
            }
            Err(reason) => {
                if self.failing != Some(reason) {
                    self.failing = Some(reason);
                    warn!(reason, "machine stats failed");
                }
                // No gauge reports an old reading. The range and the spells stay.
                lock(&self.snapshot).reading = None;
            }
        }
    }

    /// Writes the means of the hour that ended, when `now` is in another hour.
    fn end_hour(&mut self, now: OffsetDateTime) {
        let hour = hour_number(now);
        if self
            .running
            .as_ref()
            .is_none_or(|running| running.hour == hour)
        {
            return;
        }
        self.write_running(hour);
    }

    /// Adds the readings of the hour under way and writes the file. Harold calls it at
    /// shutdown, so the part of the hour that it ran for keeps its mean. A later start in
    /// the same hour adds its own part, weighted by readings.
    pub(crate) fn finish(&mut self, now: OffsetDateTime) {
        self.write_running(hour_number(now));
    }

    fn write_running(&mut self, hour: i64) {
        let Some(done) = self.running.take() else {
            return;
        };
        if done.count > 0 {
            let count = f64::from(done.count);
            self.hourly.add(HourMean {
                hour: done.hour,
                minutes: done.count,
                means: done.sums.map(|sum| sum / count),
            });
        }
        self.hourly.drop_old(hour);
        if let Err(error) = self.hourly.save(&self.file) {
            warn!(reason = HOURLY_USE_UNWRITABLE, error = %error, "machine stats failed");
        }
    }

    async fn apply(&mut self, now: OffsetDateTime, reading: Reading) {
        let hour = hour_number(now);
        let values = Metric::ALL.map(|metric| reading.value(metric));
        let running = self.running.get_or_insert(Running {
            hour,
            sums: [0.0; 3],
            count: 0,
        });
        for (sum, value) in running.sums.iter_mut().zip(values) {
            *sum += value;
        }
        running.count += 1;

        self.window.push_back((now, values));
        while self
            .window
            .front()
            .is_some_and(|(at, _)| now - *at > MEAN_WINDOW)
        {
            self.window.pop_front();
        }
        let readings = self.window.len() as f64;

        let mut rises = Vec::new();
        let mut normal_top = [None; 3];
        for metric in Metric::ALL {
            let index = metric.index();
            let mean = self
                .window
                .iter()
                .map(|(_, values)| values[index])
                .sum::<f64>()
                / readings;
            let range = self.hourly.normal_range(metric, hour);
            normal_top[index] = range.map(|range| range.top);
            let past = range.filter(|range| mean > range.top);
            match (past, self.spells[index]) {
                (Some(range), None) => {
                    self.spells[index] = Some(Spell { since: now });
                    rises.push((metric, mean, range));
                }
                (None, Some(spell)) => {
                    self.spells[index] = None;
                    let minutes = (now - spell.since).whole_minutes();
                    info!(
                        metric = metric.as_str(),
                        minutes, "machine use back to normal"
                    );
                }
                _ => {}
            }
        }
        {
            let mut snapshot = lock(&self.snapshot);
            snapshot.reading = Some(reading);
            snapshot.normal_top = normal_top;
            snapshot.past_normal = Some(self.spells.map(|spell| spell.is_some()));
        }
        for (metric, value, range) in rises {
            let reader = Arc::clone(&self.reader);
            let top_processes =
                tokio::task::spawn_blocking(move || lock(&reader).top_processes(metric))
                    .await
                    .unwrap_or_default();
            warn!(
                metric = metric.as_str(),
                value,
                normal_top = range.top,
                days = range.days,
                top_processes = %top_processes,
                "machine use past normal"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Reading the machine
// ---------------------------------------------------------------------------

/// Reads this machine with `sysinfo`.
pub(crate) struct SystemReader {
    system: System,
    disks: Disks,
    home: PathBuf,
    pid: Option<Pid>,
}

impl SystemReader {
    /// A reader for the disk that holds the home folder. It reads the CPU once, because
    /// CPU use is the change since the last read.
    pub(crate) fn new() -> Self {
        let mut system = System::new();
        system.refresh_cpu_usage();
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .and_then(|home| std::fs::canonicalize(home).ok())
            .unwrap_or_else(|| PathBuf::from("/"));
        Self {
            system,
            disks: Disks::new(),
            home,
            pid: sysinfo::get_current_pid().ok(),
        }
    }

    fn cores(&self) -> f64 {
        self.system.cpus().len().max(1) as f64
    }
}

impl Reader for SystemReader {
    fn read(&mut self) -> Result<Reading, &'static str> {
        self.system.refresh_memory();
        self.system.refresh_cpu_usage();
        self.disks
            .refresh_specifics(true, sysinfo::DiskRefreshKind::nothing().with_storage());
        let disk = self
            .disks
            .list()
            .iter()
            .filter(|disk| self.home.starts_with(disk.mount_point()))
            .max_by_key(|disk| disk.mount_point().as_os_str().len())
            .ok_or(NO_HOME_DISK)?;
        let (disk_free, disk_total) = (disk.available_space(), disk.total_space());
        let (mut process_memory, mut process_cpu) = (0, 0.0);
        if let Some(pid) = self.pid {
            self.system.refresh_processes_specifics(
                ProcessesToUpdate::Some(&[pid]),
                true,
                ProcessRefreshKind::nothing().with_cpu().with_memory(),
            );
            if let Some(process) = self.system.process(pid) {
                process_memory = process.memory();
                process_cpu = f64::from(process.cpu_usage()) / 100.0 / self.cores();
            }
        }
        let memory_used = self.system.used_memory();
        Ok(Reading {
            disk_used: disk_total.saturating_sub(disk_free),
            disk_free,
            memory_used,
            memory_free: self.system.total_memory().saturating_sub(memory_used),
            swap_used: self.system.used_swap(),
            cpu: f64::from(self.system.global_cpu_usage()) / 100.0,
            load_1m: System::load_average().one,
            process_memory,
            process_cpu,
        })
    }

    fn top_processes(&mut self, metric: Metric) -> String {
        let kind = ProcessRefreshKind::nothing().with_cpu().with_memory();
        self.system
            .refresh_processes_specifics(ProcessesToUpdate::All, true, kind);
        if metric == Metric::Cpu {
            // A process's CPU use is the change between two reads.
            std::thread::sleep(MINIMUM_CPU_UPDATE_INTERVAL);
            self.system
                .refresh_processes_specifics(ProcessesToUpdate::All, true, kind);
        }
        let total_memory = self.system.total_memory();
        let cores = self.cores();
        // sysinfo has no use of swap for each process, so swap names the largest in
        // memory.
        let mut processes: Vec<(String, f64)> = self
            .system
            .processes()
            .values()
            .map(|process| {
                let used = match metric {
                    Metric::Cpu => f64::from(process.cpu_usage()) / 100.0 / cores,
                    Metric::Memory | Metric::Swap => share(process.memory(), total_memory),
                };
                (process.name().to_string_lossy().into_owned(), used)
            })
            .collect();
        processes.sort_by(|a, b| b.1.total_cmp(&a.1));
        processes
            .iter()
            .take(TOP_PROCESSES)
            .map(|(name, used)| format!("{name} {:.0}%", used * 100.0))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

// ---------------------------------------------------------------------------
// The task
// ---------------------------------------------------------------------------

/// The UTC offset in `date +%z` form, such as `+1100` or `-0330`.
pub(crate) fn parse_offset(text: &str) -> Option<time::UtcOffset> {
    let text = text.trim();
    let sign = match text.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let digits = text.get(1..)?;
    if digits.len() != 4 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let hours: i8 = digits[..2].parse().ok()?;
    let minutes: i8 = digits[2..].parse().ok()?;
    time::UtcOffset::from_hms(sign * hours, sign * minutes, 0).ok()
}

/// The local time now. The offset is read again each time, because the `time` crate
/// cannot read it once threads run and the offset changes when the clocks change. When
/// `date` fails, it is the offset that `main` read at start.
async fn local_now() -> OffsetDateTime {
    let offset = match tokio::process::Command::new("date")
        .arg("+%z")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
    {
        Ok(output) if output.status.success() => {
            parse_offset(&String::from_utf8_lossy(&output.stdout))
        }
        _ => None,
    };
    OffsetDateTime::now_utc().to_offset(offset.unwrap_or_else(telemetry::local_offset))
}

/// Reads the machine every minute until shutdown.
async fn run<R: Reader>(mut stats: MachineStats<R>, mut shutdown: watch::Receiver<()>) {
    let mut ticker =
        tokio::time::interval_at(tokio::time::Instant::now() + POLL_INTERVAL, POLL_INTERVAL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => break,
            _ = ticker.tick() => {}
        }
        tokio::select! {
            biased;
            _ = shutdown.changed() => break,
            () = async { stats.poll(local_now().await).await } => {}
        }
    }
    stats.finish(local_now().await);
}

/// Starts reading the machine, with the hourly use file at `file`.
pub(crate) fn start(file: PathBuf, shutdown: watch::Receiver<()>) -> JoinHandle<()> {
    let stats = MachineStats::new(
        SystemReader::new(),
        file,
        gauge_snapshot(),
        OffsetDateTime::now_utc().to_offset(telemetry::local_offset()),
    );
    tokio::spawn(run(stats, shutdown))
}

#[cfg(test)]
#[path = "machine_stats_tests.rs"]
mod tests;
