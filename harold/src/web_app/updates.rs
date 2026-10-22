//! `GET /updates`: the thread and the call screen, live. A new stream starts with the
//! thread from the page's `oldest` signal on (the newest 50 rows when the page names none
//! it can find) and, when Harold has calls, the call screen; then it sends each new row,
//! each row in that window whose state changed, and the call screen each time the call
//! changes, until shutdown or the page goes away.

use std::collections::HashMap;
use std::convert::Infallible;
use std::time::{Duration, Instant};

use askama::Template;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use datastar::prelude::{ElementPatchMode, PatchElements, PatchSignals};
use serde::Deserialize;
use tokio::sync::{mpsc, watch};
use tokio_stream::wrappers::ReceiverStream;
use tracing::{info, warn};

use super::chief::find_chief;
use super::pages::{
    CallSection, MessageView, RespondingSection, Window, awaiting_chief, dots_show, older_line,
    thread_window,
};
use super::todos::TodosSection;
use super::{AppState, thread_start_ms};
use crate::store::{AppMessage, AppReplyState};
use crate::voice_input::PhoneView;

/// How often an open page reads the todos again.
const TODOS_EVERY: Duration = Duration::from_secs(2);

/// How many patches may wait for a slow page before the stream stops reading the thread.
const PATCHES_QUEUED: usize = 16;

/// `#messages` as a whole, the same section the thread page holds.
#[derive(Template)]
#[template(path = "web_app/messages.html")]
struct MessagesSection {
    messages: Vec<MessageView>,
    /// For `#older`: the query value naming the oldest row shown, when older rows remain.
    older: Option<String>,
}

/// The query Datastar adds to `GET /updates`: the page's signals as JSON.
#[derive(Deserialize)]
pub(crate) struct UpdatesQuery {
    datastar: Option<String>,
}

