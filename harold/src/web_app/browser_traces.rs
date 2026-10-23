//! `POST /telemetry/traces`: the spans the web app records in the phone's browser, kept to
//! an allowed list of fields and forwarded to the collector.
//!
//! The phone cannot reach the collector's port, so the page posts each batch here. Harold
//! rebuilds the batch from the fields it allows, so a query string, a message or any text
//! the user typed cannot go further, and sends it on in the background. No span content
//! reaches a log line.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value, json};
use tokio::sync::Semaphore;
use tracing::{info, warn};

use super::AppState;
use crate::telemetry;

/// The largest batch read, in bytes.
pub(crate) const BODY_LIMIT: usize = 256 * 1024;

/// How long one forward may take before it gives up.
const FORWARD_TIMEOUT: Duration = Duration::from_secs(2);

/// How many forwards run at once.
const FORWARDS_AT_ONCE: usize = 4;

/// The service the collector stores the spans under.
const SERVICE_NAME: &str = "harold-web-app";

/// The scopes whose spans are kept: document load, fetch and the page's own script.
const SCOPES: [&str; 3] = [
    "@opentelemetry/instrumentation-document-load",
    "@opentelemetry/instrumentation-fetch",
    SERVICE_NAME,
];

/// The most characters `exception.message` keeps.
const MESSAGE_CHARS: usize = 200;

/// The most characters a path, a name or another kept text keeps.
const TEXT_CHARS: usize = 512;

/// Where the browser spans go, and whether a batch is forwarded at all.
pub(crate) struct BrowserTraces {
    /// The collector's traces address; `None` is export off, and a batch is dropped.
    url: Option<String>,
    client: reqwest::Client,
    forwards: Arc<Semaphore>,
    /// Whether the last forward failed, so one line is logged when it starts and one when
    /// it recovers.
    failing: Arc<AtomicBool>,
}

impl BrowserTraces {
    /// Forwards to `collector`, the collector's OTLP HTTP address, when there is one.
    pub(crate) fn new(collector: Option<&str>) -> Self {
        Self {
            url: collector
                .map(|collector| format!("{}/v1/traces", collector.trim_end_matches('/'))),
            client: reqwest::Client::builder()
                .timeout(FORWARD_TIMEOUT)
                .build()
                .unwrap_or_default(),
            forwards: Arc::new(Semaphore::new(FORWARDS_AT_ONCE)),
            failing: Arc::default(),
        }
    }

    /// Whether the page records and sends browser spans.
    pub(crate) fn on(&self) -> bool {
        self.url.is_some()
    }

    /// Sends `batch`, holding `spans` spans, in the background. A batch beyond the
    /// forwards at once is dropped and counted.
    fn forward(&self, batch: Value, spans: u64) {
        let Some(url) = self.url.clone() else {
            return;
        };
        let Ok(permit) = Arc::clone(&self.forwards).try_acquire_owned() else {
            telemetry::count_app_spans_not_forwarded(spans, "queue_full");
            return;
        };
        let client = self.client.clone();
        let failing = Arc::clone(&self.failing);
        tokio::spawn(async move {
            let sent = client.post(url).json(&batch).send().await;
            drop(permit);
            match sent {
                Ok(response) if response.status().is_success() => {
                    if failing.swap(false, Ordering::Relaxed) {
                        info!("app traces forwarded again");
                    }
                }
                Ok(_) => not_forwarded(&failing, spans, "collector_refused"),
                Err(error) if error.is_timeout() => not_forwarded(&failing, spans, "timed_out"),
                Err(_) => not_forwarded(&failing, spans, "collector_unreachable"),
            }
        });
    }
}

/// Counts the spans a failed forward lost, and logs the first failure of a run.
fn not_forwarded(failing: &AtomicBool, spans: u64, reason: &'static str) {
    telemetry::count_app_spans_not_forwarded(spans, reason);
    if !failing.swap(true, Ordering::Relaxed) {
        warn!(reason, "app traces not forwarded");
    }
}

