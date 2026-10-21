//! The alert address. With `[telemetry] alert_listen` set, HyperDX's webhooks post
//! `{"alert": "<name>", "state": "firing" | "cleared"}` to `POST /alert`. Harold accepts
//! only the six alerts in `ALERT_NAMES` and types `harold alert: <name> <state>` into the pane of
//! the agent running in the chief's folder, with the same checks and typing as timed
//! prompts. The chief decides what to do with the line. Harold also queues lines of its
//! own through `AlertQueue`: `harold alert: delivery-given-up firing`,
//! `harold alert: chief-message-held-back firing`, and
//! `harold alert: store-failing firing` and `cleared`. The address refuses all three names.
//!
//! A line is typed only when it changes what the chief was last told about its alert, so
//! HyperDX's repeated `firing` posts reach the chief once. Every `delivery-given-up` is a
//! different lost message, and each is typed. A line waits for the chief without limit.
//!
//! No log line carries any part of a posted body. A refused post logs a fixed line, and
//! the alert and state of an accepted one are fields, from the fixed lists below.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::time::Duration;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::routing::any;
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::sync::mpsc::{self, error::TrySendError};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::{debug, error, info, warn};

use crate::agent::domain::EffectiveAgentState;
use crate::timed_prompts::{
    AgentsView, Attempt, Ports, ROUND_INTERVAL, Reason, TYPED_PANE_HOLD, TmuxPorts, Writer,
    type_when_ready,
};

/// The alerts HyperDX may post, as named in the definition files.
pub(crate) const ALERT_NAMES: [&str; 6] = [
    "phone-request-unavailable",
    "phone-request-slow",
    "wrong-agent",
    "slot-idle",
    "local-model-failing",
    "watch-cost-high",
];

const ALERT_STATES: [&str; 2] = ["firing", "cleared"];

const LINE_PREFIX: &str = "harold alert: ";

/// The alert Harold queues itself when it gives up a delivery. The address does not
/// accept it: it is not in `ALERT_NAMES`.
pub(crate) const DELIVERY_GIVEN_UP: &str = "delivery-given-up";

/// The alert Harold queues itself when a message or spoken line from the chief is held
/// back by a channel's list. The address does not accept it: it is not in `ALERT_NAMES`.
pub(crate) const CHIEF_MESSAGE_HELD_BACK: &str = "chief-message-held-back";

/// Whether every line of `alert` is typed, however many are queued: each is a different
/// lost message, and none is cleared.
fn typed_every_time(alert: &str) -> bool {
    alert == DELIVERY_GIVEN_UP || alert == CHIEF_MESSAGE_HELD_BACK
}

/// The alert Harold queues itself when it marks its store failing, and clears when the
/// store recovers. The address does not accept it: it is not in `ALERT_NAMES`.
pub(crate) const STORE_FAILING: &str = "store-failing";

/// Where Harold queues its own alert lines, for the same typing as posted ones.
#[derive(Clone, Debug)]
pub(crate) struct AlertQueue(mpsc::Sender<Queued>);

impl AlertQueue {
    /// A queue, and the receiver `deliver` reads.
    pub(crate) fn new() -> (Self, mpsc::Receiver<Queued>) {
        let (queue_tx, queue_rx) = mpsc::channel(QUEUE_CAPACITY);
        (Self(queue_tx), queue_rx)
    }

    /// Queues `harold alert: <alert> firing`. A full or closed queue drops the line with
    /// the same log line as a post's.
    pub(crate) fn firing(&self, alert: &str) {
        queue_line(&self.0, format!("{LINE_PREFIX}{alert} firing"));
    }

    /// Queues `harold alert: <alert> cleared`, as `firing` does.
    pub(crate) fn cleared(&self, alert: &str) {
        queue_line(&self.0, format!("{LINE_PREFIX}{alert} cleared"));
    }
}

/// Lines accepted and not yet read by `deliver`, which reads them while a line waits. A
/// post that finds it full is answered 204 and the alert is dropped.
pub(crate) const QUEUE_CAPACITY: usize = 32;

/// A queued line, and when it was queued.
pub(crate) type Queued = (String, Instant);

/// The largest body read. A webhook's body is a few dozen bytes.
const BODY_LIMIT: usize = 16 * 1024;

/// How long the address may take to close its connections once delivery has stopped.
const SERVER_STOP_LIMIT: Duration = Duration::from_secs(1);

// ---------------------------------------------------------------------------
// The line
// ---------------------------------------------------------------------------

