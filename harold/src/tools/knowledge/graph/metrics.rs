//! The graph's two gauges, `harold.knowledge.graph_waiting` and `harold.knowledge.typed_relations`.
//! The graph writer is the only writer of either number, so it publishes them after each
//! change and once a minute, and the gauges report them at every export while the graph is
//! on. Reading them never wakes TypeDB.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

static ON: AtomicBool = AtomicBool::new(false);
static WAITING: AtomicU64 = AtomicU64::new(0);
static TYPED: AtomicU64 = AtomicU64::new(0);
static REGISTERED: OnceLock<[opentelemetry::metrics::ObservableGauge<u64>; 2]> = OnceLock::new();

fn register() {
    REGISTERED.get_or_init(|| {
        let meter = crate::telemetry::meter();
        let gauge = |name: &'static str, unit: &'static str, value: &'static AtomicU64| {
            meter
                .u64_observable_gauge(name)
                .with_unit(unit)
                .with_callback(move |observer| {
                    if ON.load(Ordering::Relaxed) {
                        observer.observe(value.load(Ordering::Relaxed), &[]);
                    }
                })
                .build()
        };
        [
            gauge("harold.knowledge.graph_waiting", "{record}", &WAITING),
            gauge("harold.knowledge.typed_relations", "{relation}", &TYPED),
        ]
    });
}

#[cfg(test)]
thread_local! {
    /// What this thread published, so a test sees its own writer and no other test's.
    pub(crate) static PUBLISHED: std::cell::RefCell<Vec<(usize, Option<usize>)>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Records the numbers the gauges report from now on.
pub(crate) fn publish(waiting: usize, typed_relations: Option<usize>) {
    #[cfg(test)]
    PUBLISHED.with(|published| published.borrow_mut().push((waiting, typed_relations)));
    register();
    WAITING.store(waiting as u64, Ordering::Relaxed);
    if let Some(typed) = typed_relations {
        TYPED.store(typed as u64, Ordering::Relaxed);
    }
    ON.store(true, Ordering::Relaxed);
}
