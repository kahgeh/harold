//! Publishes the `merged_not_released` list of work waiting every 10 minutes. The pod
//! stats module publishes the other three lists on gauges of the same names. This module
//! adds its own series, with `list = merged_not_released`, on instruments of those names.
//! It only reads: it runs `ReleaseToolService::products`.

use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::metrics::ObservableGauge;

use time::{OffsetDateTime, UtcOffset};
use tokio::sync::watch;
use tokio::time::{MissedTickBehavior, interval};
use tracing::{info, warn};

use super::gather::{Reach, ReachError, reach_for};
use super::service::ReleaseToolService;
use crate::pod_stats::{
    Git, INVENTORY_ITEM_DAYS, INVENTORY_ITEMS, INVENTORY_OLDEST_HOURS, ListStats, list_stats,
    read_registry,
};

#[cfg(test)]
thread_local! {
    /// How many times this thread published, so a test counts its own task.
    pub(super) static PUBLISHES: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    /// How many publishes this thread finished, on every way out of `publish`.
    pub(super) static PUBLISHES_DONE: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Raises `PUBLISHES_DONE` when a publish ends, early return included.
#[cfg(test)]
struct PublishDone;

#[cfg(test)]
impl Drop for PublishDone {
    fn drop(&mut self) {
        PUBLISHES_DONE.with(|count| count.set(count.get() + 1));
    }
}

pub(super) const PERIOD: Duration = Duration::from_secs(600);

/// The value of the `list` attribute of this module's series.
const LIST: &str = "merged_not_released";

/// The stats of the list of each product that has a read. A paused product, a product
/// with no git answer and a product with no release tag have no entry.
static STORE: Mutex<BTreeMap<String, Entry>> = Mutex::new(BTreeMap::new());

/// The stats of one product and the registry that listed it. A publish drops only the
/// products of its own registry, so two services never drop each other's series.
struct Entry {
    registry: Option<PathBuf>,
    stats: ListStats,
}

static REGISTERED: OnceLock<(ObservableGauge<u64>, [ObservableGauge<f64>; 2])> = OnceLock::new();

fn store() -> MutexGuard<'static, BTreeMap<String, Entry>> {
    STORE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn attributes(product: &str) -> [KeyValue; 2] {
    [
        KeyValue::new("pod.product", product.to_string()),
        KeyValue::new("list", LIST),
    ]
}

/// Builds the three gauges, once. They report every product in the store at each export.
pub(super) fn register() {
    REGISTERED.get_or_init(|| {
        let meter = crate::telemetry::meter();
        let items = meter
            .u64_observable_gauge(INVENTORY_ITEMS.0)
            .with_unit(INVENTORY_ITEMS.1)
            .with_callback(|observer| {
                for (product, Entry { stats, .. }) in store().iter() {
                    observer.observe(stats.items, &attributes(product));
                }
            })
            .build();
        let hours = |(name, unit): (&'static str, &'static str), pick: fn(&ListStats) -> f64| {
            meter
                .f64_observable_gauge(name)
                .with_unit(unit)
                .with_callback(move |observer| {
                    for (product, Entry { stats, .. }) in store().iter() {
                        observer.observe(pick(stats), &attributes(product));
                    }
                })
                .build()
        };
        (
            items,
            [
                hours(INVENTORY_OLDEST_HOURS, |stats| stats.oldest_hours),
                hours(INVENTORY_ITEM_DAYS, |stats| stats.item_days),
            ],
        )
    });
}

/// Reads the lists of every product once and records the `merged_not_released` stats.
pub(super) async fn publish<G: Git>(
    service: &ReleaseToolService<G>,
    now: OffsetDateTime,
    offset: UtcOffset,
) {
    #[cfg(test)]
    PUBLISHES.with(|count| count.set(count.get() + 1));
    #[cfg(test)]
    let _done = PublishDone;
    // A failed read keeps the last numbers: it is not a statement that the list is empty.
    let Ok(products) = service.products("", now, offset).await else {
        return;
    };
    let registry = service.registry().map(PathBuf::from);
    let mut store = store();
    // A product that left the registry ends its series.
    store.retain(|name, entry| {
        entry.registry != registry || products.iter().any(|product| &product.name == name)
    });
    for product in products {
        // A product with no release tag has no release to wait for. Its list holds the last
        // 14 days of merges for the command only, so the gauge leaves it out.
        if product.paused || product.unavailable.is_some() || product.release.is_empty() {
            store.remove(&product.name);
            continue;
        }
        let ages: Vec<_> = product.merged_at.iter().map(|at| now - *at).collect();
        let stats = list_stats(&ages);
        store.insert(
            product.name,
            Entry {
                registry: registry.clone(),
                stats,
            },
        );
    }
}

/// The file of counted tags beside the pod stats tips file. A tool's data folder is
/// `<store folder>/tools/release`, so the file is `<store folder>/pod/release-tips`.
pub(super) fn tips_file(data_folder: &Path) -> PathBuf {
    data_folder
        .parent()
        .and_then(Path::parent)
        .unwrap_or(Path::new("."))
        .join("pod")
        .join("release-tips")
}

/// What `release-tips` holds for one product.
enum Counted {
    /// No line for the product.
    Nothing,
    /// A line that does not read as `<product> <tag>`, or a file that cannot be read.
    Unreadable,
    Tag(String),
}

/// The counted tags in `file`. A missing file holds none.
fn read_tips(file: &Path, product: &str) -> Counted {
    let text = match std::fs::read_to_string(file) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Counted::Nothing,
        Err(_) => return Counted::Unreadable,
    };
    for line in text.lines() {
        let mut words = line.split_whitespace();
        if words.next() != Some(product) {
            continue;
        }
        return match (words.next(), words.next()) {
            (Some(tag), None) => Counted::Tag(tag.to_owned()),
            _ => Counted::Unreadable,
        };
    }
    Counted::Nothing
}

