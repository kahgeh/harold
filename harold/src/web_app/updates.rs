//! `GET /updates`: the thread and the call screen, live. A new stream starts with the whole
//! thread and, when Harold has calls, the call screen; then it sends each new row, each row
//! whose state changed, and the call screen each time the call changes, until shutdown or
//! the page goes away.

use std::collections::HashMap;
use std::convert::Infallible;
use std::time::Instant;

use askama::Template;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use datastar::prelude::{ElementPatchMode, PatchElements};
use tokio::sync::{mpsc, watch};
use tokio_stream::wrappers::ReceiverStream;
use tracing::{info, warn};

use super::chief::find_chief;
use super::pages::{CallSection, MessageView, RespondingSection, awaiting_chief, dots_show};
use super::{AppState, thread_start_ms};
use crate::store::{AppMessage, AppReplyState};
use crate::voice_input::PhoneView;

/// How many patches may wait for a slow page before the stream stops reading the thread.
const PATCHES_QUEUED: usize = 16;

/// `#messages` as a whole, the same section the thread page holds.
#[derive(Template)]
#[template(path = "web_app/messages.html")]
struct MessagesSection {
    messages: Vec<MessageView>,
}

/// What the page shows of one row, to tell a changed row from an unchanged one.
type Shown = (String, Option<AppReplyState>);

