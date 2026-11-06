//! The outermost layer of the web app. It counts each request by route class and result,
//! and records the time to the first byte. It never reads a header or a body.

use std::time::Instant;

use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::telemetry::{count_web_request, record_web_request_duration};

/// A response extension. A check that refuses a request sets it to the reason, which is
/// one of `WEB_REFUSAL_REASONS`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Refusal(pub &'static str);

/// A refusal response: `status` with an empty body, marked with `reason` for the layer.
pub(crate) fn refuse(status: StatusCode, reason: &'static str) -> Response {
    let mut response = status.into_response();
    response.extensions_mut().insert(Refusal(reason));
    response
}

/// The route class of a path or of a route pattern. The result is always one of
/// `WEB_ROUTE_CLASSES`. Only the first segment of the path decides the class, and every
/// route in `router` has its own first segment, so a raw path never becomes a class.
pub(crate) fn route_class(path: &str) -> &'static str {
    let first = path.trim_start_matches('/').split('/').next().unwrap_or("");
    match first {
        "" | "thread" => "thread",
        "updates" => "updates",
        "replies" | "dictation" | "push" => "replies",
        "todos" => "todos",
        "notes" => "notes",
        "specs" => "specs",
        "c" => "pages",
        "call" | "echo-test" => "call",
        "pair" => "pair",
        "static" => "static",
        "telemetry" => "telemetry",
        "page" => "page_events",
        _ => "other",
    }
}

/// The status class of a status code.
fn status_class(status: StatusCode) -> &'static str {
    match status.as_u16() {
        300..=399 => "3xx",
        400..=499 => "4xx",
        500..=599 => "5xx",
        _ => "2xx",
    }
}

/// Counts the request when its response head is ready. The time to the first byte is the
/// time that the next layer takes to return the head.
pub(crate) async fn track_requests(request: Request, next: Next) -> Response {
    let class = route_class(request.uri().path());
    let started = Instant::now();
    let response = next.run(request).await;
    let seconds = started.elapsed().as_secs_f64();
    let status = response.status();
    let refusal = response.extensions().get::<Refusal>().copied();
    let (result, reason) = match refusal {
        Some(Refusal(reason)) => ("refused", reason),
        None if status == StatusCode::NOT_FOUND => ("not_found", ""),
        None if status.is_server_error() => ("error", ""),
        None => ("served", ""),
    };
    count_web_request(class, result, status_class(status), reason);
    // A stream of updates stays open, so its head time says nothing.
    if class != "updates" {
        record_web_request_duration(class, seconds);
    }
    response
}
