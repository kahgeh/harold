//! The thread page and the app's own files, all compiled into the binary.

use askama::Template;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Response};

use super::{AppState, DATASTAR_JS, thread_start_ms};
use crate::store::{AppMessage, AppSender};
use crate::voice_input::{CallState, PhoneView, Said, Speaker};

/// The content security policy every response carries: Harold's own scripts, styles and
/// files only, nothing inline and nothing from another site.
pub(crate) const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; script-src 'self' 'unsafe-eval'; style-src 'self'; img-src 'self'; connect-src 'self'; manifest-src 'self'; worker-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'";

const APP_CSS: &[u8] = include_bytes!("../../static/web_app/app.css");
const ALERTS_JS: &[u8] = include_bytes!("../../static/web_app/alerts.js");
const SW_JS: &[u8] = include_bytes!("../../static/web_app/sw.js");
const MANIFEST: &[u8] = include_bytes!("../../static/web_app/manifest.webmanifest");
const CALL_JS: &[u8] = include_bytes!("../../static/web_app/call.js");
const CALL_WORKLET_JS: &[u8] = include_bytes!("../../static/web_app/call-worklet.js");

const JAVASCRIPT: &str = "text/javascript";

/// The thread: the last `THREAD_DAYS` days of messages, oldest first.
#[derive(Template)]
#[template(path = "web_app/thread.html")]
pub(crate) struct ThreadPage {
    pub messages: Vec<MessageView>,
    /// Harold's VAPID public key, base64url: the page's `applicationServerKey`.
    pub public_key: String,
    /// Whether a call can start: the header offers one, or says voice is not set up.
    pub call_set_up: bool,
    /// The call screen, shown by `call.js` while a call lasts.
    pub call: CallSection,
}

/// One row of the thread, as `#messages` shows it.
#[derive(Template)]
#[template(path = "web_app/message.html")]
pub(crate) struct MessageView {
    pub event_id: String,
    /// `chief` or `user`, the row's class.
    pub sender: &'static str,
    pub text: String,
    /// A reply's state label, such as `typed`.
    pub state: Option<&'static str>,
}

impl From<AppMessage> for MessageView {
    fn from(message: AppMessage) -> Self {
        Self {
            event_id: message.event_id,
            sender: match message.sender {
                AppSender::Chief => "chief",
                AppSender::User => "user",
            },
            text: message.text,
            state: message.state.map(|state| state.label()),
        }
    }
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

/// `GET /`: the thread page. A store that cannot be read gives `503`.
pub(crate) async fn get_thread(State(state): State<AppState>) -> Response {
    let Ok(messages) = state.store.app_messages_since(thread_start_ms()).await else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let calls = state.calls.as_ref();
    let page = ThreadPage {
        messages: messages.into_iter().map(MessageView::from).collect(),
        public_key: state.public_key.to_string(),
        call_set_up: calls.is_some_and(|calls| calls.set_up()),
        call: calls.map_or_else(CallSection::off, |calls| {
            CallSection::from(&*calls.view().borrow())
        }),
    };
    match page.render() {
        Ok(html) => Html(html).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// `GET /static/{name}`: one of the app's seven files, or `404`.
pub(crate) async fn get_static(Path(name): Path<String>) -> Response {
    let (content_type, bytes) = match name.as_str() {
        "app.css" => ("text/css", APP_CSS),
        "alerts.js" => (JAVASCRIPT, ALERTS_JS),
        "call.js" => (JAVASCRIPT, CALL_JS),
        "call-worklet.js" => (JAVASCRIPT, CALL_WORKLET_JS),
        "sw.js" => (JAVASCRIPT, SW_JS),
        "datastar-1.0.2.js" => (JAVASCRIPT, DATASTAR_JS),
        "manifest.webmanifest" => ("application/manifest+json", MANIFEST),
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
