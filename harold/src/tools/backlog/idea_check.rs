//! The idea check: which backlog items have a spec yet. It tells pod stats through
//! `IdeaSignals`, which owns the instruments. It finds a spec through `read_specs`, the one
//! reader of the pod folder, and reads nothing there itself.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use time::{OffsetDateTime, UtcOffset};
use tokio::sync::watch;
use tokio::time::{MissedTickBehavior, interval};
use tracing::{info, warn};

use super::store::BacklogStore;
use crate::pod_stats::{IdeaSignals, POD_FOLDER, Sources, read_registry, read_specs};

/// The time between two checks, as the release tool's list.
pub(super) const PERIOD: Duration = Duration::from_secs(600);

/// The key in `failed` for a registry that cannot be read.
const REGISTRY_KEY: &str = "";

/// Runs the check every `PERIOD` until a shutdown signal, or until the sender is dropped.
pub(super) async fn run<S: BacklogStore, G: IdeaSignals>(
    store: std::sync::Arc<S>,
    registry: PathBuf,
    signals: G,
    mut shutdown: watch::Receiver<()>,
) {
    let mut ticks = interval(PERIOD);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // The products whose last read failed: each logs the failure once.
    let mut failed = HashSet::new();
    loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => return,
            _ = ticks.tick() => {}
        }
        // Shutdown also drops a check under way: it only reads and counts.
        tokio::select! {
            biased;
            _ = shutdown.changed() => return,
            () = check(
                &*store,
                &registry,
                &signals,
                OffsetDateTime::now_utc(),
                crate::telemetry::local_offset(),
                &mut failed,
            ) => {}
        }
    }
}

/// Counts the items of each registered product that have no spec, and records the phase
/// `idea_to_open` for each item whose spec the check sees for the first time. A product
/// that cannot be read keeps its last count and records nothing.
pub(super) async fn check<S: BacklogStore, G: IdeaSignals>(
    store: &S,
    registry: &Path,
    signals: &G,
    now: OffsetDateTime,
    offset: UtcOffset,
    failed: &mut HashSet<String>,
) {
    let products = match read_registry(registry) {
        Ok(products) => {
            failed.remove(REGISTRY_KEY);
            products
        }
        Err(_) => {
            if failed.insert(REGISTRY_KEY.to_owned()) {
                warn!(reason = "registry_unreadable", "backlog idea check failed");
            }
            return;
        }
    };
    for product in products {
        let sources = Sources {
            busy: &HashMap::new(),
            reflog: &[],
            gone: &HashMap::new(),
            now,
            offset,
        };
        let specs = match read_specs(&product.repo.join(POD_FOLDER), &sources) {
            Ok(read) => {
                failed.remove(&product.name);
                read.specs
            }
            Err(reason) => {
                if failed.insert(product.name.clone()) {
                    warn!(pod.product = %product.name, reason, "backlog idea check failed");
                }
                continue;
            }
        };
        let Ok(times) = store.idea_times(&product.name).await else {
            continue;
        };
        let drafts: HashMap<&str, Option<OffsetDateTime>> = specs
            .iter()
            .map(|spec| (spec.id.as_str(), spec.open_at))
            .collect();
        let ideas = times
            .iter()
            .filter(|item| !drafts.contains_key(item.id.as_str()))
            .count();
        signals.set_idea_count(&product.name, u32::try_from(ideas).unwrap_or(u32::MAX));
        for item in &times {
            let Some(Some(draft)) = drafts.get(item.id.as_str()) else {
                continue;
            };
            if item.opened_at_ms.is_some() {
                continue;
            }
            let draft_ms = (draft.unix_timestamp_nanos() / 1_000_000) as i64;
            // The store keeps the first sight, so the phase is recorded once.
            if !matches!(
                store.mark_opened(&product.name, &item.id, draft_ms).await,
                Ok(true)
            ) {
                continue;
            }
            // An item put after its spec was drafted has no wait to record.
            let wait_s = (draft_ms - item.first_put_at_ms) as f64 / 1000.0;
            if wait_s < 0.0 {
                continue;
            }
            signals.record_idea_opened(&product.name, &item.id, wait_s);
            info!(pod.product = %product.name, spec.id = %item.id, wait_s, "backlog idea opened");
        }
    }
}

#[cfg(test)]
#[path = "idea_check_tests.rs"]
mod idea_check_tests;
