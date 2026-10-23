//! A hand-over: the call answerer's request for the chief, typed into the chief's pane as
//! `🎙 hand-over H1: <request>`. It types even while the chief works (`Writer::HandOver`),
//! tries again every 5 seconds for 30 minutes, also after the call ends, and then tells the
//! user in the web app's thread that it could not.
//!
//! Hand-overs stay typed: agent-message-tool, which would have carried them with a receipt
//! check, was set aside (ADR 0009, note of 2026-10-23).
//! "Typed" is all Harold knows: tmux took the keys, not that the chief read the line.
//!
//! No log line holds the request: the lines carry `hand_over_id`.

use super::CallPorts;
use super::call::{MARK, chief_pane};
use crate::agent::domain::EffectiveAgentState;
use crate::away_message::one_line;
use crate::telemetry::{self, StartedBy};
use crate::timed_prompts::{Attempt, Ports, Reason, Writer, type_when_ready};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use tokio::time::{Instant, sleep};
use tracing::{info, warn};
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// How often a hand-over that could not be typed is tried again.
pub(super) const RETRY: Duration = Duration::from_secs(5);
/// How long a hand-over is kept trying.
pub(super) const GIVE_UP: Duration = Duration::from_secs(30 * 60);
/// The longest request typed, in characters.
const REQUEST_CHARS: usize = 1000;

/// `text` without a leading `H<1 to 4 digits>:` and the spaces after it: the tag the chief
/// puts on its report so Harold can match it to a hand-over, which nobody should hear or
/// read. A tag with nothing after it, and any other line, comes back as it is.
pub(crate) fn without_hand_over_tag(text: &str) -> &str {
    let Some(rest) = text.strip_prefix('H') else {
        return text;
    };
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    if !(1..=4).contains(&digits) {
        return text;
    }
    let Some(after) = rest[digits..].strip_prefix(':') else {
        return text;
    };
    let said = after.trim_start_matches(' ');
    if said.is_empty() { text } else { said }
}

/// Where a hand-over stands, as far as Harold knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HandOverState {
    Waiting,
    Typed,
}

impl HandOverState {
    /// How the facts word it: never that the chief received or read it.
    pub(crate) fn as_fact(self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Typed => "typed into the chief's pane",
        }
    }
}

/// A hand-over as the call facts hold it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HandOverFact {
    pub id: String,
    pub request: String,
    pub state: HandOverState,
}

/// The hand-overs of one call, numbered `H1`, `H2`, … within it.
#[derive(Default, Clone)]
pub(super) struct HandOvers {
    shared: Arc<Mutex<Vec<HandOverFact>>>,
    /// Shared with the clones a run that outlives its call holds, so numbers do not repeat.
    count: Arc<AtomicU32>,
}

impl HandOvers {
    pub(super) fn facts(&self) -> Vec<HandOverFact> {
        self.shared
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Numbers the hand-over and starts typing it, in a task that outlives the call.
    /// Returns the number, such as `H1`. With the call turn's span, the chief's turn for the
    /// typed line joins that turn's trace.
    pub(super) fn start<P: CallPorts>(
        &self,
        ports: Arc<P>,
        request: &str,
        turn: Option<&tracing::Span>,
    ) -> String {
        let id = format!("H{}", self.count.fetch_add(1, Ordering::SeqCst) + 1);
        let request: String = one_line(request).chars().take(REQUEST_CHARS).collect();
        self.shared
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(HandOverFact {
                id: id.clone(),
                request: request.clone(),
                state: HandOverState::Waiting,
            });
        tokio::spawn(keep_trying(
            ports,
            Arc::clone(&self.shared),
            id.clone(),
            request,
            turn.map(OpenTelemetrySpanExt::context),
        ));
        id
    }
}

/// How one try ended.
enum Try {
    Typed {
        pane: String,
        while_busy: bool,
    },
    Wait(Reason),
    /// The text is in the box but Enter failed; typing again would repeat it.
    EnterFailed,
}

async fn try_once<P: CallPorts>(ports: &P, line: &str) -> Try {
    let Some(folder) = ports.chief_folder() else {
        return Try::Wait(Reason::NoAgentInFolder);
    };
    let panes = ports.panes();
    let view = panes.agents();
    match type_when_ready(panes, Writer::HandOver, &folder, line, &view, &[]).await {
        Attempt::Typed { pane } => {
            let while_busy = view
                .panes
                .iter()
                .any(|seen| seen.pane_id == pane && seen.state == EffectiveAgentState::Busy);
            Try::Typed { pane, while_busy }
        }
        Attempt::Wait(reason) => Try::Wait(reason),
        Attempt::EnterFailed { .. } => Try::EnterFailed,
    }
}

fn mark_typed(shared: &Mutex<Vec<HandOverFact>>, id: &str) {
    let mut all = shared.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(fact) = all.iter_mut().find(|fact| fact.id == id) {
        fact.state = HandOverState::Typed;
    }
}

/// Tries every `RETRY` until the line is typed or `GIVE_UP` has passed.
async fn keep_trying<P: CallPorts>(
    ports: Arc<P>,
    shared: Arc<Mutex<Vec<HandOverFact>>>,
    id: String,
    request: String,
    turn: Option<opentelemetry::Context>,
) {
    let line = format!("{MARK} hand-over {id}: {request}");
    let began = Instant::now();
    let mut told = false;
    let reason = loop {
        let reason = match try_once(&*ports, &line).await {
            Try::Typed { pane, while_busy } => {
                mark_typed(&shared, &id);
                telemetry::prompt_typed(
                    &pane,
                    StartedBy::Voice,
                    turn,
                    time::OffsetDateTime::now_utc(),
                );
                let wait_ms = u64::try_from(began.elapsed().as_millis()).unwrap_or(u64::MAX);
                info!(
                    hand_over_id = %id,
                    pane_id = %pane,
                    typed_while_busy = while_busy,
                    wait_ms,
                    "hand-over typed"
                );
                return;
            }
            Try::EnterFailed => break "enter_failed",
            Try::Wait(reason) => reason,
        };
        if !told {
            told = true;
            info!(hand_over_id = %id, reason = reason.as_str(), "hand-over waiting");
        }
        if began.elapsed() >= GIVE_UP {
            break reason.as_str();
        }
        sleep(RETRY).await;
    };
    warn!(hand_over_id = %id, reason, "hand-over not typed");
    if let Some(answering) = ports.answering() {
        let pane = chief_pane(&*ports);
        let text = format!("Harold couldn't pass hand-over {id} to the chief.");
        answering.thread.tell(pane.as_deref(), &text).await;
    }
}

#[cfg(test)]
#[path = "hand_over_tests.rs"]
mod tests;
