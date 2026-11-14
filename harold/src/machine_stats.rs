//! Machine stats: the disk, memory, swap and CPU use of the machine Harold runs on. Every
//! minute Harold reads the machine and shows the values as gauges. It also keeps the
//! hourly use, the mean of each metric for each local hour, for 14 days, in the hourly
//! use file. From the hourly use it computes the normal range of memory, swap and CPU use
//! for the current hour of the day, and sends `harold.machine.normal_top` and
//! `harold.machine.past_normal`. Disk use has no normal range: the `disk-low` alert reads
//! the fixed limit.

use std::collections::{BTreeMap, HashMap, VecDeque};
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
use tracing::{Instrument as _, debug, info, info_span, warn};

use crate::agent::snapshot::AgentSnapshotHub;
use crate::pod_roles::PodRoles;
use crate::pod_stats::{AGENT_PANE, POD_FOLDER, key_values};
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
    /// The CPU and the memory of each agent at the latest poll, with `none` for the rest.
    /// Empty before the first reading and after a reading that failed.
    pub agents: Vec<AgentTotal>,
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

/// The attributes of an agent gauge. They are metric attributes only: no log line carries
/// them.
fn owner_attributes(owner: &Owner) -> Vec<KeyValue> {
    vec![
        KeyValue::new("role", owner.role),
        KeyValue::new("agent.label", owner.label.clone()),
        KeyValue::new("pod.product", owner.product.clone()),
        KeyValue::new("spec.id", owner.spec.clone()),
    ]
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
            Box::new(gauge_f64(
                &meter,
                &snapshot,
                "harold.agent.cpu.utilization",
                Some("1"),
                |snapshot| {
                    snapshot
                        .agents
                        .iter()
                        .map(|total| (total.cpu, owner_attributes(&total.owner)))
                        .collect()
                },
            )),
            Box::new(gauge_u64(
                &meter,
                &snapshot,
                "harold.agent.memory.usage",
                "By",
                |snapshot| {
                    snapshot
                        .agents
                        .iter()
                        .map(|total| (total.memory, owner_attributes(&total.owner)))
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
// Which agent a process belongs to
// ---------------------------------------------------------------------------

/// The most parents that the walk follows from one process.
const MAX_DEPTH: usize = 64;
/// How far the start time of an agent in the agent monitor may differ from the start time
/// of its process, in seconds. The two are read apart.
const START_TOLERANCE: i64 = 2;

/// The `role`, `agent.label`, `pod.product` and `spec.id` value of a process with no value.
const NONE: &str = "none";
const OTHER: &str = "other";
const CHIEF: &str = "chief";

/// One process of the process table.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ProcInfo {
    pub pid: u32,
    pub parent: Option<u32>,
    /// The start time in seconds since the epoch.
    pub start: u64,
    pub name: String,
    /// The CPU share of the whole machine, from 0 to 1.
    pub cpu: f64,
    /// The resident memory in bytes.
    pub memory: u64,
    /// The current folder, when it is known.
    pub cwd: Option<PathBuf>,
}

/// A live agent of the agent monitor, of any provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentProc {
    pub pid: u32,
    pub start_ms: i64,
    pub pane_id: String,
    /// The working directory of the pane.
    pub folder: String,
}

/// The agent that owns a process: the values of the agent gauges.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Owner {
    pub role: &'static str,
    pub label: String,
    pub product: String,
    pub spec: String,
}

impl Owner {
    fn none() -> Self {
        Self::plain(NONE, NONE)
    }

    fn other() -> Self {
        Self::plain(OTHER, OTHER)
    }

    fn plain(role: &'static str, label: &str) -> Self {
        Self {
            role,
            label: label.to_owned(),
            product: NONE.to_owned(),
            spec: NONE.to_owned(),
        }
    }
}

/// Reads the environment of one process.
pub(crate) trait ProcessEnv {
    /// The variables of process `pid`, or `None` when they cannot be read. The caller keeps
    /// three of them and drops the rest at once.
    fn environ(&mut self, pid: u32) -> Option<Vec<(String, String)>>;
}

/// The longest `POD_PRODUCT` and `POD_SPEC` that Harold accepts, and the longest label.
const NAME_LIMIT: usize = 64;

/// The only variables of an agent process that Harold keeps, each checked. A value that
/// fails its rule is `None`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct PodEnv {
    role: Option<&'static str>,
    product: Option<String>,
    spec: Option<String>,
}

fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= NAME_LIMIT
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

impl PodEnv {
    /// Keeps `POD_ROLE`, `POD_PRODUCT` and `POD_SPEC` and drops every other variable. No
    /// value of a variable is kept in a field that a log, a span or an error can show unless
    /// it passed its rule.
    fn from_vars(vars: Vec<(String, String)>) -> Self {
        let mut kept = Self::default();
        for (key, value) in vars {
            match key.as_str() {
                "POD_ROLE" => {
                    kept.role = ["chief", "pnem", "pod_coordination", "build_worker", "other"]
                        .into_iter()
                        .find(|role| *role == value);
                }
                "POD_PRODUCT" => kept.product = valid_name(&value).then_some(value),
                "POD_SPEC" => kept.spec = valid_name(&value).then_some(value),
                _ => {}
            }
        }
        kept
    }
}

/// Gives each process of a table to the agent that started it.
#[derive(Default)]
pub(crate) struct Walker {
    /// The checked environment of each agent process, by pid and start time. It holds an
    /// agent that has no readable environment too, so each is read once.
    environments: HashMap<(u32, u64), PodEnv>,
}

impl Walker {
    /// How many agent environments Harold holds.
    pub(crate) fn cached(&self) -> usize {
        self.environments.len()
    }

    /// The owner of every process in `table`. The walk follows the parents of a process up
    /// to the first live agent of `agents`. It stops at a parent that is not in the table,
    /// at a parent that started after its child (the parent pid is used again), at a loop
    /// and after `MAX_DEPTH` parents. An agent gets its owner from the role record, then
    /// from the agent-pane file of its folder, then from the environment of its process.
    pub(crate) fn assign(
        &mut self,
        table: &[ProcInfo],
        agents: &[AgentProc],
        roles: Option<&PodRoles>,
        env: &mut dyn ProcessEnv,
    ) -> HashMap<u32, Owner> {
        let by_pid: HashMap<u32, &ProcInfo> =
            table.iter().map(|process| (process.pid, process)).collect();
        let live: HashMap<u32, &AgentProc> = agents.iter().map(|a| (a.pid, a)).collect();
        let mut agent_owners: HashMap<u32, Owner> = HashMap::new();
        let mut owners = HashMap::new();
        let mut seen = std::collections::HashSet::new();
        for process in table {
            let owner = match walk(process, &by_pid, &live) {
                None => Owner::none(),
                Some(agent) => agent_owners
                    .entry(agent.pid)
                    .or_insert_with(|| {
                        seen.insert((agent.pid, agent.start));
                        self.agent_owner(agent, live[&agent.pid], roles, env)
                    })
                    .clone(),
            };
            owners.insert(process.pid, owner);
        }
        self.environments.retain(|key, _| seen.contains(key));
        owners
    }

    fn agent_owner(
        &mut self,
        process: &ProcInfo,
        agent: &AgentProc,
        roles: Option<&PodRoles>,
        env: &mut dyn ProcessEnv,
    ) -> Owner {
        let start = i64::try_from(process.start).unwrap_or(i64::MAX);
        if let Some((product, role)) = roles.and_then(|roles| roles.by_pid(process.pid, start)) {
            return record_owner(&product, &role);
        }
        if let Some(owner) = build_worker_owner(agent) {
            return owner;
        }
        let key = (process.pid, process.start);
        let pod_env = self
            .environments
            .entry(key)
            .or_insert_with(|| PodEnv::from_vars(env.environ(process.pid).unwrap_or_default()));
        let folder = process
            .cwd
            .clone()
            .or_else(|| (!agent.folder.is_empty()).then(|| PathBuf::from(&agent.folder)));
        env_owner(pod_env, folder.as_deref())
    }
}

