//! The evidence line to the chief: one `EVIDENCE` line for each spec whose evidence file
//! says `missing` and has no `told:` line. After the line is typed, the file gets `told:`.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;

use time::{OffsetDateTime, UtcOffset};
use tracing::{info, warn};

use super::files::{add_told_once, read_evidence};
use super::state::{QueryResult, State};
use crate::pod_stats::{POD_FOLDER, Product};
use crate::telemetry;

/// How one try to type the line ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LineResult {
    Typed,
    /// No chief pane was found. The line waits.
    NoChief,
    /// The chief was found and nothing was typed. Holds the reason, for example `pane_busy`.
    NotTyped(&'static str),
}

/// Types one line into the chief's pane. It never waits for a busy pane: the caller tries
/// again at the next check.
pub(crate) trait ChiefLine: Send + Sync {
    fn type_line<'a>(
        &'a self,
        line: &'a str,
    ) -> Pin<Box<dyn Future<Output = LineResult> + Send + 'a>>;
}

/// The port of a host with no chief: each line waits.
pub(crate) struct NoChief;

impl ChiefLine for NoChief {
    fn type_line<'a>(
        &'a self,
        _line: &'a str,
    ) -> Pin<Box<dyn Future<Output = LineResult> + Send + 'a>> {
        Box::pin(async { LineResult::NoChief })
    }
}

/// `EVIDENCE <product> <id> missing: <query> (<reason>)[, <query> (<reason>)]`. It holds
/// names and fixed reason words only.
pub(crate) fn evidence_line(product: &str, id: &str, absent: &[QueryResult]) -> String {
    let queries: Vec<String> = absent
        .iter()
        .map(|result| format!("{} ({})", result.name, result.reason))
        .collect();
    format!("EVIDENCE {product} {id} missing: {}", queries.join(", "))
}

/// The ids of the evidence files in `pod`, in order.
fn evidence_ids(pod: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(pod.join("evidence")) else {
        return Vec::new();
    };
    let mut ids: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            name.strip_suffix(".md")
                .filter(|id| !id.starts_with('.'))
                .map(str::to_string)
        })
        .collect();
    ids.sort();
    ids
}

/// Types the line for each `missing` spec of `product` that has no `told:` line, then adds
/// `told:` to its file. A line that is not typed leaves the file as it is, so the next call
/// tries again.
pub(crate) async fn tell_missing(
    chief: &dyn ChiefLine,
    product: &Product,
    now: OffsetDateTime,
    offset: UtcOffset,
) {
    let pod = product.repo.join(POD_FOLDER);
    for id in evidence_ids(&pod) {
        let Some(file) = read_evidence(&pod, &id) else {
            continue;
        };
        if file.state != State::Missing || file.is_told() {
            continue;
        }
        let absent: Vec<QueryResult> = file
            .results
            .iter()
            .filter(|result| !result.present)
            .cloned()
            .collect();
        let line = evidence_line(&product.name, &id, &absent);
        match chief.type_line(&line).await {
            LineResult::Typed => {
                telemetry::count_evidence_line("typed");
                telemetry::feature_used("evidence-follow-up");
                info!(spec.id = %id, "evidence line typed");
                if let Err(error) = add_told_once(&pod, &id, now.to_offset(offset)) {
                    warn!(spec.id = %id, %error, "evidence told line not written");
                }
            }
            LineResult::NoChief => not_typed(&id, "no_chief", "no_chief"),
            LineResult::NotTyped(reason) => not_typed(&id, "not_typed", reason),
        }
    }
}

fn not_typed(id: &str, outcome: &'static str, reason: &'static str) {
    telemetry::count_evidence_line(outcome);
    warn!(spec.id = %id, reason, "evidence line not typed");
}
