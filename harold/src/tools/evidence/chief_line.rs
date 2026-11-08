//! The evidence line to the chief: one `EVIDENCE` line for each spec whose evidence file
//! says `missing` and has no `told:` line. After the line is typed, the file gets `told:`.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use time::{OffsetDateTime, UtcOffset};
use tracing::{info, warn};

use super::files::{add_told_once, read_evidence};
use super::state::{QueryResult, State};
use crate::outbound::left_out::{ChiefLookup, resolve_chief};
use crate::pod_stats::{POD_FOLDER, Product, read_registry};
use crate::restart_reminder::TurnEnds;
use crate::telemetry;
use crate::timed_prompts::{AgentPane, LineOutcome, Ports, Writer, type_line_once};

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
    // The daily pass and a chief turn end may meet: one at a time, so no line goes twice.
    static ONE_AT_A_TIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _one = ONE_AT_A_TIME.lock().await;
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

/// How many times, and how far apart, the line is tried while the pane reads busy.
const IDLE_TRIES: u32 = 6;
const IDLE_RETRY: std::time::Duration = std::time::Duration::from_millis(500);

/// Types the line into the chief's pane through the writer of the tools,
/// `Writer::Tool("evidence")`, and the chief lookup of the restart reminder.
pub(crate) struct TmuxChiefLine<P> {
    ports: Arc<P>,
}

impl<P: Ports> TmuxChiefLine<P> {
    pub(crate) fn new(ports: Arc<P>) -> Self {
        Self { ports }
    }

    /// The chief's pane id, or the result that says why there is none.
    fn chief_pane(&self) -> Result<String, LineResult> {
        let view = self.ports.agents();
        if view.listing_failed {
            return Err(LineResult::NotTyped("pane_busy"));
        }
        let chief = self.ports.chief();
        match resolve_chief(
            view.panes.iter().map(AgentPane::position),
            chief.folder,
            chief.session,
        ) {
            ChiefLookup::Found(pane_id) => Ok(pane_id),
            ChiefLookup::None => Err(LineResult::NoChief),
            ChiefLookup::Ambiguous { .. } => Err(LineResult::NotTyped("several_agents_in_folder")),
        }
    }
}

impl<P: Ports> ChiefLine for TmuxChiefLine<P> {
    fn type_line<'a>(
        &'a self,
        line: &'a str,
    ) -> Pin<Box<dyn Future<Output = LineResult> + Send + 'a>> {
        Box::pin(async move {
            let pane_id = match self.chief_pane() {
                Ok(pane_id) => pane_id,
                Err(result) => return result,
            };
            // Harold's snapshot shows a pane idle only a moment after the turn end.
            let mut outcome = LineOutcome::PaneBusy("pane_busy");
            for attempt in 0..IDLE_TRIES {
                if attempt > 0 {
                    tokio::time::sleep(IDLE_RETRY).await;
                }
                outcome =
                    type_line_once(&*self.ports, Writer::Tool("evidence"), &pane_id, line).await;
                if !matches!(outcome, LineOutcome::PaneBusy(_)) {
                    break;
                }
            }
            match outcome {
                LineOutcome::Typed { .. } => LineResult::Typed,
                LineOutcome::PaneBusy(reason) => LineResult::NotTyped(reason),
                LineOutcome::NoPane => LineResult::NotTyped("no_pane"),
                LineOutcome::Failed => LineResult::NotTyped("type_failed"),
            }
        })
    }
}

/// At the end of a turn of the chief, it types each line that waits. It passes every turn
/// end on to `next`, the hook that was there before it.
pub(crate) struct EvidenceTurnEnds<P> {
    chief: Arc<TmuxChiefLine<P>>,
    registry: Option<PathBuf>,
    next: Option<Arc<dyn TurnEnds>>,
}

impl<P: Ports> EvidenceTurnEnds<P> {
    pub(crate) fn new(
        ports: Arc<P>,
        registry: Option<PathBuf>,
        next: Option<Arc<dyn TurnEnds>>,
    ) -> Self {
        Self {
            chief: Arc::new(TmuxChiefLine::new(ports)),
            registry,
            next,
        }
    }

    /// Types the waiting lines when `pane_id` is the chief's pane.
    pub(crate) async fn on_turn_end(&self, pane_id: &str, _session_id: &str, _transcript: &str) {
        if !matches!(self.chief.chief_pane(), Ok(chief) if chief == pane_id) {
            return;
        }
        let Some(registry) = &self.registry else {
            return;
        };
        let Ok(products) = read_registry(registry) else {
            return;
        };
        let now = OffsetDateTime::now_utc();
        for product in products.iter().filter(|product| !product.paused) {
            tell_missing(&*self.chief, product, now, telemetry::local_offset()).await;
        }
    }
}

impl<P: Ports> TurnEnds for EvidenceTurnEnds<P> {
    fn turn_ended(&self, pane_id: &str, session_id: &str, transcript_path: &str) {
        if let Some(next) = &self.next {
            next.turn_ended(pane_id, session_id, transcript_path);
        }
        let this = Self {
            chief: Arc::clone(&self.chief),
            registry: self.registry.clone(),
            next: None,
        };
        let (pane_id, session_id, transcript_path) = (
            pane_id.to_string(),
            session_id.to_string(),
            transcript_path.to_string(),
        );
        tokio::spawn(async move {
            this.on_turn_end(&pane_id, &session_id, &transcript_path)
                .await;
        });
    }
}