/// The line typed for a posted body, or `Err` for anything but a JSON object with one of
/// the four alert names and `firing` or `cleared`.
pub(crate) fn alert_line(body: &[u8]) -> Result<String, ()> {
    // Read as a map, not a derived struct: serde would also take `["slot-idle","firing"]`.
    let Ok(Value::Object(post)) = serde_json::from_slice(body) else {
        return Err(());
    };
    let field = |name: &str| post.get(name).and_then(Value::as_str).ok_or(());
    let (alert, state) = (field("alert")?, field("state")?);
    if !ALERT_NAMES.contains(&alert) || !ALERT_STATES.contains(&state) {
        return Err(());
    }
    Ok(format!("{LINE_PREFIX}{alert} {state}"))
}

/// The alert and state a line names, for log fields.
fn alert_and_state(line: &str) -> (&str, &str) {
    line.strip_prefix(LINE_PREFIX)
        .and_then(|rest| rest.split_once(' '))
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// The address
// ---------------------------------------------------------------------------

/// `POST /alert`. Every other path and method is refused like an unknown alert.
pub(crate) fn router(queue: mpsc::Sender<Queued>) -> Router {
    Router::new()
        .route("/alert", any(take_alert))
        .fallback(refuse)
        .with_state(queue)
}

async fn take_alert(State(queue): State<mpsc::Sender<Queued>>, request: Request) -> StatusCode {
    if request.method() != Method::POST {
        return refuse().await;
    }
    let Ok(body) = axum::body::to_bytes(request.into_body(), BODY_LIMIT).await else {
        return refuse().await;
    };
    let Ok(line) = alert_line(&body) else {
        return refuse().await;
    };
    // The post is answered at once; typing waits in the background for an idle chief.
    queue_line(&queue, line);
    StatusCode::NO_CONTENT
}

/// Queues `line`, stamped now. A full or closed queue drops it and logs the drop.
fn queue_line(queue: &mpsc::Sender<Queued>, line: String) {
    let Err(dropped) = queue.try_send((line, Instant::now())) else {
        return;
    };
    let (TrySendError::Full((line, _)) | TrySendError::Closed((line, _))) = &dropped;
    let (alert, state) = alert_and_state(line);
    match dropped {
        TrySendError::Full(_) => warn!(
            alert,
            state,
            reason = "queue_full",
            "alert not delivered (queue_full)"
        ),
        TrySendError::Closed(_) => warn!(
            alert,
            state,
            reason = "shutting_down",
            "alert not delivered (shutting_down)"
        ),
    }
}

async fn refuse() -> StatusCode {
    warn!("alert ignored (unknown alert)");
    StatusCode::BAD_REQUEST
}

// ---------------------------------------------------------------------------
// What the chief was told
// ---------------------------------------------------------------------------

/// Why a line is not typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HeldBack {
    /// The chief was already told this state, or its line already waits.
    Repeat,
    /// A `cleared` for a `firing` the chief never heard.
    NeverFired,
}

impl HeldBack {
    /// The log field's value.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Repeat => "repeat",
            Self::NeverFired => "never_fired",
        }
    }
}

/// A line waiting for the chief, and whether a refused try at it was logged.
struct Waiting {
    line: String,
    refused: bool,
}

/// The state each alert's line last told the chief, and the lines still waiting, oldest
/// first, at most one for each alert. Kept in memory only, so it starts empty with
/// Harold.
#[derive(Default)]
pub(crate) struct Desk {
    told: HashMap<String, String>,
    waiting: VecDeque<Waiting>,
}

impl Desk {
    /// Takes a queued line. `Ok` when it now waits to be typed; otherwise why it is held
    /// back. A line for an alert whose line already waits replaces it: a repeat changes
    /// nothing, and the other state cancels both, since the chief never heard the first.
    /// Every `delivery-given-up` waits, but one waiting takes in the next.
    pub(crate) fn take(&mut self, line: String) -> Result<(), HeldBack> {
        let (alert, state) = alert_and_state(&line);
        let waiting_at = self
            .waiting
            .iter()
            .position(|waiting| alert_and_state(&waiting.line).0 == alert);
        if let Some(at) = waiting_at {
            if typed_every_time(alert) || alert_and_state(&self.waiting[at].line).1 == state {
                return Err(HeldBack::Repeat);
            }
            self.waiting.remove(at);
            return Err(if state == "cleared" {
                HeldBack::NeverFired
            } else {
                HeldBack::Repeat
            });
        }
        if !typed_every_time(alert) {
            match self.told.get(alert).map(String::as_str) {
                Some(told) if told == state => return Err(HeldBack::Repeat),
                None if state == "cleared" => return Err(HeldBack::NeverFired),
                _ => {}
            }
        }
        self.waiting.push_back(Waiting {
            line,
            refused: false,
        });
        Ok(())
    }