/// The first process, from `process` up, that is a live agent.
fn walk<'a>(
    process: &'a ProcInfo,
    by_pid: &HashMap<u32, &'a ProcInfo>,
    live: &HashMap<u32, &AgentProc>,
) -> Option<&'a ProcInfo> {
    let mut current = process;
    for _ in 0..=MAX_DEPTH {
        let is_agent = live.get(&current.pid).is_some_and(|agent| {
            let start = i64::try_from(current.start).unwrap_or(i64::MAX);
            (agent.start_ms / 1000 - start).abs() <= START_TOLERANCE
        });
        if is_agent {
            return Some(current);
        }
        let parent = by_pid.get(&current.parent?)?;
        if parent.start > current.start {
            return None;
        }
        current = parent;
    }
    None
}

fn pod_role(role: &str) -> &'static str {
    match role {
        "chief" => CHIEF,
        "pnem" => "pnem",
        "pod_coordination" => "pod_coordination",
        _ => OTHER,
    }
}

/// The owner for a role record: the chief, or a role of a product.
fn record_owner(product: &str, role: &str) -> Owner {
    match pod_role(role) {
        CHIEF => Owner::plain(CHIEF, CHIEF),
        OTHER => Owner::other(),
        role => with_product(role, product),
    }
}

fn with_product(role: &'static str, product: &str) -> Owner {
    Owner {
        role,
        label: product.to_owned(),
        product: product.to_owned(),
        spec: NONE.to_owned(),
    }
}

/// The owner for a validated environment. A role that needs a product and has none, or an
/// environment with no role, is `other`.
fn env_owner(env: &PodEnv, folder: Option<&Path>) -> Owner {
    match env.role {
        Some(CHIEF) => Owner::plain(CHIEF, CHIEF),
        Some(role @ ("pnem" | "pod_coordination")) => env
            .product
            .as_deref()
            .map_or_else(Owner::other, |product| with_product(role, product)),
        Some("build_worker") => {
            let Some(label) = folder.and_then(folder_name) else {
                return Owner::other();
            };
            Owner {
                role: "build_worker",
                label,
                product: env.product.clone().unwrap_or_else(|| NONE.to_owned()),
                spec: env.spec.clone().unwrap_or_else(|| NONE.to_owned()),
            }
        }
        _ => Owner::other(),
    }
}

/// The last part of `folder`, cut to the label limit.
fn folder_name(folder: &Path) -> Option<String> {
    let name = folder.file_name()?.to_string_lossy();
    Some(name.chars().take(NAME_LIMIT).collect())
}

/// The owner when the agent-pane file of the agent's folder names the agent's pane: a build
/// worker. The label is the folder name. The spec id is that of the `started/<id>.md` file
/// that names the worktree, found through the `.git` file of the worktree.
fn build_worker_owner(agent: &AgentProc) -> Option<Owner> {
    if agent.folder.is_empty() {
        return None;
    }
    let folder = Path::new(&agent.folder);
    let text = std::fs::read_to_string(folder.join(AGENT_PANE)).ok()?;
    let pane = text.lines().next()?.split_whitespace().next()?;
    if pane != agent.pane_id {
        return None;
    }
    let label = folder_name(folder)?;
    let spec = started_spec(folder, &label).unwrap_or_else(|| NONE.to_owned());
    Some(Owner {
        role: "build_worker",
        label,
        product: NONE.to_owned(),
        spec,
    })
}

/// The id of the spec whose `started/<id>.md` file names `folder` in its `worktree:` pair.
/// The pod folder is the one of the base checkout that the worktree's `.git` file points
/// into. A worktree whose base checkout is not found has no spec id.
fn started_spec(folder: &Path, label: &str) -> Option<String> {
    let git = std::fs::read_to_string(folder.join(".git")).ok()?;
    let gitdir = git.lines().next()?.strip_prefix("gitdir:")?.trim();
    let (base, _) = gitdir.split_once("/.git/worktrees/")?;
    let mut files: Vec<PathBuf> =
        std::fs::read_dir(Path::new(base).join(POD_FOLDER).join("started"))
            .ok()?
            .filter_map(|entry| Some(entry.ok()?.path()))
            .filter(|path| path.extension().is_some_and(|extension| extension == "md"))
            .collect();
    files.sort();
    files.into_iter().find_map(|path| {
        let text = std::fs::read_to_string(&path).ok()?;
        let named = *key_values(&text).get("worktree")?;
        let same = named == label || Path::new(named) == folder;
        let id = path.file_stem()?.to_str()?;
        (same && valid_name(id)).then(|| id.to_owned())
    })
}

