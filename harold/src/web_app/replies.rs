//! The page's two posts: a typed reply for the chief, and the phone's push subscription.
//! Neither the reply's words nor the subscription's address or keys reach a log line.

use std::convert::Infallible;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use datastar::prelude::PatchSignals;
use serde::Deserialize;
use tracing::info;

use super::AppState;
use crate::store::{AppPushSubscribed, append_app_push_subscribed, append_app_reply};

/// The most characters a reply may hold, after trimming.
pub(crate) const REPLY_MAX_CHARS: usize = 1000;
/// The largest reply body read, in bytes.
const REPLY_BODY_MAX_BYTES: usize = 16 * 1024;
/// The largest subscription body read, in bytes.
const SUBSCRIPTION_BODY_MAX_BYTES: usize = 4 * 1024;
/// The longest push endpoint taken, in characters.
const ENDPOINT_MAX_CHARS: usize = 2048;
/// The longest subscription key taken, in characters.
const KEY_MAX_CHARS: usize = 256;

/// The page's signals, as Datastar's `@post` sends them.
#[derive(Deserialize)]
struct ReplySignals {
    reply: String,
}

/// The subscription as the page's `alerts.js` posts it.
#[derive(Deserialize)]
struct SubscriptionBody {
    endpoint: String,
    keys: SubscriptionKeys,
}

#[derive(Deserialize)]
struct SubscriptionKeys {
    p256dh: String,
    auth: String,
}

/// `POST /replies`: stores the trimmed reply for the chief and clears the page's input.
/// A blank reply, one over `REPLY_MAX_CHARS`, or a body that is not the signals gives
/// `400`; a store that cannot take it gives `503`. Both have empty bodies.
pub(crate) async fn post_reply(State(state): State<AppState>, body: Bytes) -> Response {
    if body.len() > REPLY_BODY_MAX_BYTES {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let Ok(signals) = serde_json::from_slice::<ReplySignals>(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let text = signals.reply.trim();
    let chars = text.chars().count();
    if chars == 0 || chars > REPLY_MAX_CHARS {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if append_app_reply(&state.store, text).await.is_err() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    info!(chars, "app reply stored");
    let cleared = PatchSignals::new(r#"{"reply":""}"#);
    Sse::new(tokio_stream::once(Ok::<Event, Infallible>(cleared.into()))).into_response()
}

/// `POST /push/subscription`: stores the phone's push subscription and answers `204`.
/// An endpoint that is not `https://` or is too long, a key that is not short base64url,
/// or a body over 4 KiB gives `400`; a store that cannot take it gives `503`.
pub(crate) async fn post_subscription(State(state): State<AppState>, body: Bytes) -> Response {
    if body.len() > SUBSCRIPTION_BODY_MAX_BYTES {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let Ok(subscription) = serde_json::from_slice::<SubscriptionBody>(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let endpoint_ok = subscription.endpoint.starts_with("https://")
        && subscription.endpoint.chars().count() <= ENDPOINT_MAX_CHARS;
    let keys = &subscription.keys;
    if !endpoint_ok || !is_key(&keys.p256dh) || !is_key(&keys.auth) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let event = AppPushSubscribed {
        endpoint: subscription.endpoint,
        p256dh: subscription.keys.p256dh,
        auth: subscription.keys.auth,
    };
    if append_app_push_subscribed(&state.store, &event)
        .await
        .is_err()
    {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    StatusCode::NO_CONTENT.into_response()
}

/// True for non-empty base64url of at most `KEY_MAX_CHARS` characters: `A–Z a–z 0–9 - _`,
/// with up to two `=` of padding at the end.
fn is_key(key: &str) -> bool {
    let unpadded = key.trim_end_matches('=');
    !unpadded.is_empty()
        && key.len() <= KEY_MAX_CHARS
        && key.len() - unpadded.len() <= 2
        && unpadded
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}