    /// The line to try next.
    pub(crate) fn oldest(&self) -> Option<&str> {
        self.waiting.front().map(|waiting| waiting.line.as_str())
    }

    /// Lets go of the oldest line, now in the chief's pane, and remembers its state.
    pub(crate) fn told_oldest(&mut self) {
        let Some(waiting) = self.waiting.pop_front() else {
            return;
        };
        let (alert, state) = alert_and_state(&waiting.line);
        if !typed_every_time(alert) {
            self.told.insert(alert.to_string(), state.to_string());
        }
    }

    /// `true` only the first time a try at the oldest line is refused.
    pub(crate) fn first_refusal(&mut self) -> bool {
        self.waiting
            .front_mut()
            .is_some_and(|waiting| !std::mem::replace(&mut waiting.refused, true))
    }
}

// ---------------------------------------------------------------------------
// Delivering
// ---------------------------------------------------------------------------

/// A pane Harold typed an alert into. The snapshot still says idle for a moment after
/// typing, so it counts as busy until it is seen not idle, for at most a minute, as a
/// timed prompt's pane does.
struct HeldPane {
    pane_id: String,
    until: Instant,
}

impl HeldPane {
    fn new(pane_id: String) -> Self {
        Self {
            pane_id,
            until: Instant::now() + TYPED_PANE_HOLD,
        }
    }
}

/// The panes that count as busy for this try. Lets go of the held pane once `view` shows
/// it not idle or gone, or its minute is up.
fn held_panes(held: &mut Option<HeldPane>, view: &AgentsView) -> Vec<String> {
    let still_held = held.as_ref().is_some_and(|held| {
        Instant::now() < held.until
            && view
                .panes
                .iter()
                .any(|pane| pane.pane_id == held.pane_id && pane.state == EffectiveAgentState::Idle)
    });
    if !still_held {
        *held = None;
    }
    held.iter().map(|held| held.pane_id.clone()).collect()
}

/// Types each queued line that changes what the chief was told into the agent in
/// `chief_folder`, trying the oldest waiting line every ROUND_INTERVAL until the chief's
/// pane is idle with an empty input box, however long that takes. Posts are read while a
/// line waits, so repeats are held back as they arrive and do not fill the queue. Shutdown
/// abandons the lines still waiting; it is looked at between tries only, so a try is never
/// stopped between typing and pressing Enter.
pub(crate) async fn deliver<P: Ports>(
    ports: P,
    chief_folder: Option<String>,
    mut queue: mpsc::Receiver<Queued>,
    mut shutdown: watch::Receiver<()>,
) {
    let chief_folder = chief_folder.filter(|folder| !folder.trim().is_empty());
    let mut desk = Desk::default();
    let mut held = None;
    let mut open = true;
    loop {
        if let Some(line) = desk.oldest() {
            let line = line.to_string();
            try_line(&ports, chief_folder.as_deref(), &line, &mut desk, &mut held).await;
        }
        let waiting = desk.oldest().is_some();
        if !open && !waiting {
            return;
        }
        tokio::select! {
            biased;
            _ = shutdown.changed() => return,
            queued = queue.recv(), if open => match queued {
                Some((line, _)) => {
                    if let Err(held_back) = desk.take(line.clone()) {
                        let (alert, state) = alert_and_state(&line);
                        debug!(
                            alert,
                            state,
                            reason = held_back.as_str(),
                            "alert repeat held back"
                        );
                    }
                }
                None => open = false,
            },
            _ = tokio::time::sleep(ROUND_INTERVAL), if waiting => {}
        }
    }
}

/// One try at typing `line`, the oldest waiting line. A line that reached the pane is
/// told, even when Enter failed: it is in the box, and typing it again would put it there
/// twice. A refused try leaves it waiting, and the first refusal is logged.
async fn try_line<P: Ports>(
    ports: &P,
    chief_folder: Option<&str>,
    line: &str,
    desk: &mut Desk,
    held: &mut Option<HeldPane>,
) {
    let (alert, state) = alert_and_state(line);
    let attempt = match chief_folder {
        Some(folder) => {
            let view = ports.agents();
            let typed_into = held_panes(held, &view);
            type_when_ready(ports, Writer::Alert, folder, line, &view, &typed_into).await
        }
        None => Attempt::Wait(Reason::NoAgentInFolder),
    };
    match attempt {
        Attempt::Typed { pane } => {
            info!(alert, state, pane_id = %pane, outcome = "typed", "alert typed");
            *held = Some(HeldPane::new(pane));
            desk.told_oldest();
        }
        Attempt::EnterFailed { pane, error } => {
            warn!(
                alert,
                state,
                pane_id = %pane,
                reason = "enter_failed",
                error = %error,
                "alert not delivered (enter_failed)"
            );
            *held = Some(HeldPane::new(pane));
            desk.told_oldest();
        }
        Attempt::Wait(reason) => {
            if desk.first_refusal() {
                log_waiting(alert, state, &reason);
            }
        }
    }
}

