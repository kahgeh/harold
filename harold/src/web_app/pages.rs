//! The thread page and the app's own files, all compiled into the binary.

use std::convert::Infallible;
use std::time::Instant;

use askama::Template;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::sse::{Event, Sse};
use axum::response::{Html, IntoResponse, Response};
use datastar::prelude::{ElementPatchMode, PatchElements, PatchSignals};
use tracing::{info, warn};

use super::chief::{ChiefPane, find_chief};
use super::todos::{TodoCount, TodosSection};
use super::updates::{UpdatesQuery, oldest_signal};
use super::{AppState, DATASTAR_JS, WebAppIdentity, thread_start_ms};
use crate::store::{AppMessage, AppReplyState, AppSender};
use crate::voice_input::{CallState, PhoneView, Said, Speaker};

/// The content security policy every response carries: Harold's own scripts, styles and
/// files only, nothing inline and nothing from another site.
pub(crate) const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; script-src 'self' 'unsafe-eval'; style-src 'self'; img-src 'self'; connect-src 'self'; manifest-src 'self'; worker-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'";

const APP_CSS: &[u8] = include_bytes!("../../static/web_app/app.css");
const ALERTS_JS: &[u8] = include_bytes!("../../static/web_app/alerts.js");
const SW_JS: &[u8] = include_bytes!("../../static/web_app/sw.js");
const PAGES_JS: &[u8] = include_bytes!("../../static/web_app/pages.js");
const ECHO_TEST_JS: &[u8] = include_bytes!("../../static/web_app/echo-test.js");
const CALL_JS: &[u8] = include_bytes!("../../static/web_app/call.js");
const CALL_WORKLET_JS: &[u8] = include_bytes!("../../static/web_app/call-worklet.js");

const JAVASCRIPT: &str = "text/javascript";

/// The thread: the last `THREAD_DAYS` days of messages, oldest first.
#[derive(Template)]
#[template(path = "web_app/thread.html")]
pub(crate) struct ThreadPage {
    /// The app's name: the title, the heading and the home-screen label.
    pub app_name: String,
    /// Whether the page links the app's icon.
    pub has_icon: bool,
    pub messages: Vec<MessageView>,
    /// Whether the `#older` line shows: older rows remain.
    pub older: bool,
    /// The oldest row shown, as the page's `oldest` signal holds it; empty without rows.
    pub oldest: String,
    /// Harold's VAPID public key, base64url: the page's `applicationServerKey`.
    pub public_key: String,
    /// Whether a call can start: the header offers one, or says voice is not set up.
    pub call_set_up: bool,
    /// The call screen, shown by `call.js` while a call lasts.
    pub call: CallSection,
    /// The responding dots, under the thread.
    pub responding: RespondingSection,
    /// The todo page.
    pub todos: TodosSection,
    /// The todo count on the **Todos** button.
    pub todo_count: TodoCount,
}

/// How many rows the thread page opens with, and each older load adds.
pub(crate) const WINDOW_ROWS: usize = 50;

/// A run of the thread's rows, oldest first, and whether older rows remain before it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Window<'a> {
    pub rows: &'a [AppMessage],
    pub more: bool,
}

impl Window<'_> {
    /// The `event_id` of the oldest row in the window.
    pub(crate) fn oldest(&self) -> Option<&str> {
        self.rows.first().map(|row| row.event_id.as_str())
    }
}

/// The rows a page shows, cut from the thread `rows` (oldest first). When `from` is found,
/// the rows from it on, with older rows remaining when it is not the first. Otherwise,
/// whether `from` is none, unknown, pruned or malformed, the newest `WINDOW_ROWS`.
pub(crate) fn thread_window<'a>(rows: &'a [AppMessage], from: Option<&str>) -> Window<'a> {
    let start = from
        .and_then(|from| rows.iter().position(|row| row.event_id == from))
        .unwrap_or_else(|| rows.len().saturating_sub(WINDOW_ROWS));
    Window {
        rows: &rows[start..],
        more: start > 0,
    }
}

/// The up to `WINDOW_ROWS` rows before `before`, with older rows remaining when more come
/// before them. A `before` that is not found gives no rows and none remain.
pub(crate) fn older_rows<'a>(rows: &'a [AppMessage], before: &str) -> Window<'a> {
    let Some(at) = rows.iter().position(|row| row.event_id == before) else {
        return Window {
            rows: &[],
            more: false,
        };
    };
    let start = at.saturating_sub(WINDOW_ROWS);
    Window {
        rows: &rows[start..at],
        more: start > 0,
    }
}

