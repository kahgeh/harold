//! Publishes the `merged_not_released` list of work waiting every 10 minutes. The pod
//! stats module publishes the other three lists on gauges of the same names. This module
//! adds its own series, with `list = merged_not_released`, on instruments of those names.
//! It only reads: it runs `ReleaseToolService::products`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::metrics::ObservableGauge;

use time::{OffsetDateTime, UtcOffset};
use tokio::sync::watch;
use tokio::time::{MissedTickBehavior, interval};

use super::service::ReleaseToolService;
use crate::pod_stats::{
    Git, INVENTORY_ITEM_DAYS, INVENTORY_ITEMS, INVENTORY_OLDEST_HOURS, ListStats, list_stats,
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

/// The stats of the list of each product that has a read. A paused product and a product
/// with no git answer have no entry.
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
        if product.paused || product.unavailable.is_some() {
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

/// Publishes now and then every `period`, until shutdown or until the sender is dropped.
pub(super) async fn run<G: Git>(
    service: ReleaseToolService<G>,
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
            () = publish(&service, OffsetDateTime::now_utc(), crate::telemetry::local_offset()) => {}
        }
    }
}

#[cfg(test)]
#[path = "inventory_tests.rs"]
mod inventory_tests;