/// Logs a line's first refused try with a fixed message for each reason. With no agent in
/// the chief's folder, the reason is `no_chief`.
fn log_waiting(alert: &str, state: &str, reason: &Reason) {
    let name = reason.as_str();
    match reason {
        Reason::NoAgentInFolder => info!(
            alert,
            state,
            reason = "no_chief",
            "alert waiting (no_chief)"
        ),
        Reason::PaneListFailing => info!(
            alert,
            state,
            reason = name,
            "alert waiting (pane_list_failing)"
        ),
        // Not reached for the chief's folder, which is the only folder an alert types into:
        // the chief is that folder's first agent pane. The variant stays for other folders.
        Reason::SeveralAgentsInFolder { .. } => info!(
            alert,
            state,
            reason = name,
            "alert waiting (several_agents_in_folder)"
        ),
        Reason::InputBoxUnreadable => info!(
            alert,
            state,
            reason = name,
            "alert waiting (input_box_unreadable)"
        ),
        Reason::AgentBusy => info!(alert, state, reason = name, "alert waiting (agent_busy)"),
        Reason::AgentStateUnknown => info!(
            alert,
            state,
            reason = name,
            "alert waiting (agent_state_unknown)"
        ),
        Reason::InputBoxNotFound => info!(
            alert,
            state,
            reason = name,
            "alert waiting (input_box_not_found)"
        ),
        Reason::InputBoxHoldsText => info!(
            alert,
            state,
            reason = name,
            "alert waiting (input_box_holds_text)"
        ),
        Reason::TypingFailed => info!(alert, state, reason = name, "alert waiting (typing_failed)"),
        // Timed prompts only: a try at typing never gives these.
        Reason::WindowPassed => info!(alert, state, reason = name, "alert waiting (window_passed)"),
        Reason::NewStore => info!(alert, state, reason = name, "alert waiting (new_store)"),
    }
}

// ---------------------------------------------------------------------------
// Start-up
// ---------------------------------------------------------------------------

/// Listens on `addr` and types accepted alerts into the chief's pane until shutdown. A
/// failure to listen is returned; Harold then logs it and runs without the alert address.
pub(crate) async fn start(
    addr: SocketAddr,
    chief_folder: Option<String>,
    ports: TmuxPorts,
    shutdown: watch::Receiver<()>,
) -> std::io::Result<(JoinHandle<()>, AlertQueue)> {
    let listener = TcpListener::bind(addr).await?;
    info!(address = %addr, "alert address listening");
    Ok(serve_on(listener, chief_folder, ports, shutdown))
}

/// Serves the address on `listener` and runs the delivery, both until shutdown. The handle
/// ends when both have stopped. The queue is Harold's own way in.
fn serve_on<P: Ports>(
    listener: TcpListener,
    chief_folder: Option<String>,
    ports: P,
    shutdown: watch::Receiver<()>,
) -> (JoinHandle<()>, AlertQueue) {
    let (queue, queue_rx) = AlertQueue::new();
    let delivery = tokio::spawn(deliver(ports, chief_folder, queue_rx, shutdown.clone()));
    let mut server_shutdown = shutdown;
    let server =
        axum::serve(listener, router(queue.0.clone())).with_graceful_shutdown(async move {
            let _ = server_shutdown.changed().await;
        });
    let mut server = tokio::spawn(async move { server.await });
    let handle = tokio::spawn(async move {
        let _ = delivery.await;
        // Delivery stops at shutdown, when the address stops taking connections too. A
        // client that holds its connection open is not waited for.
        match tokio::time::timeout(SERVER_STOP_LIMIT, &mut server).await {
            Ok(Ok(Err(error))) => error!(error = %error, "alert address stopped"),
            Ok(_) => {}
            Err(_) => server.abort(),
        }
    });
    (handle, queue)
}

#[cfg(test)]
#[path = "alerts_tests.rs"]
mod tests;