/// Reads the process table of the machine and the environment of one process.
pub(crate) trait ProcessSource: ProcessEnv + Send + 'static {
    /// Every process, with its parent, start time, CPU share and memory.
    fn processes(&mut self) -> Vec<ProcInfo>;
}

/// The live agents of the agent monitor.
pub(crate) type AgentSource = Arc<dyn Fn() -> Vec<AgentProc> + Send + Sync>;
/// The pod role record, when there is one.
pub(crate) type RolesSource = Arc<dyn Fn() -> Option<Arc<PodRoles>> + Send + Sync>;

/// The CPU and the memory of the processes of one agent.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AgentTotal {
    pub owner: Owner,
    /// The CPU share of the whole machine, from 0 to 1.
    pub cpu: f64,
    /// The resident memory in bytes.
    pub memory: u64,
    /// The processes of the agent, the ones with one name together.
    pub processes: Vec<ProcUse>,
}

/// The use of the processes that have one name.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ProcUse {
    pub name: String,
    pub cpu: f64,
    pub memory: u64,
}

/// What one walk of the process table did, for the debug line.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct WalkCounts {
    processes: usize,
    cached: usize,
    walked: usize,
    environ_reads: usize,
}

/// Counts the environment reads of one walk.
struct CountingEnv<'a> {
    inner: &'a mut dyn ProcessEnv,
    reads: usize,
}

impl ProcessEnv for CountingEnv<'_> {
    fn environ(&mut self, pid: u32) -> Option<Vec<(String, String)>> {
        self.reads += 1;
        self.inner.environ(pid)
    }
}

/// Gives the processes of the machine to their agents at each poll.
pub(crate) struct AgentUse {
    source: Box<dyn ProcessSource>,
    agents: AgentSource,
    roles: RolesSource,
    walker: Walker,
}

impl AgentUse {
    pub(crate) fn new(
        source: Box<dyn ProcessSource>,
        agents: AgentSource,
        roles: RolesSource,
    ) -> Self {
        Self {
            source,
            agents,
            roles,
            walker: Walker::default(),
        }
    }

    /// The total of each agent, largest CPU first, and what the walk did.
    fn read(&mut self) -> (Vec<AgentTotal>, WalkCounts) {
        let table = self.source.processes();
        let agents = (self.agents)();
        let roles = (self.roles)();
        let mut env = CountingEnv {
            inner: &mut *self.source,
            reads: 0,
        };
        let owners = self
            .walker
            .assign(&table, &agents, roles.as_deref(), &mut env);
        let environ_reads = env.reads;
        let mut totals: Vec<AgentTotal> = Vec::new();
        let mut walked = 0;
        for process in &table {
            let Some(owner) = owners.get(&process.pid) else {
                continue;
            };
            if owner.role != NONE {
                walked += 1;
            }
            let total = match totals.iter().position(|total| total.owner == *owner) {
                Some(index) => &mut totals[index],
                None => {
                    totals.push(AgentTotal {
                        owner: owner.clone(),
                        cpu: 0.0,
                        memory: 0,
                        processes: Vec::new(),
                    });
                    totals.last_mut().expect("just pushed")
                }
            };
            total.cpu += process.cpu;
            total.memory += process.memory;
            match total
                .processes
                .iter_mut()
                .find(|used| used.name == process.name)
            {
                Some(used) => {
                    used.cpu += process.cpu;
                    used.memory += process.memory;
                }
                None => total.processes.push(ProcUse {
                    name: process.name.clone(),
                    cpu: process.cpu,
                    memory: process.memory,
                }),
            }
        }
        totals.sort_by(|a, b| b.cpu.total_cmp(&a.cpu));
        let counts = WalkCounts {
            processes: table.len(),
            cached: self.walker.cached(),
            walked,
            environ_reads,
        };
        (totals, counts)
    }
}

/// Reads the process table with `sysinfo`.
pub(crate) struct SystemTable {
    system: System,
}

impl SystemTable {
    pub(crate) fn new() -> Self {
        Self {
            system: System::new(),
        }
    }
}

