//! The alert address. With `[telemetry] alert_listen` set, HyperDX's webhooks post
//! `{"alert": "<name>", "state": "firing" | "cleared"}` to `POST /alert`. Harold accepts
//! only the four alerts below and types `harold alert: <name> <state>` into the pane of
//! the agent running in the chief's folder, with the same checks and typing as timed
//! prompts. The chief decides what to do with the line. Harold also queues one line of
//! its own, `harold alert: delivery-given-up firing`, through `AlertQueue`; the address
//! refuses that name.
//!
//! No log line carries any part of a posted body. A refused post logs a fixed line, and
//! the alert and state of an accepted one are fields, from the fixed lists below.

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
use tracing::{error, info, warn};

use crate::agent::domain::EffectiveAgentState;
use crate::timed_prompts::{
    AgentsView, Attempt, Ports, ROUND_INTERVAL, Reason, TYPED_PANE_HOLD, TmuxPorts, Writer,
    type_when_ready,
};

/// The alerts HyperDX may post, as named in the definition files.
pub(crate) const ALERT_NAMES: [&str; 4] = [
    "phone-request-unavailable",
    "phone-request-slow",
    "wrong-agent",
    "slot-idle",
];

const ALERT_STATES: [&str; 2] = ["firing", "cleared"];

const LINE_PREFIX: &str = "harold alert: ";

/// The alert Harold queues itself when it gives up a delivery. The address does not
/// accept it: it is not in `ALERT_NAMES`.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "used when a delivery is given up (Task 3)")
)]
pub(crate) const DELIVERY_GIVEN_UP: &str = "delivery-given-up";

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
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "used when a delivery is given up (Task 3)")
    )]
    pub(crate) fn firing(&self, alert: &str) {
        queue_line(&self.0, format!("{LINE_PREFIX}{alert} firing"));
    }
}

/// Lines accepted and not yet typed. A post that finds it full is answered 204 and the
/// alert is dropped.
pub(crate) const QUEUE_CAPACITY: usize = 32;

/// A line waiting to be typed, and when it was posted.
pub(crate) type Queued = (String, Instant);

/// How long a line waits for the chief to be idle, from its post, before it is dropped.
const GIVE_UP_AFTER: Duration = Duration::from_secs(10 * 60);

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

/// Types each queued line into the agent in `chief_folder`, trying every ROUND_INTERVAL
/// until 10 minutes after its post. Lines are typed one at a time, so a line behind one
/// that waited may find its 10 minutes already up; it is dropped as `agent_busy`. Shutdown abandons a line still waiting; it is looked at between
/// tries only, so a try is never stopped between typing and pressing Enter.
pub(crate) async fn deliver<P: Ports>(
    ports: P,
    chief_folder: Option<String>,
    mut queue: mpsc::Receiver<Queued>,
    mut shutdown: watch::Receiver<()>,
) {
    let chief_folder = chief_folder.filter(|folder| !folder.trim().is_empty());
    let mut held = None;
    loop {
        let (line, posted_at) = tokio::select! {
            biased;
            _ = shutdown.changed() => return,
            queued = queue.recv() => match queued {
                Some(queued) => queued,
                None => return,
            },
        };
        let (alert, state) = alert_and_state(&line);
        let Some(folder) = chief_folder.as_deref() else {
            log_not_delivered(alert, state, &Reason::NoAgentInFolder);
            continue;
        };
        let give_up_at = posted_at + GIVE_UP_AFTER;
        if Instant::now() >= give_up_at {
            // Never tried: it waited its 10 minutes behind an earlier line.
            log_not_delivered(alert, state, &Reason::AgentBusy);
            continue;
        }
        loop {
            let view = ports.agents();
            let typed_into = held_panes(&mut held, &view);
            match type_when_ready(&ports, Writer::Alert, folder, &line, &view, &typed_into).await {
                Attempt::Typed { pane } => {
                    info!(alert, state, pane_id = %pane, outcome = "typed", "alert typed");
                    held = Some(HeldPane::new(pane));
                    break;
                }
                Attempt::EnterFailed { pane, error } => {
                    // Not retyped: the line is in the box, and typing it again would put
                    // it there twice.
                    warn!(
                        alert,
                        state,
                        pane_id = %pane,
                        reason = "enter_failed",
                        error = %error,
                        "alert not delivered (enter_failed)"
                    );
                    held = Some(HeldPane::new(pane));
                    break;
                }
                Attempt::Wait(reason @ Reason::NoAgentInFolder) => {
                    log_not_delivered(alert, state, &reason);
                    break;
                }
                Attempt::Wait(reason) => {
                    if Instant::now() >= give_up_at {
                        log_not_delivered(alert, state, &reason);
                        break;
                    }
                    tokio::select! {
                        biased;
                        _ = shutdown.changed() => return,
                        _ = tokio::time::sleep(ROUND_INTERVAL) => {}
                    }
                }
            }
        }
    }
}

/// Logs a dropped line with a fixed message for each reason. With no agent in the chief's
/// folder, the reason is `no_chief`.
fn log_not_delivered(alert: &str, state: &str, reason: &Reason) {
    let name = reason.as_str();
    match reason {
        Reason::NoAgentInFolder => warn!(
            alert,
            state,
            reason = "no_chief",
            "alert not delivered (no_chief)"
        ),
        Reason::PaneListFailing => warn!(
            alert,
            state,
            reason = name,
            "alert not delivered (pane_list_failing)"
        ),
        Reason::SeveralAgentsInFolder { .. } => warn!(
            alert,
            state,
            reason = name,
            "alert not delivered (several_agents_in_folder)"
        ),
        Reason::InputBoxUnreadable => warn!(
            alert,
            state,
            reason = name,
            "alert not delivered (input_box_unreadable)"
        ),
        Reason::AgentBusy => warn!(
            alert,
            state,
            reason = name,
            "alert not delivered (agent_busy)"
        ),
        Reason::AgentStateUnknown => warn!(
            alert,
            state,
            reason = name,
            "alert not delivered (agent_state_unknown)"
        ),
        Reason::InputBoxNotFound => warn!(
            alert,
            state,
            reason = name,
            "alert not delivered (input_box_not_found)"
        ),
        Reason::InputBoxHoldsText => warn!(
            alert,
            state,
            reason = name,
            "alert not delivered (input_box_holds_text)"
        ),
        Reason::TypingFailed => warn!(
            alert,
            state,
            reason = name,
            "alert not delivered (typing_failed)"
        ),
        // Timed prompts only: a try at typing never gives these.
        Reason::WindowPassed => warn!(
            alert,
            state,
            reason = name,
            "alert not delivered (window_passed)"
        ),
        Reason::NewStore => warn!(
            alert,
            state,
            reason = name,
            "alert not delivered (new_store)"
        ),
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