/// `POST /telemetry/traces`: keeps the allowed fields of the batch and forwards it. Answers
/// `200 {}` at once, `413` for a body over `BODY_LIMIT` and `400` for one that is not OTLP
/// JSON. With export off the batch is dropped and the answer is the same `200 {}`.
pub(crate) async fn post_traces(State(state): State<AppState>, body: Bytes) -> Response {
    if body.len() > BODY_LIMIT {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    }
    let Ok(request) = serde_json::from_slice::<Value>(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Some(resource_spans) = request.get("resourceSpans").and_then(Value::as_array) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if state.browser_traces.on() {
        let (batch, spans) = keep_allowed(resource_spans);
        if spans > 0 {
            state.browser_traces.forward(batch, spans);
        }
    }
    ([(header::CONTENT_TYPE, "application/json")], "{}").into_response()
}

/// A batch of only the spans from the expected scopes, each reduced to its allowed fields,
/// with the resource Harold sets, and how many spans it holds.
fn keep_allowed(resource_spans: &[Value]) -> (Value, u64) {
    let mut scopes = Vec::new();
    let mut count = 0;
    for scope_spans in resource_spans
        .iter()
        .filter_map(|resource| resource.get("scopeSpans").and_then(Value::as_array))
        .flatten()
    {
        let name = scope_spans
            .pointer("/scope/name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let Some(scope) = SCOPES.iter().find(|scope| **scope == name) else {
            continue;
        };
        let spans: Vec<Value> = scope_spans
            .get("spans")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|span| keep_span(span, scope))
            .collect();
        count += spans.len() as u64;
        if !spans.is_empty() {
            scopes.push(json!({ "scope": { "name": scope }, "spans": spans }));
        }
    }
    let batch = json!({
        "resourceSpans": [{
            "resource": { "attributes": [
                string_attribute("service.name", SERVICE_NAME),
                string_attribute("service.version", env!("CARGO_PKG_VERSION")),
            ]},
            "scopeSpans": scopes,
        }]
    });
    (batch, count)
}

fn string_attribute(key: &str, value: &str) -> Value {
    json!({ "key": key, "value": { "stringValue": value } })
}

/// The span reduced to its trace id, span id, parent, name, kind, times, status code, kept
/// attributes and events' names and times. None for a span whose ids or times are not
/// well formed.
fn keep_span(span: &Value, scope: &str) -> Option<Value> {
    let trace_id = hex(span.get("traceId")?, 32)?;
    let span_id = hex(span.get("spanId")?, 16)?;
    let parent = span
        .get("parentSpanId")
        .and_then(Value::as_str)
        .filter(|parent| !parent.is_empty());
    let name = plain_name(span.get("name")?.as_str()?)?;
    let start = nanos(span.get("startTimeUnixNano")?)?;
    let end = nanos(span.get("endTimeUnixNano")?)?;
    let mut kept = Map::new();
    kept.insert("traceId".into(), trace_id.into());
    kept.insert("spanId".into(), span_id.into());
    if let Some(parent) = parent {
        kept.insert("parentSpanId".into(), hex(&json!(parent), 16)?.into());
    }
    kept.insert("name".into(), name.clone().into());
    kept.insert("kind".into(), small_number(span.get("kind"), 5).into());
    kept.insert("startTimeUnixNano".into(), start.into());
    kept.insert("endTimeUnixNano".into(), end.into());
    kept.insert(
        "status".into(),
        json!({ "code": small_number(span.pointer("/status/code"), 2) }),
    );
    let attributes = keep_attributes(span.get("attributes"), &name, scope);
    if !attributes.is_empty() {
        kept.insert("attributes".into(), attributes.into());
    }
    let events = keep_events(span.get("events"));
    if !events.is_empty() {
        kept.insert("events".into(), events.into());
    }
    Some(kept.into())
}