impl ProcessEnv for SystemTable {
    /// Only `POD_ROLE`, `POD_PRODUCT` and `POD_SPEC` leave this function: it drops every
    /// other variable as it reads it.
    fn environ(&mut self, pid: u32) -> Option<Vec<(String, String)>> {
        let pid = Pid::from_u32(pid);
        // A throwaway `System`: the full environment does not stay in Harold's memory after
        // this call.
        let mut system = System::new();
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid]),
            true,
            ProcessRefreshKind::nothing().with_environ(sysinfo::UpdateKind::Always),
        );
        let process = system.process(pid)?;
        Some(
            process
                .environ()
                .iter()
                .filter_map(|entry| {
                    let (key, value) = entry.to_str()?.split_once('=')?;
                    matches!(key, "POD_ROLE" | "POD_PRODUCT" | "POD_SPEC")
                        .then(|| (key.to_owned(), value.to_owned()))
                })
                .collect(),
        )
    }
}

impl ProcessSource for SystemTable {
    fn processes(&mut self) -> Vec<ProcInfo> {
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing()
                .with_cpu()
                .with_memory()
                .with_cwd(sysinfo::UpdateKind::OnlyIfNotSet),
        );
        let cores = self.system.cpus().len().max(1) as f64;
        // The CPU list is empty until it is refreshed once.
        if self.system.cpus().is_empty() {
            self.system.refresh_cpu_usage();
        }
        self.system
            .processes()
            .values()
            .map(|process| ProcInfo {
                pid: process.pid().as_u32(),
                parent: process.parent().map(Pid::as_u32),
                start: process.start_time(),
                name: process.name().to_string_lossy().into_owned(),
                cpu: f64::from(process.cpu_usage()) / 100.0 / cores,
                memory: process.memory(),
                cwd: process.cwd().map(Path::to_path_buf),
            })
            .collect()
    }
}

/// The most agents that the alert line names, and the most processes of each.
const TOP_AGENTS: usize = 3;
const TOP_AGENT_PROCESSES: usize = 2;
/// The most characters of a process name in the alert line.
const NAME_CUT: usize = 40;

/// The name of an agent in the alert line.
fn agent_text(owner: &Owner) -> String {
    match owner.role {
        NONE => "no agent".to_owned(),
        CHIEF => CHIEF.to_owned(),
        OTHER => "other agent".to_owned(),
        role => format!("{} {}", role.replace('_', " "), owner.label),
    }
}

