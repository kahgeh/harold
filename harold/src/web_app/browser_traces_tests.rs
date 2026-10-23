use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::browser_traces::path_only;
use super::origin::loopback_hosts;
use super::{AppState, WebAppIdentity, router};
use crate::outbound::fake::CapturedLogs;
use crate::store::HaroldStore;

const LOGIN: &str = "chief@example.com";
const TRACE: &str = "0af7651916cd43dd8448eb211c80319c";
const CANARY: &str = "BROWSER-CANARY-5512";

/// What the collector stand-in did with the batches posted to it.
#[derive(Default)]
struct CollectorState {
    batches: Mutex<Vec<Value>>,
    /// The status it answers with.
    status: Mutex<u16>,
    /// How long it holds each request before answering.
    hold: Mutex<Duration>,
}

struct Collector {
    addr: SocketAddr,
    state: Arc<CollectorState>,
    handle: JoinHandle<()>,
}

impl Collector {
    async fn start() -> Self {
        let state = Arc::new(CollectorState {
            status: Mutex::new(200),
            ..CollectorState::default()
        });
        async fn take(State(state): State<Arc<CollectorState>>, body: String) -> StatusCode {
            let hold = *state.hold.lock().unwrap();
            if !hold.is_zero() {
                tokio::time::sleep(hold).await;
            }
            if let Ok(batch) = serde_json::from_str(&body) {
                state.batches.lock().unwrap().push(batch);
            }
            StatusCode::from_u16(*state.status.lock().unwrap()).unwrap()
        }
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new()
            .route("/v1/traces", post(take))
            .with_state(Arc::clone(&state));
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self {
            addr,
            state,
            handle,
        }
    }

    fn address(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn batches(&self) -> Vec<Value> {
        self.state.batches.lock().unwrap().clone()
    }

    async fn wait_for_batches(&self, count: usize) -> Vec<Value> {
        for _ in 0..200 {
            let batches = self.batches();
            if batches.len() >= count {
                return batches;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "the collector got {} batches, not {count}",
            self.batches().len()
        );
    }
}

impl Drop for Collector {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// The app's router on a free port, with browser spans forwarded to `collector`.
struct App {
    addr: SocketAddr,
    _shutdown: watch::Sender<()>,
    _directory: std::path::PathBuf,
    handle: JoinHandle<()>,
}

impl App {
    async fn start(collector: Option<&str>) -> Self {
        let directory =
            std::env::temp_dir().join(format!("harold-browser-{}", uuid::Uuid::new_v4()));
        let store = Arc::new(HaroldStore::open(directory.join("store")).await.unwrap());
        let (shutdown, shutdown_rx) = watch::channel(());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = AppState::for_tests(
            store,
            LOGIN,
            "BPublicKey",
            Arc::from(loopback_hosts("127.0.0.1:0", addr)),
            shutdown_rx,
            None,
            WebAppIdentity::default(),
        );
        let state = match collector {
            Some(collector) => state.with_browser_collector(collector),
            None => state,
        };
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, router(state)).await;
        });
        Self {
            addr,
            _shutdown: shutdown,
            _directory: directory,
            handle,
        }
    }

    /// Posts `body` to the route as the page does.
    async fn post(&self, body: impl Into<reqwest::Body>) -> reqwest::Response {
        self.post_from(body, "same-origin").await
    }

    async fn post_from(&self, body: impl Into<reqwest::Body>, site: &str) -> reqwest::Response {
        reqwest::Client::new()
            .post(format!("http://{}/telemetry/traces", self.addr))
            .header("Tailscale-User-Login", LOGIN)
            .header("Sec-Fetch-Site", site)
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap()
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.handle.abort();
        let _ = std::fs::remove_dir_all(&self._directory);
    }
}

fn attribute(key: &str, value: Value) -> Value {
    json!({ "key": key, "value": value })
}

fn text(key: &str, value: &str) -> Value {
    attribute(key, json!({ "stringValue": value }))
}

