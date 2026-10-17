//! `GET /updates`: the thread and the call screen, live. A new stream starts with the whole
//! thread and, when Harold has calls, the call screen; then it sends each new row, each row
//! whose state changed, and the call screen each time the call changes, until shutdown or
//! the page goes away.

use std::collections::HashMap;
use std::convert::Infallible;

use askama::Template;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use datastar::prelude::{ElementPatchMode, PatchElements};
use tokio::sync::{mpsc, watch};
use tokio_stream::wrappers::ReceiverStream;

use super::pages::{CallSection, MessageView};
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
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let Some(whole) = whole_thread(&messages) else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let section = match call.as_mut() {
        Some(view) => match call_screen(&view.borrow_and_update()) {
            Some(section) => Some(section),
            None => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        },
        None => None,
    };
    let (patches, queued) = mpsc::channel(PATCHES_QUEUED);
    // The channel has room for both, and nothing reads it yet: these sends cannot wait or
    // fail.
    let _ = patches.try_send(Ok(whole));
    if let Some(section) = section {
        let _ = patches.try_send(Ok(section));
    }
    tokio::spawn(follow_thread(
        state,
        changes,
        call,
        shown(&messages),
        patches,
    ));
    Sse::new(ReceiverStream::<Result<Event, Infallible>>::new(queued))
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// Sends a patch for each new or changed row on every change of the thread, and the call
/// screen on every change of the call. Returns at shutdown, when the page has gone (a send
/// fails), or when the store stops telling of changes.
async fn follow_thread(
    state: AppState,
    mut changes: watch::Receiver<u64>,
    mut call: Option<watch::Receiver<PhoneView>>,
    mut known: HashMap<String, Shown>,
    patches: mpsc::Sender<Result<Event, Infallible>>,
) {
    let mut shutdown = state.shutdown.clone();
    loop {
        tokio::select! {
            _ = shutdown.changed() => return,
            () = patches.closed() => return,
            changed = changes.changed() => {
                if changed.is_err() {
                    return;
                }
            }
            section = call_changed(&mut call) => {
                // A screen that cannot be drawn waits for the next change.
                if let Some(section) = section
                    && patches.send(Ok(section)).await.is_err()
                {
                    return;
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
        for message in messages {
            let Some(patch) = patch_for(before.get(&message.event_id), message) else {
                continue;
            };
            if patches.send(Ok(patch)).await.is_err() {
                return;
            }
        }
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