/// The `top_agents` text of the alert line for `metric`: the 3 agents with the highest mean
/// over the window, each with its 2 processes with the highest mean, as
/// `pnem harold 4% (claude 4%) · no agent 12% (launchd 10%)`. Memory and swap rank by memory.
/// A mean is the sum over the readings of the window divided by their number, so a reading
/// where an agent is absent counts as 0. Also returns whether an agent that is not `none`
/// is named.
fn top_agents(
    window: &VecDeque<(OffsetDateTime, Vec<AgentTotal>)>,
    metric: Metric,
    total_memory: u64,
) -> (String, bool) {
    if window.is_empty() {
        return (String::new(), false);
    }
    let readings = window.len() as f64;
    let value = |cpu: f64, memory: u64| match metric {
        Metric::Cpu => cpu,
        Metric::Memory | Metric::Swap => share(memory, total_memory),
    };
    let mut agents: HashMap<&Owner, (f64, HashMap<&str, f64>)> = HashMap::new();
    for (_, totals) in window {
        for total in totals {
            let (sum, processes) = agents.entry(&total.owner).or_default();
            *sum += value(total.cpu, total.memory);
            for process in &total.processes {
                *processes.entry(process.name.as_str()).or_default() +=
                    value(process.cpu, process.memory);
            }
        }
    }
    let ranked = |mut items: Vec<(String, f64, bool)>| {
        items.retain(|(_, sum, _)| *sum > 0.0);
        items.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        items
    };
    let by_mean = ranked(
        agents
            .iter()
            .map(|(owner, (sum, _))| (agent_text(owner), *sum, owner.role != NONE))
            .collect(),
    );
    let mut named = false;
    let mut parts = Vec::new();
    for (text, sum, is_agent) in by_mean.into_iter().take(TOP_AGENTS) {
        named |= is_agent;
        let names = agents
            .iter()
            .filter(|(owner, _)| agent_text(owner) == text)
            .flat_map(|(_, (_, processes))| processes.iter())
            .map(|(name, sum)| (name.chars().take(NAME_CUT).collect::<String>(), *sum, false))
            .collect();
        let names = ranked(names)
            .into_iter()
            .take(TOP_AGENT_PROCESSES)
            .map(|(name, sum, _)| format!("{name} {:.0}%", sum / readings * 100.0))
            .collect::<Vec<_>>()
            .join(", ");
        parts.push(format!("{text} {:.0}% ({names})", sum / readings * 100.0));
    }
    (parts.join(" · "), named)
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
    /// The use of each agent in the same readings.
    agent_window: VecDeque<(OffsetDateTime, Vec<AgentTotal>)>,
    spells: [Option<Spell>; 3],
    snapshot: Arc<Mutex<Snapshot>>,
    /// The reason of the reading failure that has been logged since a reading last worked.
    failing: Option<&'static str>,
    /// Gives each process to its agent. None until `set_agent_use`.
    agent_use: Option<Arc<Mutex<AgentUse>>>,
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
            agent_window: VecDeque::new(),
            spells: [None; 3],
            snapshot,
            failing: None,
            agent_use: None,
        }
    }

    /// Turns on the agent gauges.
    pub(crate) fn set_agent_use(&mut self, agent_use: AgentUse) {
        self.agent_use = Some(Arc::new(Mutex::new(agent_use)));
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
                let mut snapshot = lock(&self.snapshot);
                snapshot.reading = None;
                snapshot.agents.clear();
            }
        }
    }

    /// Gives each process to its agent and returns the total of each agent. Empty when the
    /// agent gauges are off.
    async fn read_agents(&self) -> Vec<AgentTotal> {
        let Some(agent_use) = self.agent_use.as_ref().map(Arc::clone) else {
            return Vec::new();
        };
        let read = tokio::task::spawn_blocking(move || lock(&agent_use).read()).await;
        let Ok((totals, counts)) = read else {
            return Vec::new();
        };
        debug!(
            processes = counts.processes,
            cached = counts.cached,
            walked = counts.walked,
            environ_reads = counts.environ_reads,
            "agent use walked"
        );
        totals
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
        let agents = self.read_agents().await;
        self.agent_window.push_back((now, agents.clone()));
        while self
            .agent_window
            .front()
            .is_some_and(|(at, _)| now - *at > MEAN_WINDOW)
        {
            self.agent_window.pop_front();
        }
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
            snapshot.agents = agents;
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
            let total_memory = reading.memory_used.saturating_add(reading.memory_free);
            let (top_agents, named) = top_agents(&self.agent_window, metric, total_memory);
            if named {
                telemetry::feature_used("agent-resource-use");
            }
            warn!(
                metric = metric.as_str(),
                value,
                normal_top = range.top,
                days = range.days,
                top_processes = %top_processes,
                top_agents = %top_agents,
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

/// Starts reading the machine, with the hourly use file at `file`. The agent monitor of
/// `snapshots` names the live agents.
pub(crate) fn start(
    file: PathBuf,
    snapshots: AgentSnapshotHub,
    shutdown: watch::Receiver<()>,
) -> JoinHandle<()> {
    let mut stats = MachineStats::new(
        SystemReader::new(),
        file,
        gauge_snapshot(),
        OffsetDateTime::now_utc().to_offset(telemetry::local_offset()),
    );
    let agents: AgentSource = Arc::new(move || {
        snapshots
            .current()
            .panes
            .iter()
            .map(|pane| AgentProc {
                pid: pane.pane.incarnation.agent_pid,
                start_ms: pane.pane.incarnation.agent_started_at_ms,
                pane_id: pane.pane.incarnation.pane_id.clone(),
                folder: pane.pane.working_directory.clone(),
            })
            .collect()
    });
    stats.set_agent_use(AgentUse::new(
        Box::new(SystemTable::new()),
        agents,
        Arc::new(crate::pod_roles::installed),
    ));
    tokio::spawn(run(stats, shutdown))
}

#[cfg(test)]
#[path = "machine_stats_tests.rs"]
mod tests;
