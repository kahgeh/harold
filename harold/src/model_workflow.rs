//! The model workflows of Harold: each place in Harold's code that runs a model.
//!
//! Each model call ends with `record`. It adds the tokens to `harold.workflow.tokens` and
//! the cost, in AUD, to `harold.workflow.cost`. `CALL_SITES` lists every model workflow,
//! and a test fails when a file runs a model and no entry names it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use opentelemetry::KeyValue;

use crate::settings::PricesSettings;
use crate::telemetry::{TurnTokens, counted_model, instruments};

/// The `pod.product` of a call that serves no product.
const NO_PRODUCT: &str = "harold";
/// The label of a call with no spec, and of a seed.
const NONE: &str = "none";
/// The token kinds of `harold.workflow.tokens`.
const KINDS: [&str; 4] = ["input", "output", "cache_read", "cache_write"];

/// One model workflow and its workflow step.
pub(crate) struct CallSite {
    /// The `workflow` attribute.
    pub workflow: &'static str,
    /// The `workflow.step` attribute.
    pub step: &'static str,
    /// The file that runs the model.
    pub model_file: &'static str,
    /// The file that calls `record` for this model workflow.
    pub record_file: &'static str,
}

/// Every model workflow of Harold's code. The model workflow of the restart check is not
/// here yet: its stage adds the entry with its `record` call.
pub(crate) const CALL_SITES: &[CallSite] = &[
    CallSite {
        workflow: "watch",
        step: "run",
        model_file: "tools/watch/runner.rs",
        record_file: "tools/watch/runner.rs",
    },
    CallSite {
        workflow: "call_answer",
        step: "answer",
        model_file: "voice_input/answerer.rs",
        record_file: "voice_input/answerer.rs",
    },
    CallSite {
        workflow: "turn_end_update",
        step: "update",
        model_file: "voice_input/answerer.rs",
        record_file: "voice_input/mac.rs",
    },
    CallSite {
        workflow: "activity_summary",
        step: "summarize",
        model_file: "activity_summary.rs",
        record_file: "activity_summary.rs",
    },
    CallSite {
        workflow: "notification_summary",
        step: "summarize",
        model_file: "channels/mod.rs",
        record_file: "channels/mod.rs",
    },
    CallSite {
        workflow: "inbound_routing",
        step: "classify",
        model_file: "inbound/mod.rs",
        record_file: "inbound/mod.rs",
    },
];

/// The cost in AUD of one model call. A reported cost in USD wins. Otherwise the cost is
/// the tokens times the list prices of `model`. A model with neither gives `None`.
pub(crate) fn cost_aud(
    prices: &PricesSettings,
    model: &str,
    tokens: &TurnTokens,
    reported_cost_usd: Option<f64>,
) -> Option<f64> {
    let usd = match reported_cost_usd {
        Some(usd) => usd,
        None => {
            let list = prices.list.get(model)?;
            let per_million = [
                (tokens.input, list.input),
                (tokens.output, list.output),
                (tokens.cache_read, list.cache_read),
                (tokens.cache_write, list.cache_write),
            ];
            per_million
                .iter()
                .map(|(count, price)| *count as f64 * price / 1_000_000.0)
                .sum()
        }
    };
    Some(usd * prices.exchange_rate_usd_to_aud)
}

/// The product that a folder serves: the product with the longest `match` path that holds
/// the folder, or `harold`.
pub(crate) fn product_for<'a>(
    folder: Option<&Path>,
    registry: &'a [(String, Vec<PathBuf>)],
) -> &'a str {
    let Some(folder) = folder else {
        return NO_PRODUCT;
    };
    registry
        .iter()
        .flat_map(|(name, paths)| paths.iter().map(move |path| (name, path)))
        .filter(|(_, path)| folder.starts_with(path))
        .max_by_key(|(_, path)| path.components().count())
        .map_or(NO_PRODUCT, |(name, _)| name.as_str())
}