fn span(name: &str, span_id: &str, parent: &str, attributes: Vec<Value>) -> Value {
    json!({
        "traceId": TRACE,
        "spanId": span_id,
        "parentSpanId": parent,
        "name": name,
        "kind": 3,
        "startTimeUnixNano": "1761250000000000000",
        "endTimeUnixNano": "1761250000010000000",
        "attributes": attributes,
        "events": [
            { "timeUnixNano": "1761250000001000000", "name": "fetchStart",
              "attributes": [text("leak", CANARY)] }
        ],
        "links": [{ "traceId": TRACE, "spanId": "00f067aa0ba902b7" }],
        "status": { "code": 1, "message": CANARY },
    })
}

fn batch(scope: &str, spans: Vec<Value>) -> Value {
    json!({ "resourceSpans": [{
        "resource": { "attributes": [text("service.name", "unknown_service")] },
        "scopeSpans": [{ "scope": { "name": scope }, "spans": spans }],
    }]})
}

/// Every span the collector got, flattened.
fn spans_of(batches: &[Value]) -> Vec<Value> {
    batches
        .iter()
        .flat_map(|batch| batch["resourceSpans"].as_array().unwrap())
        .flat_map(|resource| resource["scopeSpans"].as_array().unwrap())
        .flat_map(|scope| scope["spans"].as_array().unwrap().clone())
        .collect()
}

fn attribute_of<'a>(span: &'a Value, key: &str) -> Option<&'a Value> {
    span["attributes"]
        .as_array()?
        .iter()
        .find(|attribute| attribute["key"] == key)
        .map(|attribute| &attribute["value"])
}

/// The six spans a load and a reply gave in the trial.
fn trial_spans() -> Vec<Value> {
    vec![
        span(
            "documentLoad",
            "00f067aa0ba902b1",
            "",
            vec![text("http.url", "https://x/?q=1")],
        ),
        span(
            "documentFetch",
            "00f067aa0ba902b2",
            "00f067aa0ba902b1",
            vec![],
        ),
        span(
            "resourceFetch",
            "00f067aa0ba902b3",
            "00f067aa0ba902b1",
            vec![text(
                "url.full",
                "https://mac.tail.ts.net/static/app.css?v=3",
            )],
        ),
        span(
            "POST",
            "00f067aa0ba902b4",
            "",
            vec![
                text(
                    "url.full",
                    "https://mac.tail.ts.net/replies?datastar=%7B%22reply%22%3A%22hi%22%7D",
                ),
                text("http.request.method", "POST"),
                attribute("http.response.status_code", json!({ "intValue": "200" })),
            ],
        ),
        span(
            "browser.error",
            "00f067aa0ba902b5",
            "",
            vec![
                text("error.kind", "error"),
                text("exception.type", "TypeError"),
                text("code.filepath", "https://mac.tail.ts.net/static/call.js"),
                attribute("code.lineno", json!({ "intValue": 214 })),
                attribute("code.column", json!({ "intValue": "9" })),
            ],
        ),
        span(
            "browser.error",
            "00f067aa0ba902b6",
            "",
            vec![text("error.kind", "unhandledrejection")],
        ),
    ]
}

#[tokio::test(flavor = "current_thread")]
async fn the_trials_six_spans_are_forwarded_with_path_only_urls_and_only_kept_attributes() {
    let collector = Collector::start().await;
    let app = App::start(Some(&collector.address())).await;

    let response = app
        .post(batch("@opentelemetry/instrumentation-fetch", trial_spans()).to_string())
        .await;

    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "{}");
    let batches = collector.wait_for_batches(1).await;
    let spans = spans_of(&batches);
    assert_eq!(spans.len(), 6);
    let resource = &batches[0]["resourceSpans"][0]["resource"];
    assert_eq!(
        attribute_of(resource_as_span(resource), "service.name"),
        Some(&json!({ "stringValue": "harold-web-app" }))
    );
    let by_name = |name: &str| {
        spans
            .iter()
            .filter(|span| span["name"] == name)
            .collect::<Vec<_>>()
    };
    let post = by_name("POST")[0];
    assert_eq!(
        attribute_of(post, "url.full"),
        Some(&json!({ "stringValue": "/replies" }))
    );
    assert_eq!(
        attribute_of(post, "http.request.method"),
        Some(&json!({ "stringValue": "POST" }))
    );
    assert_eq!(
        attribute_of(post, "http.response.status_code"),
        Some(&json!({ "intValue": "200" }))
    );
    assert_eq!(post["traceId"], TRACE);
    assert_eq!(post["events"][0]["name"], "fetchStart");
    let resource_fetch = by_name("resourceFetch")[0];
    assert_eq!(
        attribute_of(resource_fetch, "url.full"),
        Some(&json!({ "stringValue": "/static/app.css" }))
    );
    assert_eq!(resource_fetch["parentSpanId"], "00f067aa0ba902b1");
    let error = by_name("browser.error")[0];
    assert_eq!(
        attribute_of(error, "code.filepath"),
        Some(&json!({ "stringValue": "/static/call.js" }))
    );
    assert_eq!(
        attribute_of(error, "code.lineno"),
        Some(&json!({ "intValue": "214" }))
    );
    assert_eq!(
        attribute_of(error, "code.column"),
        Some(&json!({ "intValue": "9" }))
    );
    // `http.url` is not on the list, whatever the span.
    assert!(attribute_of(by_name("documentLoad")[0], "http.url").is_none());
    let sent = serde_json::to_string(&batches).unwrap();
    assert!(!sent.contains('?'), "{sent}");
    assert!(!sent.contains("datastar"), "{sent}");
}

