//! The thread page and the app's own files, all compiled into the binary.

use askama::Template;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Response};

use super::{AppState, DATASTAR_JS, thread_start_ms};
use crate::store::{AppMessage, AppSender};

/// The content security policy every response carries: Harold's own scripts, styles and
/// files only, nothing inline and nothing from another site.
pub(crate) const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; script-src 'self' 'unsafe-eval'; style-src 'self'; img-src 'self'; connect-src 'self'; manifest-src 'self'; worker-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'";

const APP_CSS: &[u8] = include_bytes!("../../static/web_app/app.css");
const ALERTS_JS: &[u8] = include_bytes!("../../static/web_app/alerts.js");
const SW_JS: &[u8] = include_bytes!("../../static/web_app/sw.js");
const MANIFEST: &[u8] = include_bytes!("../../static/web_app/manifest.webmanifest");

const JAVASCRIPT: &str = "text/javascript";

/// The thread: the last `THREAD_DAYS` days of messages, oldest first.
#[derive(Template)]
#[template(path = "web_app/thread.html")]
pub(crate) struct ThreadPage {
    pub messages: Vec<MessageView>,
    /// Harold's VAPID public key, base64url: the page's `applicationServerKey`.
    pub public_key: String,
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
    let page = ThreadPage {
        messages: messages.into_iter().map(MessageView::from).collect(),
        public_key: state.public_key.to_string(),
    };
    match page.render() {
        Ok(html) => Html(html).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// `GET /static/{name}`: one of the app's five files, or `404`.
pub(crate) async fn get_static(Path(name): Path<String>) -> Response {
    let (content_type, bytes) = match name.as_str() {
        "app.css" => ("text/css", APP_CSS),
        "alerts.js" => (JAVASCRIPT, ALERTS_JS),
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