/// What the page's signals say of the oldest row it shows: `None` without signals,
/// `Some(None)` when they name none, and the id otherwise. Malformed signals are none.
fn oldest_signal(query: &UpdatesQuery) -> Option<Option<String>> {
    let signals = serde_json::from_str::<serde_json::Value>(query.datastar.as_deref()?).ok();
    Some(
        signals
            .as_ref()
            .and_then(|signals| signals.get("oldest"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
    )
}

/// The window the stream follows and what the page shows of its rows.
struct Seen {
    /// The window's oldest row; the newest 50 rows once it is pruned.
    from: Option<String>,
    /// Every row the page was sent, so a row that comes back into the window when `from`
    /// is pruned is not appended twice.
    known: HashMap<String, Shown>,
}

/// What the page shows of one row, to tell a changed row from an unchanged one.
type Shown = (String, Option<AppReplyState>);

/// `GET /updates`: an SSE stream of Datastar patches. A store that cannot be read at the
/// start gives `503`.
pub(crate) async fn get_updates(
    State(state): State<AppState>,
    query: Result<Query<UpdatesQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let said = query.ok().and_then(|Query(query)| oldest_signal(&query));
    let from = said.as_ref().and_then(Option::as_deref);
    // Watch before the first read, so a change made during it is not missed.
    let changes = state.store.watch_app_thread();
    let mut call = state.calls.as_ref().map(|calls| calls.view());
    let Ok(messages) = state.store.app_messages_since(thread_start_ms()).await else {
        warn!(reason = "store_unread", "app page not connected");
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let dots = Dots::of(&state, &messages);
    let todos_shown = todos_html(&state.questions.refresh().await);
    // The page may have been drawn, or the stream dropped while the phone was hidden, before
    // the dots and the todos changed, so the first patch carries them too.
    let window = thread_window(&messages, from);
    let Some((whole, bytes)) = whole_thread(window, dots.shown, &todos_shown) else {
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
    // A page that sent its signals but holds another oldest row than the window starts at
    // is told the new one. A request with no signals at all is not a page.
    let oldest_patch = match (said, window.oldest()) {
        (Some(said), Some(oldest)) if said.as_deref() != Some(oldest) => Some(oldest_event(oldest)),
        _ => None,
    };
    let (patches, queued) = mpsc::channel(PATCHES_QUEUED);
    // The channel has room for all three, and nothing reads it yet: these sends cannot wait
    // or fail.
    let _ = patches.try_send(Ok(whole));
    if let Some(section) = section {
        let _ = patches.try_send(Ok(section));
    }
    if let Some(patch) = oldest_patch {
        let _ = patches.try_send(Ok(patch));
    }
    info!(rows = window.rows.len(), bytes, "app page connected");
    let seen = Seen {
        from: window.oldest().map(str::to_owned),
        known: shown(window.rows),
    };
    tokio::spawn(follow_thread(
        state,
        changes,
        call,
        seen,
        dots,
        todos_shown,
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
    seen: Seen,
    dots: Dots,
    todos_shown: String,
    patches: mpsc::Sender<Result<Event, Infallible>>,
) {
    let connected = Instant::now();
    let reason = send_patches(state, changes, call, seen, dots, todos_shown, patches).await;
    info!(
        reason,
        seconds = connected.elapsed().as_secs(),
        "app page disconnected"
    );
}

/// Sends a patch for each new or changed row of the window that starts at `from` on every
/// change of the thread (the newest 50 rows once `from` is pruned), and the call screen on
/// every change of the call. Returns why it stopped: `shutdown`, `page_gone`
/// (the page has gone: a send fails) or `store_stopped` (the store stops telling of
/// changes).
async fn send_patches(
    state: AppState,
    mut changes: watch::Receiver<u64>,
    mut call: Option<watch::Receiver<PhoneView>>,
    mut seen: Seen,
    mut dots: Dots,
    mut todos_shown: String,
    patches: mpsc::Sender<Result<Event, Infallible>>,
) -> &'static str {
    let mut shutdown = state.shutdown.clone();
    let mut todo_posts = state.todos_changed.subscribe();
    let mut todo_ticks =
        tokio::time::interval_at(tokio::time::Instant::now() + TODOS_EVERY, TODOS_EVERY);
    todo_ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
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
            _ = todo_ticks.tick() => {
                if let Some(patch) = todos_patch(&state, &mut todos_shown).await
                    && patches.send(Ok(patch)).await.is_err()
                {
                    return "page_gone";
                }
                continue;
            }
            _ = todo_posts.changed() => {
                if let Some(patch) = todos_patch(&state, &mut todos_shown).await
                    && patches.send(Ok(patch)).await.is_err()
                {
                    return "page_gone";
                }
                continue;
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
        dots.awaiting = awaiting_chief(&messages);
        let dots_patch = dots.refresh(&state);
        let window = thread_window(&messages, seen.from.as_deref());
        for message in window.rows {
            let Some(patch) = patch_for(seen.known.get(&message.event_id), message.clone()) else {
                continue;
            };
            seen.known.insert(
                message.event_id.clone(),
                (message.text.clone(), message.state),
            );
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

/// The todo page and the todo count as one string of HTML, to tell a changed page from an
/// unchanged one.
fn todos_html(section: &TodosSection) -> String {
    let page = section.render().unwrap_or_default();
    let count = section.count().render().unwrap_or_default();
    format!("{page}{count}")
}

/// Reads the todos and returns the patch that brings the page to them, if they differ from
/// what `shown` holds; `shown` then holds the new HTML.
async fn todos_patch(state: &AppState, shown: &mut String) -> Option<Event> {
    let html = todos_html(&state.questions.refresh().await);
    if html.is_empty() || html == *shown {
        return None;
    }
    shown.clone_from(&html);
    Some(PatchElements::new(html).into())
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
        let chief = find_chief(
            &state.snapshots.borrow(),
            state.chief_folder.as_deref(),
            state.chief_session.as_deref(),
        );
        Self {
            awaiting,
            shown: dots_show(awaiting, &chief),
        }
    }

    /// The patch that brings the page to the dots as they now are, if they changed.
    fn refresh(&mut self, state: &AppState) -> Option<Event> {
        let chief = find_chief(
            &state.snapshots.borrow(),
            state.chief_folder.as_deref(),
            state.chief_session.as_deref(),
        );
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
        std::future::pending().await
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

/// One patch setting the page's `oldest` signal.
fn oldest_event(oldest: &str) -> Event {
    PatchSignals::new(serde_json::json!({ "oldest": oldest }).to_string()).into()
}

/// One patch replacing `#messages` with the rows of `window`, and `#responding`, `#todos`
/// and `#todo-count`, each morphed by its id; and the patch's size in bytes.
fn whole_thread(window: Window<'_>, dots: bool, todos: &str) -> Option<(Event, usize)> {
    let section = MessagesSection {
        messages: window.rows.iter().cloned().map(MessageView::from).collect(),
        older: older_line(&window),
    };
    let responding = RespondingSection { on: dots };
    let html = format!(
        "{}{}{todos}",
        section.render().ok()?,
        responding.render().ok()?
    );
    let bytes = html.len();
    Some((PatchElements::new(html).into(), bytes))
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