/// The resource as a span-shaped value, for `attribute_of`.
fn resource_as_span(resource: &Value) -> &Value {
    resource
}

#[tokio::test(flavor = "current_thread")]
async fn a_span_loses_a_query_an_unknown_attribute_a_status_message_and_a_link() {
    let collector = Collector::start().await;
    let app = App::start(Some(&collector.address())).await;
    let leaky = span(
        "POST",
        "00f067aa0ba902b4",
        "",
        vec![
            text(
                "url.full",
                &format!(
                    "https://mac.tail.ts.net/todos/answer?datastar={{\"reply\":\"{CANARY}\"}}#top"
                ),
            ),
            text("draft", CANARY),
            text("http.url", CANARY),
        ],
    );

    app.post(batch("@opentelemetry/instrumentation-fetch", vec![leaky]).to_string())
        .await;

    let batches = collector.wait_for_batches(1).await;
    let sent = serde_json::to_string(&batches).unwrap();
    assert!(!sent.contains(CANARY), "{sent}");
    let span = &spans_of(&batches)[0];
    assert_eq!(
        attribute_of(span, "url.full"),
        Some(&json!({ "stringValue": "/todos/answer" }))
    );
    assert_eq!(span["attributes"].as_array().unwrap().len(), 1);
    assert!(span.get("links").is_none());
    assert_eq!(span["status"], json!({ "code": 1 }));
    assert!(span["events"][0].get("attributes").is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn spans_from_an_unknown_scope_are_dropped() {
    let collector = Collector::start().await;
    let app = App::start(Some(&collector.address())).await;
    let mut mixed = batch("some-other-library", trial_spans());
    mixed["resourceSpans"][0]["scopeSpans"].as_array_mut().unwrap().push(
        json!({ "scope": { "name": "harold-web-app" }, "spans": [
            span("browser.message_shown", "00f067aa0ba902c1", "00f067aa0ba902b4", vec![text("harold.row", "chief")])
        ]}),
    );

    app.post(mixed.to_string()).await;

    let spans = spans_of(&collector.wait_for_batches(1).await);
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0]["name"], "browser.message_shown");
    assert_eq!(
        attribute_of(&spans[0], "harold.row"),
        Some(&json!({ "stringValue": "chief" }))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_message_is_kept_only_on_a_browser_error_and_cut_to_200_characters() {
    let collector = Collector::start().await;
    let app = App::start(Some(&collector.address())).await;
    let long = "x".repeat(300);
    let spans = vec![
        span(
            "browser.error",
            "00f067aa0ba902b5",
            "",
            vec![text("exception.message", &long)],
        ),
        span(
            "POST",
            "00f067aa0ba902b6",
            "",
            vec![text("exception.message", "no"), text("harold.row", "chief")],
        ),
    ];

    app.post(batch("harold-web-app", spans).to_string()).await;

    let spans = spans_of(&collector.wait_for_batches(1).await);
    let message = attribute_of(&spans[0], "exception.message").unwrap()["stringValue"]
        .as_str()
        .unwrap();
    assert_eq!(message.chars().count(), 200);
    assert!(spans[1].get("attributes").is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn a_body_over_256_kb_gets_413_and_anything_else_that_is_not_otlp_json_gets_400() {
    let collector = Collector::start().await;
    let app = App::start(Some(&collector.address())).await;

    assert_eq!(app.post("x".repeat(256 * 1024 + 1)).await.status(), 413);
    assert_eq!(app.post("not json").await.status(), 400);
    assert_eq!(app.post(r#"{"spans":[]}"#).await.status(), 400);
    assert_eq!(app.post("[]").await.status(), 400);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(collector.batches().is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn a_post_from_another_origin_gets_403_and_forwards_nothing() {
    let collector = Collector::start().await;
    let app = App::start(Some(&collector.address())).await;
    let body = batch("harold-web-app", trial_spans()).to_string();

    let response = app.post_from(body, "cross-site").await;

    assert_eq!(response.status(), 403);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(collector.batches().is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn with_export_off_a_post_gets_200_and_nothing_is_forwarded() {
    let collector = Collector::start().await;
    let app = App::start(None).await;

    let response = app
        .post(batch("harold-web-app", trial_spans()).to_string())
        .await;

    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "{}");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(collector.batches().is_empty());
}

fn count(reason: &str) -> u64 {
    crate::telemetry::test_support::exact_counter_value(
        "harold.app_spans.not_forwarded",
        &[("reason", reason)],
    )
    .unwrap_or(0)
}

#[tokio::test(flavor = "current_thread")]
async fn a_refusing_collector_gives_one_line_then_one_line_on_recovery_and_counts_the_spans() {
    crate::telemetry::test_support::install();
    let logs = CapturedLogs::start();
    let collector = Collector::start().await;
    let app = App::start(Some(&collector.address())).await;
    let body = batch("harold-web-app", trial_spans()).to_string();
    let before = count("collector_refused");
    *collector.state.status.lock().unwrap() = 503;

    for _ in 0..2 {
        app.post(body.clone()).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    assert_eq!(count("collector_refused") - before, 12);
    let text = logs.text();
    assert_eq!(
        text.matches("app traces not forwarded").count(),
        1,
        "{text}"
    );
    assert!(text.contains(r#"reason="collector_refused""#), "{text}");
    assert!(!text.contains("forwarded again"), "{text}");

    *collector.state.status.lock().unwrap() = 200;
    app.post(body.clone()).await;
    app.post(body).await;
    tokio::time::sleep(Duration::from_millis(150)).await;

    let text = logs.text();
    assert_eq!(
        text.matches("app traces forwarded again").count(),
        1,
        "{text}"
    );
    assert_eq!(
        text.matches("app traces not forwarded").count(),
        1,
        "{text}"
    );
    assert!(!text.contains("BROWSER-CANARY"), "{text}");
}

#[tokio::test(flavor = "current_thread")]
async fn a_collector_nothing_listens_on_is_unreachable() {
    crate::telemetry::test_support::install();
    let closed = {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap()
    };
    let app = App::start(Some(&format!("http://{closed}"))).await;
    let before = count("collector_unreachable");

    app.post(batch("harold-web-app", trial_spans()).to_string())
        .await;
    tokio::time::sleep(Duration::from_millis(150)).await;

    assert_eq!(count("collector_unreachable") - before, 6);
}

#[tokio::test(flavor = "current_thread")]
async fn a_batch_beyond_four_forwards_at_once_is_dropped_and_counted() {
    crate::telemetry::test_support::install();
    let collector = Collector::start().await;
    *collector.state.hold.lock().unwrap() = Duration::from_millis(1200);
    let app = App::start(Some(&collector.address())).await;
    let body = batch("harold-web-app", trial_spans()).to_string();
    let before = count("queue_full");

    for _ in 0..5 {
        app.post(body.clone()).await;
    }

    assert_eq!(count("queue_full") - before, 6);
    assert_eq!(collector.wait_for_batches(4).await.len(), 4);
}

#[test]
fn a_path_is_cut_at_the_query_and_the_fragment_and_loses_scheme_and_host() {
    assert_eq!(
        path_only("https://a.b.ts.net:8443/replies?x=1#y"),
        "/replies"
    );
    assert_eq!(path_only("http://127.0.0.1:1/"), "/");
    assert_eq!(path_only("/static/call.js?v=2"), "/static/call.js");
    assert_eq!(path_only("https://host"), "");
}