/// `GET /updates`: an SSE stream of Datastar patches. A store that cannot be read at the
/// start gives `503`.
pub(crate) async fn get_updates(State(state): State<AppState>) -> Response {
    // Watch before the first read, so a change made during it is not missed.
    let changes = state.store.watch_app_thread();
    let mut call = state.calls.as_ref().map(|calls| calls.view());
    let Ok(messages) = state.store.app_messages_since(thread_start_ms()).await else {
        warn!(reason = "store_unread", "app page not connected");
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let Some(whole) = whole_thread(&messages) else {
        warn!(reason = "render_failed", "app page not connected");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let section = match call.as_mut() {
        Some(view) => match call_screen(&view.borrow_and_update()) {
            Some(section) => Some(section),
            None => {
                warn!(reason = "render_failed", "app page not connected");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        },
        None => None,
    };
    let (patches, queued) = mpsc::channel(PATCHES_QUEUED);
    let dots = Dots::of(&state, &messages);
    // The channel has room for all of them, and nothing reads it yet: these sends cannot
    // wait or fail.
    let _ = patches.try_send(Ok(whole));
    // The page was drawn without the dots, so they are sent only when they are on.
    if dots.shown
        && let Some(patch) = responding_patch(true)
    {
        let _ = patches.try_send(Ok(patch));
    }
    if let Some(section) = section {
        let _ = patches.try_send(Ok(section));
    }
    info!("app page connected");
    tokio::spawn(follow_thread(
        state,
        changes,
        call,
        shown(&messages),
        dots,
        patches,
    ));
    Sse::new(ReceiverStream::<Result<Event, Infallible>>::new(queued))
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// Follows the thread until the page goes, and logs how it ended and how long the page was
/// connected. Harold notices a closed page at the next patch or keep-alive.
async fn follow_thread(
    state: AppState,
    changes: watch::Receiver<u64>,
    call: Option<watch::Receiver<PhoneView>>,
    known: HashMap<String, Shown>,
    dots: Dots,
    patches: mpsc::Sender<Result<Event, Infallible>>,
) {
    let connected = Instant::now();
    let reason = send_patches(state, changes, call, known, dots, patches).await;
    info!(
        reason,
        seconds = connected.elapsed().as_secs(),
        "app page disconnected"
    );
}

/// Sends a patch for each new or changed row on every change of the thread, and the call
/// screen on every change of the call. Returns why it stopped: `shutdown`, `page_gone`
/// (the page has gone: a send fails) or `store_stopped` (the store stops telling of
/// changes).
async fn send_patches(
    state: AppState,
    mut changes: watch::Receiver<u64>,
    mut call: Option<watch::Receiver<PhoneView>>,
    mut known: HashMap<String, Shown>,
    mut dots: Dots,
    patches: mpsc::Sender<Result<Event, Infallible>>,
) -> &'static str {
    let mut shutdown = state.shutdown.clone();
    let mut agents = Some(state.snapshots.clone());
    loop {
        tokio::select! {
            _ = shutdown.changed() => return "shutdown",
            () = patches.closed() => return "page_gone",
            changed = changes.changed() => {
                if changed.is_err() {
                    return "store_stopped";
                }
            }
            () = agents_changed(&mut agents) => {
                if let Some(patch) = dots.refresh(&state)
                    && patches.send(Ok(patch)).await.is_err()
                {
                    return "page_gone";
                }
                continue;
            }
            section = call_changed(&mut call) => {
                // A screen that cannot be drawn waits for the next change.
                if let Some(section) = section
                    && patches.send(Ok(section)).await.is_err()
                {
                    return "page_gone";
                }
                continue;
            }
        }
        // A failed read waits for the next change, which reads the whole thread again.
        let Ok(messages) = state.store.app_messages_since(thread_start_ms()).await else {
            continue;
        };
        // Rows pruned from the thread drop out of `known` here.
        let before = std::mem::replace(&mut known, shown(&messages));
        dots.awaiting = awaiting_chief(&messages);
        let dots_patch = dots.refresh(&state);
        for message in messages {
            let Some(patch) = patch_for(before.get(&message.event_id), message) else {
                continue;
            };
            if patches.send(Ok(patch)).await.is_err() {
                return "page_gone";
            }
        }
        // After the rows, so a chief row lands before the dots go.
        if let Some(patch) = dots_patch
            && patches.send(Ok(patch)).await.is_err()
        {
            return "page_gone";
        }
    }
}

/// Whether the responding dots show, and whether the newest row is a reply awaiting the
/// chief, which only a read of the thread can change.
struct Dots {
    awaiting: bool,
    shown: bool,
}

impl Dots {
    fn of(state: &AppState, messages: &[AppMessage]) -> Self {
        let awaiting = awaiting_chief(messages);
        let chief = find_chief(&state.snapshots.borrow(), state.chief_folder.as_deref());
        Self {
            awaiting,
            shown: dots_show(awaiting, &chief),
        }
    }

    /// The patch that brings the page to the dots as they now are, if they changed.
    fn refresh(&mut self, state: &AppState) -> Option<Event> {
        let chief = find_chief(&state.snapshots.borrow(), state.chief_folder.as_deref());
        let on = dots_show(self.awaiting, &chief);
        if on == self.shown {
            return None;
        }
        self.shown = on;
        responding_patch(on)
    }
}

/// One patch replacing `#responding`, morphed by its id.
fn responding_patch(on: bool) -> Option<Event> {
    Some(PatchElements::new(RespondingSection { on }.render().ok()?).into())
}

/// Resolves when the agent snapshot next changes. Never resolves once the hub has gone.
async fn agents_changed(agents: &mut Option<watch::Receiver<crate::agent::domain::AgentSnapshot>>) {
    let Some(receiver) = agents.as_mut() else {
        return std::future::pending().await;
    };
    if receiver.changed().await.is_err() {
        *agents = None;
        return std::future::pending().await;
    }
}

/// The patch a reloaded row needs, if any, given what the page showed `before`: a new row
/// is appended to `#messages`, a row that changed is morphed by its id.
fn patch_for(before: Option<&Shown>, message: AppMessage) -> Option<Event> {
    if before.is_some_and(|(text, state)| *text == message.text && *state == message.state) {
        return None;
    }
    let row = MessageView::from(message).render().ok()?;
    let patch = match before {
        None => PatchElements::new(row)
            .selector("#messages")
            .mode(ElementPatchMode::Append),
        Some(_) => PatchElements::new(row),
    };
    Some(patch.into())
}

/// The call screen once the call next changes. Never resolves without calls, or once the
/// calls have gone.
async fn call_changed(call: &mut Option<watch::Receiver<PhoneView>>) -> Option<Event> {
    let Some(view) = call.as_mut() else {
        return std::future::pending().await;
    };
    if view.changed().await.is_err() {
        *call = None;
        return std::future::pending().await;
    }
    call_screen(&view.borrow_and_update())
}

/// One patch replacing `#call` with the screen `view` shows, morphed by its id.
fn call_screen(view: &PhoneView) -> Option<Event> {
    let section = CallSection::from(view).render().ok()?;
    Some(PatchElements::new(section).into())
}

/// One patch replacing `#messages` with `messages`, morphed by its id.
fn whole_thread(messages: &[AppMessage]) -> Option<Event> {
    let section = MessagesSection {
        messages: messages.iter().cloned().map(MessageView::from).collect(),
    };
    Some(PatchElements::new(section.render().ok()?).into())
}

fn shown(messages: &[AppMessage]) -> HashMap<String, Shown> {
    messages
        .iter()
        .map(|message| {
            (
                message.event_id.clone(),
                (message.text.clone(), message.state),
            )
        })
        .collect()
}