/// Whether `#older` shows: older rows remain before the window.
pub(crate) fn older_line(window: &Window<'_>) -> bool {
    window.oldest().is_some() && window.more
}

/// `text` as a single-quoted JavaScript string, for a Datastar expression.
fn js_string(text: &str) -> String {
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('\'');
    for c in text.chars() {
        match c {
            '\\' => quoted.push_str("\\\\"),
            '\'' => quoted.push_str("\\'"),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            c => quoted.push(c),
        }
    }
    quoted.push('\'');
    quoted
}

/// One row of the thread, as `#messages` shows it.
#[derive(Template)]
#[template(path = "web_app/message.html")]
pub(crate) struct MessageView {
    pub event_id: String,
    /// `chief` or `user`, the row's class.
    pub sender: &'static str,
    /// The text as plain parts and links.
    pub parts: Vec<Part>,
    /// Your reply's mark; `None` on the chief's rows.
    pub mark: Option<Mark>,
    /// For a reply that waits or was not sent, the words after `me`, such as
    /// `waiting: chief busy`.
    pub words: Option<&'static str>,
}

/// The mark before `me`: stored by Harold, or typed into the chief's pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mark {
    Stored,
    Typed,
}

/// One run of a message's text: plain, or an `http://` or `https://` address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Part {
    pub text: String,
    pub link: bool,
}

/// Characters left outside a link when they end the address.
const LINK_TRAILING: &[char] = &['.', ',', ';', ':', '!', '?', ')', ']', '\'', '"'];

/// Splits `text` into plain parts and links. A link is a run starting `http://` or
/// `https://` up to the next whitespace, without the trailing punctuation in
/// `LINK_TRAILING`; a run that is only the scheme stays plain.
pub(crate) fn split_links(text: &str) -> Vec<Part> {
    let mut parts = Vec::new();
    let mut plain_from = 0;
    let mut at = 0;
    while at < text.len() {
        let rest = &text[at..];
        let scheme = ["https://", "http://"]
            .into_iter()
            .find(|scheme| rest.starts_with(scheme));
        let Some(scheme) = scheme else {
            at += rest.chars().next().map_or(1, char::len_utf8);
            continue;
        };
        let run = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let address = rest[..run].trim_end_matches(LINK_TRAILING);
        if address.len() <= scheme.len() {
            at += scheme.len();
            continue;
        }
        if plain_from < at {
            parts.push(Part {
                text: text[plain_from..at].to_owned(),
                link: false,
            });
        }
        parts.push(Part {
            text: address.to_owned(),
            link: true,
        });
        at += address.len();
        plain_from = at;
    }
    if plain_from < text.len() {
        parts.push(Part {
            text: text[plain_from..].to_owned(),
            link: false,
        });
    }
    parts
}

impl From<AppMessage> for MessageView {
    fn from(message: AppMessage) -> Self {
        let user = message.sender == AppSender::User;
        let mark = match message.state {
            _ if !user => None,
            Some(AppReplyState::Typed | AppReplyState::WaitingChiefBusy) => Some(Mark::Typed),
            _ => Some(Mark::Stored),
        };
        let words = match message.state {
            Some(
                state @ (AppReplyState::WaitingChiefBusy
                | AppReplyState::NotSentNoChief
                | AppReplyState::NotSentPaused),
            ) => Some(state.label()),
            _ => None,
        };
        Self {
            event_id: message.event_id,
            sender: if user { "user" } else { "chief" },
            parts: split_links(&message.text),
            mark,
            words,
        }
    }
}

/// `#responding`, under the newest message: the three dots while the chief works on your
/// last reply, empty otherwise.
#[derive(Template)]
#[template(
    source = r#"<div id="responding" class="responding" role="status">{% if on %}<span class="dots" aria-label="the chief is responding"><i></i><i></i><i></i></span>{% endif %}</div>"#,
    ext = "html"
)]
pub(crate) struct RespondingSection {
    pub on: bool,
}

/// True when the newest row is your reply, typed into the chief's pane or waiting for it
/// (`awaiting_chief`).
pub(crate) fn awaiting_chief(messages: &[AppMessage]) -> bool {
    messages.last().is_some_and(|last| {
        last.sender == AppSender::User
            && matches!(
                last.state,
                Some(AppReplyState::Typed | AppReplyState::WaitingChiefBusy)
            )
    })
}

