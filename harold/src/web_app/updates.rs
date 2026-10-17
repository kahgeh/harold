//! `GET /updates`: the thread, live. A new stream starts with the whole thread, then sends
//! each new row and each row whose state changed, until shutdown or the page goes away.

use std::collections::HashMap;
use std::convert::Infallible;

use askama::Template;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use datastar::prelude::{ElementPatchMode, PatchElements};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use super::pages::MessageView;
use super::{AppState, thread_start_ms};
use crate::store::{AppMessage, AppReplyState};

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
    let Ok(messages) = state.store.app_messages_since(thread_start_ms()).await else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let Some(whole) = whole_thread(&messages) else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let (patches, queued) = mpsc::channel(PATCHES_QUEUED);
    // The channel has room, and nothing reads it yet: this send cannot wait or fail.
    let _ = patches.try_send(Ok(whole));
    tokio::spawn(follow_thread(state, changes, shown(&messages), patches));
    Sse::new(ReceiverStream::<Result<Event, Infallible>>::new(queued))
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// Sends a patch for each new or changed row on every change of the thread. Returns at
/// shutdown, when the page has gone (a send fails), or when the store stops telling of
/// changes.
async fn follow_thread(
    state: AppState,
    mut changes: tokio::sync::watch::Receiver<u64>,
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