/// Saves `tag` as the counted tag of `product`, keeping the lines of the other products.
/// The file is written to a temporary file beside it and renamed, for the user only. The
/// line of a product that cannot be read is dropped.
fn save_tip(file: &Path, product: &str, tag: &str) -> std::io::Result<()> {
    let old = std::fs::read_to_string(file).unwrap_or_default();
    let mut counted: BTreeMap<String, String> = old
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            Some((words.next()?.to_owned(), words.next()?.to_owned()))
        })
        .collect();
    counted.insert(product.to_owned(), tag.to_owned());
    if let Some(folder) = file.parent() {
        std::fs::create_dir_all(folder)?;
    }
    let text: String = counted
        .iter()
        .map(|(product, tag)| format!("{product} {tag}\n"))
        .collect();
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

/// Records `merged_to_released` for the specs that the newest tag of each product reaches
/// for the first time. The new tag is saved to `tips` first (ADR 0003), and the values are
/// recorded after: a failed write records nothing, and a later read tries the same tag.
pub(super) async fn record_releases<G: Git>(
    service: &ReleaseToolService<G>,
    tips: &Path,
    now: OffsetDateTime,
    offset: UtcOffset,
) {
    let Some(registry) = service.registry() else {
        return;
    };
    let Ok(products) = read_registry(registry) else {
        return;
    };
    for product in products.iter().filter(|product| !product.paused) {
        let counted = read_tips(tips, &product.name);
        let tag = match &counted {
            Counted::Tag(tag) => Some(tag.as_str()),
            Counted::Nothing | Counted::Unreadable => None,
        };
        let reach = match reach_for(service.git(), product, tag, offset, now).await {
            Ok(Some(reach)) => reach,
            Ok(None) | Err(ReachError::Unavailable(_)) => continue,
            Err(ReachError::MissingTag) => {
                // The counted tag is gone: start again from the newest tag.
                let Ok(Some(reach)) = reach_for(service.git(), product, None, offset, now).await
                else {
                    continue;
                };
                if save_tip(tips, &product.name, &reach.tag).is_ok() {
                    warn!(pod.product = %product.name, reason = "missing_tag", "release tips reset");
                }
                continue;
            }
        };
        match counted {
            Counted::Nothing => {
                let _ = save_tip(tips, &product.name, &reach.tag);
            }
            Counted::Unreadable => {
                if save_tip(tips, &product.name, &reach.tag).is_ok() {
                    warn!(pod.product = %product.name, reason = "unreadable", "release tips reset");
                }
            }
            Counted::Tag(counted) if counted == reach.tag => {}
            Counted::Tag(_) => {
                if save_tip(tips, &product.name, &reach.tag).is_err() {
                    continue;
                }
                let specs = record_merges(&product.name, &reach);
                info!(pod.product = %product.name, tag = %reach.tag, specs, "release phase recorded");
            }
        }
    }
}

/// Records one value for each reached spec and returns how many it recorded. A spec whose
/// wait is negative, and every spec of a tag with no readable time, is skipped.
fn record_merges(product: &str, reach: &Reach) -> usize {
    let Some(tag_time) = reach.tag_time else {
        return 0;
    };
    let mut recorded = 0;
    for spec in &reach.merges {
        let seconds = (tag_time - spec.merged_at).as_seconds_f64();
        if seconds < 0.0 {
            continue;
        }
        crate::telemetry::instruments().pod_spec_lead_time.record(
            seconds,
            &[
                KeyValue::new("pod.product", product.to_owned()),
                KeyValue::new("phase", "merged_to_released"),
                KeyValue::new("spec.revisions", i64::from(spec.revisions)),
                KeyValue::new("spec.id", spec.id.clone()),
            ],
        );
        recorded += 1;
    }
    recorded
}

/// Publishes now and then every `period`, until shutdown or until the sender is dropped.
pub(super) async fn run<G: Git>(
    service: ReleaseToolService<G>,
    tips: PathBuf,
    period: Duration,
    mut shutdown: watch::Receiver<()>,
) {
    let mut ticks = interval(period);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => return,
            _ = ticks.tick() => {}
        }
        // Shutdown also drops a publish under way: it only reads.
        tokio::select! {
            biased;
            _ = shutdown.changed() => return,
            () = async {
                let offset = crate::telemetry::local_offset();
                publish(&service, OffsetDateTime::now_utc(), offset).await;
                record_releases(&service, &tips, OffsetDateTime::now_utc(), offset).await;
            } => {}
        }
    }
}

#[cfg(test)]
#[path = "inventory_tests.rs"]
mod inventory_tests;