/// Whether the responding dots show: the chief is the one pane in its folder and busy,
/// while the newest row is a reply awaiting it.
pub(crate) fn dots_show(awaiting: bool, chief: &ChiefPane) -> bool {
    awaiting && matches!(chief, ChiefPane::One { busy: true, .. })
}

/// `#call`, the call screen as the server draws it. `call.js` plays what its
/// `data-harold-*` attributes name, and the stream morphs it as the call changes.
#[derive(Template)]
#[template(path = "web_app/call.html")]
pub(crate) struct CallSection {
    /// `off`, `listening`, `thinking` or `speaking`.
    pub state: &'static str,
    /// What the call is doing, in the screen's words; empty when off.
    pub title: &'static str,
    /// The text of what plays.
    pub line: Option<String>,
    /// The user's last turn, shown as `You said:`.
    pub transcript: Option<String>,
    /// The sound to fetch and play; empty when none.
    pub sound_id: String,
    /// The line to say in the phone's own voice, and its text; empty when none.
    pub speech_id: String,
    pub speech_text: String,
}

impl CallSection {
    /// The screen with no call open.
    fn off() -> Self {
        Self {
            state: "off",
            title: "",
            line: None,
            transcript: None,
            sound_id: String::new(),
            speech_id: String::new(),
            speech_text: String::new(),
        }
    }
}

impl From<&PhoneView> for CallSection {
    fn from(view: &PhoneView) -> Self {
        let chief_line = view.sound.as_ref().is_some_and(|sound| {
            matches!(
                sound.said,
                Said::Line {
                    by: Speaker::Chief,
                    ..
                }
            )
        });
        // The tick plays while the chief thinks, so the title follows the state.
        let (state, title) = match view.state {
            CallState::NotSetUp | CallState::Off => return Self::off(),
            CallState::Listening | CallState::Transcribing => ("listening", "Listening"),
            CallState::Waiting => ("thinking", "Chief is thinking"),
            CallState::Speaking if chief_line => ("speaking", "Chief speaking"),
            CallState::Speaking => ("speaking", "Harold speaking"),
        };
        let sound_text = view.sound.as_ref().and_then(|sound| match &sound.said {
            Said::Line { text, .. } | Said::Phrase(text) => Some(text),
            Said::Tick => None,
        });
        let speech_text = view.speech.as_ref().map(|speech| &speech.text);
        Self {
            state,
            title,
            line: sound_text
                .or(speech_text)
                .filter(|text| !text.is_empty())
                .cloned(),
            transcript: view.transcript.clone(),
            sound_id: view
                .sound
                .as_ref()
                .map(|sound| sound.id.clone())
                .unwrap_or_default(),
            speech_id: view
                .speech
                .as_ref()
                .map(|speech| speech.id.clone())
                .unwrap_or_default(),
            speech_text: speech_text.cloned().unwrap_or_default(),
        }
    }
}

/// Sets the content security policy on every response that passes through.
pub(crate) async fn add_policy(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    response
}

/// `GET /`: the thread page, with the newest `WINDOW_ROWS` rows. A store that cannot be
/// read gives `503`.
pub(crate) async fn get_thread(State(state): State<AppState>) -> Response {
    let started = Instant::now();
    let Ok(messages) = state.store.app_messages_since(thread_start_ms()).await else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let calls = state.calls.as_ref();
    let chief = find_chief(
        &state.snapshots.borrow(),
        state.chief_folder.as_deref(),
        state.chief_session.as_deref(),
    );
    let responding = RespondingSection {
        on: dots_show(awaiting_chief(&messages), &chief),
    };
    let todos = state.questions.refresh().await;
    let window = thread_window(&messages, None);
    let rows = window.rows.len();
    let page = ThreadPage {
        todo_count: todos.count(),
        todos,
        app_name: state.identity.name.clone(),
        has_icon: state.identity.icon.is_some(),
        older: older_line(&window),
        oldest: js_string(window.oldest().unwrap_or_default()),
        messages: window.rows.iter().cloned().map(MessageView::from).collect(),
        public_key: state.public_key.to_string(),
        call_set_up: calls.is_some_and(|calls| calls.set_up()),
        call: calls.map_or_else(CallSection::off, |calls| {
            CallSection::from(&*calls.view().borrow())
        }),
        responding,
    };
    let Ok(html) = page.render() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    info!(
        rows,
        bytes = html.len(),
        ms = started.elapsed().as_millis() as u64,
        "app page served"
    );
    Html(html).into_response()
}