/// The models already logged as unpriced, with the day of the line.
#[derive(Default)]
struct UnpricedLog(BTreeMap<String, i64>);

impl UnpricedLog {
    /// Whether to log `model` on `day` (days since the epoch): once a day for each model.
    fn should_log(&mut self, model: &str, day: i64) -> bool {
        if self.0.get(model) == Some(&day) {
            return false;
        }
        self.0.insert(model.to_owned(), day);
        true
    }
}

#[derive(Default)]
struct State {
    prices: PricesSettings,
    registry: Vec<(String, Vec<PathBuf>)>,
    unpriced: UnpricedLog,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

/// Sets the list prices and the `match` paths that `record` reads. `main` calls it once at
/// start.
pub(crate) fn configure(prices: PricesSettings, registry: Vec<(String, Vec<PathBuf>)>) {
    let mut state = STATE.lock().unwrap_or_else(PoisonError::into_inner);
    let unpriced = state.take().map(|old| old.unpriced).unwrap_or_default();
    *state = Some(State {
        prices,
        registry,
        unpriced,
    });
}

/// Adds 0 to each series of both counters for each pair of `CALL_SITES`, so a missing
/// series means a broken `record` call and not a quiet day.
pub(crate) fn register_workflow_counters() {
    let instruments = instruments();
    for site in CALL_SITES {
        let base = [
            KeyValue::new("workflow", site.workflow),
            KeyValue::new("workflow.step", site.step),
            KeyValue::new("spec.id", NONE),
            KeyValue::new("model", NONE),
            KeyValue::new("pod.product", NO_PRODUCT),
        ];
        instruments.workflow_cost.add(0.0, &base);
        for kind in KINDS {
            let mut attributes = base.to_vec();
            attributes.push(KeyValue::new("kind", kind));
            instruments.workflow_tokens.add(0, &attributes);
        }
    }
}

/// Records one model call: its tokens and its cost. `folder` is the folder that the call
/// serves, when it has one, and gives `pod.product`.
pub(crate) fn record(
    workflow: &str,
    step: &str,
    spec: Option<&str>,
    folder: Option<&Path>,
    model: &str,
    usage: &TurnTokens,
    reported_cost_usd: Option<f64>,
) {
    let model = counted_model(model);
    let (cost, product, log_unpriced) = {
        let mut guard = STATE.lock().unwrap_or_else(PoisonError::into_inner);
        let state = guard.get_or_insert_with(State::default);
        let cost = cost_aud(&state.prices, model, usage, reported_cost_usd);
        let product = product_for(folder, &state.registry).to_owned();
        let log_unpriced =
            cost.is_none() && !usage.is_empty() && state.unpriced.should_log(model, today());
        (cost, product, log_unpriced)
    };
    if log_unpriced {
        tracing::warn!(model, workflow, "model unpriced");
    }
    let base = [
        KeyValue::new("workflow", workflow.to_owned()),
        KeyValue::new("workflow.step", step.to_owned()),
        KeyValue::new("spec.id", spec.unwrap_or(NONE).to_owned()),
        KeyValue::new("model", model.to_owned()),
        KeyValue::new("pod.product", product),
    ];
    let instruments = instruments();
    let counts = [
        ("input", usage.input),
        ("output", usage.output),
        ("cache_read", usage.cache_read),
        ("cache_write", usage.cache_write),
    ];
    for (kind, count) in counts {
        let mut attributes = base.to_vec();
        attributes.push(KeyValue::new("kind", kind));
        instruments.workflow_tokens.add(count, &attributes);
    }
    if let Some(cost) = cost {
        instruments.workflow_cost.add(cost, &base);
    }
}

/// The UTC day number, for the once-a-day line.
fn today() -> i64 {
    time::OffsetDateTime::now_utc()
        .unix_timestamp()
        .div_euclid(86_400)
}

#[cfg(test)]
#[path = "model_workflow_tests.rs"]
mod tests;