/// `value` when it is a string of exactly `length` lowercase hex digits.
fn hex(value: &Value, length: usize) -> Option<String> {
    let text = value.as_str()?;
    (text.len() == length && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
        .then(|| text.to_owned())
}

/// A time in nanoseconds as the string OTLP JSON holds it, from a string or a number of
/// digits.
fn nanos(value: &Value) -> Option<String> {
    let text = match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.as_u64()?.to_string(),
        _ => return None,
    };
    (!text.is_empty() && text.len() <= 20 && text.bytes().all(|b| b.is_ascii_digit()))
        .then_some(text)
}

/// A number from 0 to `max`, or 0.
fn small_number(value: Option<&Value>, max: u64) -> u64 {
    value
        .and_then(Value::as_u64)
        .filter(|number| *number <= max)
        .unwrap_or(0)
}

/// A span or event name made of letters, digits and `_ . - / :` and spaces only, which
/// leaves room for `documentFetch` and `HTTP POST` but no query string or message. None
/// for any other name.
fn plain_name(name: &str) -> Option<String> {
    let plain = !name.is_empty()
        && name.chars().count() <= 80
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-' | '/' | ':' | ' '));
    plain.then(|| name.to_owned())
}

/// The events' names and times: the browser's timing marks, such as `fetchStart`.
fn keep_events(events: Option<&Value>) -> Vec<Value> {
    events
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|event| {
            let name = plain_name(event.get("name")?.as_str()?)?;
            let time = nanos(event.get("timeUnixNano")?)?;
            Some(json!({ "name": name, "timeUnixNano": time }))
        })
        .collect()
}

/// The attributes on the allowed list, each in its allowed shape.
fn keep_attributes(attributes: Option<&Value>, span_name: &str, scope: &str) -> Vec<Value> {
    attributes
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|attribute| {
            let key = attribute.get("key")?.as_str()?;
            let value = attribute.get("value")?;
            keep_attribute(key, value, span_name, scope)
        })
        .collect()
}

fn keep_attribute(key: &str, value: &Value, span_name: &str, scope: &str) -> Option<Value> {
    let text = || value.get("stringValue").and_then(Value::as_str);
    let number = || integer(value);
    match key {
        "url.full" | "code.filepath" => Some(string_attribute(key, &path_only(text()?))),
        "http.request.method" => Some(string_attribute(key, &cut(text()?, 16))),
        "exception.type" => Some(string_attribute(key, &cut(text()?, 100))),
        "user_agent.original" => Some(string_attribute(key, &cut(text()?, 256))),
        "exception.message" if span_name == "browser.error" => {
            Some(string_attribute(key, &cut(text()?, MESSAGE_CHARS)))
        }
        "error.kind" if matches!(text()?, "error" | "unhandledrejection") => {
            Some(string_attribute(key, text()?))
        }
        "harold.row"
            if scope == SERVICE_NAME
                && span_name == "browser.message_shown"
                && matches!(text()?, "chief" | "me") =>
        {
            Some(string_attribute(key, text()?))
        }
        "http.response.status_code"
        | "http.response_content_length"
        | "code.lineno"
        | "code.column" => {
            Some(json!({ "key": key, "value": { "intValue": number()?.to_string() } }))
        }
        _ => None,
    }
}

/// An integer attribute value, which OTLP JSON writes as a string or a number.
fn integer(value: &Value) -> Option<i64> {
    match value.get("intValue")? {
        Value::String(text) => text.parse().ok(),
        Value::Number(number) => number.as_i64(),
        _ => None,
    }
}

/// The first `limit` characters of `text`.
fn cut(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

/// `address` as its path alone: no scheme, host, query or fragment.
pub(super) fn path_only(address: &str) -> String {
    let without_scheme = match address.split_once("://") {
        Some((_, rest)) => rest.find('/').map_or("", |at| &rest[at..]),
        None => address,
    };
    let end = without_scheme
        .find(['?', '#'])
        .unwrap_or(without_scheme.len());
    cut(&without_scheme[..end], TEXT_CHARS)
}