/// `GET /thread/older`: the up to `WINDOW_ROWS` rows before the page's `oldest` signal,
/// as two patches, then the response ends: the rows after `#older`, and the `oldest` signal.
/// `#older` itself is left as it is, so its watcher reports only when it comes into view;
/// when no older rows remain, a last patch removes it. Signals that name no `oldest`, or a
/// row not found, remove `#older` and add no rows. A store that cannot be read gives `503`.
pub(crate) async fn get_thread_older(
    State(state): State<AppState>,
    query: Result<Query<UpdatesQuery>, QueryRejection>,
) -> Response {
    let started = Instant::now();
    let Ok(messages) = state.store.app_messages_since(thread_start_ms()).await else {
        warn!(reason = "store_unread", "app thread older not read");
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let oldest = query
        .ok()
        .and_then(|Query(query)| oldest_signal(&query))
        .flatten();
    let window = older_rows(&messages, oldest.as_deref().unwrap_or_default());
    let mut rows = String::new();
    for message in window.rows {
        let Ok(html) = MessageView::from(message.clone()).render() else {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        };
        rows.push_str(&html);
        rows.push('\n');
    }
    let mut events: Vec<Event> = Vec::with_capacity(3);
    let mut bytes = rows.len();
    if !window.rows.is_empty() {
        events.push(
            PatchElements::new(rows)
                .selector("#older")
                .mode(ElementPatchMode::After)
                .into(),
        );
    }
    if !window.more {
        events.push(PatchElements::new_remove("#older").into());
    }
    if let Some(oldest) = window.oldest() {
        let signal = serde_json::json!({ "oldest": oldest }).to_string();
        bytes += signal.len();
        events.push(PatchSignals::new(signal).into());
    }
    info!(
        rows = window.rows.len(),
        bytes,
        ms = started.elapsed().as_millis() as u64,
        "app thread older served"
    );
    Sse::new(tokio_stream::iter(
        events.into_iter().map(Ok::<Event, Infallible>),
    ))
    .into_response()
}

/// The web app manifest: the app's name, and its icon at the icon's real size when it has one.
pub(crate) fn manifest_json(identity: &WebAppIdentity) -> String {
    let mut manifest = serde_json::json!({
        "name": identity.name,
        "short_name": identity.name,
        "start_url": "/",
        "scope": "/",
        "display": "standalone",
        "background_color": "#f8f9fa",
        "theme_color": "#f8f9fa",
    });
    if let Some(icon) = &identity.icon {
        manifest["icons"] = serde_json::json!([{
            "src": "/static/icon.png",
            "sizes": format!("{0}x{0}", icon.side),
            "type": "image/png",
        }]);
    }
    manifest.to_string()
}

/// `GET /static/{name}`: one of the app's files, or `404`; `icon.png` only when an icon is set.
pub(crate) async fn get_static(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Response {
    let (content_type, bytes): (&str, Vec<u8>) = match name.as_str() {
        "app.css" => ("text/css", APP_CSS.to_vec()),
        "alerts.js" => (JAVASCRIPT, ALERTS_JS.to_vec()),
        "call.js" => (JAVASCRIPT, CALL_JS.to_vec()),
        "echo-test.js" => (JAVASCRIPT, ECHO_TEST_JS.to_vec()),
        "pages.js" => (JAVASCRIPT, PAGES_JS.to_vec()),
        "call-worklet.js" => (JAVASCRIPT, CALL_WORKLET_JS.to_vec()),
        "sw.js" => (JAVASCRIPT, SW_JS.to_vec()),
        "datastar-1.0.2.js" => (JAVASCRIPT, DATASTAR_JS.to_vec()),
        "manifest.webmanifest" => (
            "application/manifest+json",
            manifest_json(&state.identity).into_bytes(),
        ),
        "icon.png" => match &state.identity.icon {
            Some(icon) => ("image/png", icon.bytes.clone()),
            None => return StatusCode::NOT_FOUND.into_response(),
        },
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    let mut response = ([(header::CONTENT_TYPE, content_type)], bytes).into_response();
    if name == "sw.js" {
        // Lets the worker, served under /static/, control the whole app.
        response
            .headers_mut()
            .insert("service-worker-allowed", HeaderValue::from_static("/"));
    }
    response
}

/// Any other path: `404`, behind the login check like every route.
pub(crate) async fn not_found() -> StatusCode {
    StatusCode::NOT_FOUND
}
