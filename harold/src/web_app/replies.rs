//! The page's posts and its photo route: a typed reply for the chief, a photo to go with
//! it, the phone's push subscription, and the one word the page says about dictation.
//! Neither the reply's words, the photo, its path nor the subscription's address or keys
//! reach a log line.

use std::convert::Infallible;
use std::path::Path as FsPath;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use datastar::prelude::PatchSignals;
use serde::Deserialize;
use time::OffsetDateTime;
use tokio::io::AsyncWriteExt;
use tracing::{info, warn};
use uuid::Uuid;

use super::AppState;
use super::route::now_ms;
use crate::store::{
    AppPushSubscribed, HaroldStore, append_app_push_subscribed, append_app_reply_with_photo,
};
use crate::telemetry;

/// The largest photo taken, in bytes: the limit Claude has for one image.
pub(crate) const PHOTO_MAX_BYTES: usize = 5 * 1024 * 1024;
/// The longest `_photo` signal taken, in characters: `PHOTO_MAX_BYTES` as base64.
const PHOTO_MAX_BASE64: usize = PHOTO_MAX_BYTES.div_ceil(3) * 4;
/// The body limit of the photo route: 7 MB, which holds a photo of `PHOTO_MAX_BYTES` as
/// base64 with the JSON around it and some more, so the handler, not axum's own refusal,
/// answers a photo that is a little too large.
pub(crate) const PHOTO_BODY_LIMIT: usize = 7 * 1024 * 1024 + 1024;
/// A photo over this size was not shrunk by the phone, which makes one of about 1 MB.
const PHOTO_NOT_SHRUNK_BYTES: usize = 3 * 1024 * 1024;

/// The most characters a reply may hold, after trimming.
pub(crate) const REPLY_MAX_CHARS: usize = 1000;
/// The largest reply body read, in bytes.
const REPLY_BODY_MAX_BYTES: usize = 16 * 1024;
/// The largest dictation check body read, in bytes.
const DICTATION_BODY_MAX_BYTES: usize = 64;
/// What the page may say about dictation: `dictation.js` reports one of these, once per page.
const DICTATION_RESULTS: [&str; 7] = [
    "missing",
    "ok",
    "not_allowed",
    "service_not_allowed",
    "network",
    "no_speech",
    "audio_capture",
];
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
    /// The id `POST /replies/photo` answered; empty or missing when no photo is attached.
    #[serde(default)]
    photo: String,
}

/// The photo as `photo.js` posts it: the `_photo` signal, the resized JPEG as base64.
#[derive(Deserialize)]
struct PhotoSignals {
    #[serde(rename = "_photo")]
    photo: String,
}

/// The check as `dictation.js` posts it.
#[derive(Deserialize)]
struct DictationCheck {
    result: String,
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
/// A blank reply with no photo, one over `REPLY_MAX_CHARS`, a `photo` that names no unused
/// upload, or a body that is not the signals gives `400`; a store that cannot take it gives
/// `503`. Both have empty bodies.
pub(crate) async fn post_reply(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if body.len() > REPLY_BODY_MAX_BYTES {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let Ok(signals) = serde_json::from_slice::<ReplySignals>(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let text = signals.reply.trim();
    let chars = text.chars().count();
    let photo_id = signals.photo.trim();
    if chars > REPLY_MAX_CHARS || (chars == 0 && photo_id.is_empty()) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let mut photo = None;
    if !photo_id.is_empty() {
        match take_photo(&state.store, photo_id).await {
            Ok(Some(file)) => photo = Some((photo_id, file)),
            Ok(None) => {
                warn!(reason = "unknown_photo", "app photo refused");
                return StatusCode::BAD_REQUEST.into_response();
            }
            Err(()) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
        }
    }
    let file = photo.as_ref().map(|(_, file)| file.as_str());
    // The phone's fetch span names itself in `traceparent`; the reply's trace joins it.
    let parent = super::phone_trace(&headers);
    let stored = telemetry::store_phone_reply_in(
        telemetry::WEB_APP_CHANNEL,
        OffsetDateTime::now_utc(),
        parent.as_ref(),
        async {
            append_app_reply_with_photo(&state.store, text, file)
                .await
                .is_ok()
        },
    )
    .await;
    if !stored {
        if let Some((id, _)) = &photo {
            // Not stored, so the photo can go with the next try.
            let _ = state.store.release_app_photo_upload(id).await;
        }
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    match file.and_then(|file| photo_bytes(&state.store.app_photos_dir(), file)) {
        Some(photo_bytes) => info!(chars, photo_bytes, "app reply stored"),
        None => info!(chars, "app reply stored"),
    }
    let cleared = PatchSignals::new(r#"{"reply":"","photo":""}"#);
    Sse::new(tokio_stream::once(Ok::<Event, Infallible>(cleared.into()))).into_response()
}

/// The file name of the unused upload `id`, taken for a reply; none when `id` names no
/// such upload. Err when the store cannot say.
async fn take_photo(store: &HaroldStore, id: &str) -> Result<Option<String>, ()> {
    if Uuid::parse_str(id).is_err() || id.len() != 36 {
        return Ok(None);
    }
    store.claim_app_photo_upload(id).await.map_err(|_| ())
}

/// The size of the photo `file` in `directory`, when it can be read.
fn photo_bytes(directory: &FsPath, file: &str) -> Option<u64> {
    std::fs::metadata(directory.join(file))
        .ok()
        .map(|metadata| metadata.len())
}

/// How a photo starts: the file's kind and extension.
fn photo_kind(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("jpeg")
    } else if bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
        Some("png")
    } else {
        None
    }
}

/// `POST /replies/photo`: the body is the page's `_photo` signal, the photo shrunk by the
/// page, as base64. Answers `200` with the `photo` signal patched to the photo's id once it
/// is written to `app-photos/` and recorded; `413` when the photo is over `PHOTO_MAX_BYTES`,
/// `400` when the body is not that signal or the photo does not start like a JPEG or a
/// PNG, `503` when it cannot be written or recorded.
pub(crate) async fn post_photo(State(state): State<AppState>, body: Bytes) -> Response {
    let Ok(signals) = serde_json::from_slice::<PhotoSignals>(&body) else {
        warn!(reason = "not_an_image", "app photo refused");
        return StatusCode::BAD_REQUEST.into_response();
    };
    if signals.photo.len() > PHOTO_MAX_BASE64 {
        warn!(reason = "too_large", "app photo refused");
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    }
    let Ok(body) = openssl::base64::decode_block(&signals.photo) else {
        warn!(reason = "not_an_image", "app photo refused");
        return StatusCode::BAD_REQUEST.into_response();
    };
    let photo_bytes = body.len();
    if photo_bytes > PHOTO_MAX_BYTES {
        warn!(reason = "too_large", photo_bytes, "app photo refused");
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    }
    let Some(kind) = photo_kind(&body) else {
        warn!(reason = "not_an_image", photo_bytes, "app photo refused");
        return StatusCode::BAD_REQUEST.into_response();
    };
    let id = Uuid::new_v4().to_string();
    let extension = if kind == "png" { "png" } else { "jpg" };
    let file = format!("{id}.{extension}");
    let directory = state.store.app_photos_dir();
    if write_photo(&directory, &file, &body).await.is_err() {
        warn!(reason = "store_failed", photo_bytes, "app photo refused");
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    if state
        .store
        .add_app_photo_upload(&id, &file, now_ms())
        .await
        .is_err()
    {
        let _ = tokio::fs::remove_file(directory.join(&file)).await;
        warn!(reason = "store_failed", photo_bytes, "app photo refused");
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    info!(photo_bytes, kind, "app photo stored");
    if photo_bytes > PHOTO_NOT_SHRUNK_BYTES {
        warn!(photo_bytes, "app photo not shrunk");
    }
    let signal = serde_json::json!({ "photo": id }).to_string();
    super::datastar_events([PatchSignals::new(signal)])
}

/// Writes `bytes` to `file` in `directory`: the folder at mode 0700 and the file at 0600.
async fn write_photo(directory: &FsPath, file: &str, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    tokio::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)
        .await?;
    tokio::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).await?;
    let mut photo = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(directory.join(file))
        .await?;
    if let Err(error) = photo.write_all(bytes).await {
        let _ = tokio::fs::remove_file(directory.join(file)).await;
        return Err(error);
    }
    photo.flush().await
}

/// `GET /replies/photo/{id}`: the photo of upload `id`, as the row shows it. `404` for an id
/// that is not a UUID, names no kept photo, or whose file cannot be read.
pub(crate) async fn get_photo(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    if Uuid::parse_str(&id).is_err() || id.len() != 36 {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Ok(Some(file)) = state.store.app_photo_file(&id).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Ok(bytes) = tokio::fs::read(state.store.app_photos_dir().join(&file)).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let content_type = if file.ends_with(".png") {
        "image/png"
    } else {
        "image/jpeg"
    };
    let mut response = ([(header::CONTENT_TYPE, content_type)], bytes).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=86400"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

/// `POST /dictation/check`: logs the one word the page says about dictation here and
/// answers `204`. A word that is not one of `DICTATION_RESULTS`, a body over 64 bytes, or
/// a body that is not `{"result": "<word>"}` gives `400`, and nothing is logged.
pub(crate) async fn post_dictation_check(body: Bytes) -> Response {
    if body.len() > DICTATION_BODY_MAX_BYTES {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let Ok(check) = serde_json::from_slice::<DictationCheck>(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Some(result) = DICTATION_RESULTS
        .into_iter()
        .find(|known| *known == check.result)
    else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    info!(result, "app dictation check");
    StatusCode::NO_CONTENT.into_response()
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
