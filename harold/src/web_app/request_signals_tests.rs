//! The outermost request layer counts by route class and result. No test opens a port:
//! the router serves one connection over an in-memory pipe.

use std::sync::Arc;

use axum::Router;
use axum::http::StatusCode;
use axum::middleware::from_fn;
use axum::routing::get;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::sync::watch;

use super::request_signals::{Refusal, route_class, track_requests};
use super::{AppState, router};
use crate::outbound::fake::CapturedLogs;
use crate::store::HaroldStore;
use crate::telemetry::{WEB_ROUTE_CLASSES, test_support};

const LOGIN: &str = "me@example.com";

/// A listener that hands out one in-memory connection and then waits for ever.
struct OneConnection(Option<DuplexStream>);

impl axum::serve::Listener for OneConnection {
    type Io = DuplexStream;
    type Addr = ();

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.0.take() {
            Some(io) => (io, ()),
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        Ok(())
    }
}

/// Sends one request to `app` over an in-memory pipe and returns the status code.
async fn status_of(app: Router, path: &str, extra_headers: &str) -> u16 {
    send(app, "GET", path, extra_headers).await
}

/// Sends one request with `method` to `app` over an in-memory pipe and returns the status code.
async fn send(app: Router, method: &str, path: &str, extra_headers: &str) -> u16 {
    let (mut client, server) = tokio::io::duplex(64 * 1024);
    let serving = tokio::spawn(async move {
        let _ = axum::serve(OneConnection(Some(server)), app).await;
    });
    let request = format!(
        "{method} {path} HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\ncontent-length: 0\r\n{extra_headers}\r\n"
    );
    client.write_all(request.as_bytes()).await.unwrap();
    let mut reply = Vec::new();
    client.read_to_end(&mut reply).await.unwrap();
    serving.abort();
    let text = String::from_utf8_lossy(&reply);
    text.split(' ').nth(1).unwrap().parse().unwrap()
}

async fn harold_router() -> (Router, std::path::PathBuf) {
    let directory =
        std::env::temp_dir().join(format!("harold-web-signals-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let store = Arc::new(HaroldStore::open(directory.join("store")).await.unwrap());
    let (_shutdown, shutdown_rx) = watch::channel(());
    let state = AppState::for_tests(
        store,
        LOGIN,
        "BPublicKeyForTheTestPage",
        Arc::from(vec!["localhost".to_owned()]),
        shutdown_rx,
        None,
        Default::default(),
    );
    (router(state), directory)
}

fn requests(class: &str, result: &str, status: &str, reason: &str) -> u64 {
    test_support::exact_counter_value(
        "harold.web_app.requests",
        &[
            ("route_class", class),
            ("result", result),
            ("status", status),
            ("reason", reason),
        ],
    )
    .unwrap_or(0)
}

#[test]
fn route_class_maps_each_route_to_one_of_the_thirteen_classes() {
    let table = [
        ("/", "thread"),
        ("/thread/older", "thread"),
        ("/updates", "updates"),
        ("/replies", "replies"),
        ("/replies/photo/{id}", "replies"),
        ("/dictation/check", "replies"),
        ("/push/subscription", "replies"),
        ("/todos/tick", "todos"),
        ("/notes", "notes"),
        ("/notes/{*name}", "notes"),
        ("/specs/{product}/{id}", "specs"),
        ("/c/{collection}", "pages"),
        ("/c/a/b", "pages"),
        ("/call/answer/{id}", "call"),
        ("/echo-test/runs", "call"),
        ("/pair", "pair"),
        ("/static/{name}", "static"),
        ("/telemetry/traces", "telemetry"),
        ("/page/seen", "page_events"),
        ("/speech-check/runs", "other"),
        ("/nothing-here", "other"),
        ("/notesx", "other"),
    ];
    for (path, class) in table {
        assert_eq!(route_class(path), class, "{path}");
        assert!(WEB_ROUTE_CLASSES.contains(&route_class(path)), "{path}");
    }
    for class in WEB_ROUTE_CLASSES {
        assert!(
            table.iter().any(|(_, found)| *found == class),
            "no path for {class}"
        );
    }
}

#[tokio::test]
async fn served_request_counts_with_route_class_and_status() {
    test_support::install();
    let (app, directory) = harold_router().await;
    let before = requests("thread", "served", "2xx", "");
    let status = status_of(app, "/", &format!("tailscale-user-login: {LOGIN}\r\n")).await;
    assert_eq!(status, 200);
    assert!(requests("thread", "served", "2xx", "") > before);
    let _ = std::fs::remove_dir_all(directory);
}

#[tokio::test]
async fn unknown_path_counts_not_found() {
    test_support::install();
    let (app, directory) = harold_router().await;
    let before = requests("other", "not_found", "4xx", "");
    let status = status_of(
        app,
        "/no-such-page",
        &format!("tailscale-user-login: {LOGIN}\r\n"),
    )
    .await;
    assert_eq!(status, 404);
    assert!(requests("other", "not_found", "4xx", "") > before);
    let _ = std::fs::remove_dir_all(directory);
}

#[tokio::test]
async fn raw_path_never_becomes_route_class() {
    test_support::install();
    let (app, directory) = harold_router().await;
    let before = requests("notes", "not_found", "4xx", "");
    // The app has no notes service here, so the note route answers 404.
    let status = status_of(
        app,
        "/notes/secret-name",
        &format!("tailscale-user-login: {LOGIN}\r\n"),
    )
    .await;
    assert_eq!(status, 404);
    assert!(requests("notes", "not_found", "4xx", "") > before);
    assert_eq!(
        test_support::exact_counter_value(
            "harold.web_app.requests",
            &[("route_class", "/notes/secret-name")]
        ),
        None
    );
    assert_eq!(route_class("/c/x/y"), "pages");
    let _ = std::fs::remove_dir_all(directory);
}

#[tokio::test]
async fn handler_error_counts_error() {
    test_support::install();
    let app = Router::new()
        .route(
            "/telemetry/boom",
            get(|| async { StatusCode::INTERNAL_SERVER_ERROR }),
        )
        .layer(from_fn(track_requests));
    let before = requests("telemetry", "error", "5xx", "");
    assert_eq!(status_of(app, "/telemetry/boom", "").await, 500);
    assert!(requests("telemetry", "error", "5xx", "") > before);
}

#[tokio::test]
async fn refusal_extension_counts_refused_with_its_reason() {
    test_support::install();
    let app = Router::new()
        .route(
            "/call/refused",
            get(|| async {
                let mut response =
                    axum::response::IntoResponse::into_response(StatusCode::FORBIDDEN);
                response.extensions_mut().insert(Refusal("not_datastar"));
                response
            }),
        )
        .layer(from_fn(track_requests));
    let before = requests("call", "refused", "4xx", "not_datastar");
    assert_eq!(status_of(app, "/call/refused", "").await, 403);
    assert!(requests("call", "refused", "4xx", "not_datastar") > before);
}

/// The router with the device check on, and the folder that holds its files.
async fn router_with_pairing() -> (Router, std::path::PathBuf) {
    let directory =
        std::env::temp_dir().join(format!("harold-web-signals-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let store = Arc::new(HaroldStore::open(directory.join("store")).await.unwrap());
    let (_shutdown, shutdown_rx) = watch::channel(());
    let pairing = super::pair::Pairing::load(
        directory.join("devices.json"),
        directory.join("code"),
        true,
        Arc::new(std::time::Instant::now),
    )
    .unwrap();
    let state = AppState::for_tests(
        store,
        LOGIN,
        "BPublicKeyForTheTestPage",
        Arc::from(vec!["localhost".to_owned()]),
        shutdown_rx,
        None,
        Default::default(),
    )
    .with_pairing(pairing);
    (router(state), directory)
}

const LOGIN_LINE: &str = "tailscale-user-login: me@example.com\r\n";

#[tokio::test(flavor = "current_thread")]
async fn no_login_counts_refused_and_logs_once_per_hour() {
    test_support::install();
    let logs = CapturedLogs::start();
    let (app, directory) = harold_router().await;
    let before = requests("todos", "refused", "4xx", "no_login");
    for _ in 0..2 {
        assert_eq!(status_of(app.clone(), "/todos/tick", "").await, 403);
    }
    // Other tests share the counter: a rise of at least two is the check possible.
    assert!(requests("todos", "refused", "4xx", "no_login") >= before + 2);
    let text = logs.text();
    assert_eq!(text.matches("app login refused").count(), 1, "{text}");
    assert!(text.contains("todos"), "{text}");
    assert!(!text.contains("/todos/tick"), "{text}");
    let _ = std::fs::remove_dir_all(directory);
}

#[tokio::test(flavor = "current_thread")]
async fn refusals_count_every_time_while_the_log_stays_once_per_hour() {
    test_support::install();
    let logs = CapturedLogs::start();
    let (app, directory) = router_with_pairing().await;
    let before = requests("pages", "refused", "4xx", "no_cookie");
    for _ in 0..2 {
        assert_eq!(status_of(app.clone(), "/c/none", LOGIN_LINE).await, 401);
    }
    // Other tests share the counter: a rise of at least two is the check possible.
    assert!(requests("pages", "refused", "4xx", "no_cookie") >= before + 2);
    let text = logs.text();
    assert_eq!(text.matches("app device refused").count(), 1, "{text}");
    let _ = std::fs::remove_dir_all(directory);
}

#[tokio::test(flavor = "current_thread")]
async fn an_unknown_cookie_counts_unknown_device() {
    test_support::install();
    let (app, directory) = router_with_pairing().await;
    let before = requests("notes", "refused", "4xx", "unknown_device");
    let headers = format!("{LOGIN_LINE}cookie: harold_device=nobody\r\n");
    assert_eq!(status_of(app, "/notes", &headers).await, 401);
    assert!(requests("notes", "refused", "4xx", "unknown_device") > before);
    let _ = std::fs::remove_dir_all(directory);
}

#[tokio::test(flavor = "current_thread")]
async fn a_cross_origin_post_counts_origin() {
    test_support::install();
    let (app, directory) = harold_router().await;
    let before = requests("replies", "refused", "4xx", "origin");
    let headers = format!("{LOGIN_LINE}sec-fetch-site: cross-site\r\n");
    assert_eq!(send(app, "POST", "/replies", &headers).await, 403);
    assert!(requests("replies", "refused", "4xx", "origin") > before);
    let _ = std::fs::remove_dir_all(directory);
}

#[tokio::test(flavor = "current_thread")]
async fn a_same_origin_post_without_datastar_counts_not_datastar() {
    test_support::install();
    let (app, directory) = harold_router().await;
    let before = requests("specs", "refused", "4xx", "not_datastar");
    let headers = format!("{LOGIN_LINE}sec-fetch-site: same-origin\r\n");
    assert_eq!(send(app, "POST", "/specs/x/y", &headers).await, 403);
    assert!(requests("specs", "refused", "4xx", "not_datastar") > before);
    let _ = std::fs::remove_dir_all(directory);
}

/// A router that mounts the layer around one handler. The handler waits `seconds` first.
fn slow_router(path: &'static str, seconds: u64) -> Router {
    Router::new()
        .route(
            path,
            get(move || async move {
                tokio::time::sleep(std::time::Duration::from_secs(seconds)).await;
                StatusCode::OK
            }),
        )
        .layer(from_fn(track_requests))
}

fn waiting(class: &str) -> u64 {
    test_support::exact_counter_value("harold.web_app.requests.waiting", &[("route_class", class)])
        .unwrap_or(0)
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn handler_held_six_seconds_logs_waiting_once() {
    test_support::install();
    let logs = CapturedLogs::start();
    let waiting_before = waiting("telemetry");
    let served_before = requests("telemetry", "served", "2xx", "");
    let status = status_of(slow_router("/telemetry/slow", 6), "/telemetry/slow", "").await;
    assert_eq!(status, 200);
    assert_eq!(waiting("telemetry"), waiting_before + 1);
    // The answer counts as served, so the served count rises. Waiting does not change the result.
    // Other tests share the counter: a rise of at least one is the check possible.
    assert!(requests("telemetry", "served", "2xx", "") > served_before);
    let text = logs.text();
    assert_eq!(text.matches("app request waiting").count(), 1, "{text}");
    assert!(text.contains("telemetry"), "{text}");
    assert!(text.contains("waited_ms"), "{text}");
    assert!(!text.contains("/telemetry/slow"), "{text}");
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn updates_never_logs_waiting() {
    test_support::install();
    let logs = CapturedLogs::start();
    let before = waiting("updates");
    let status = status_of(slow_router("/updates", 6), "/updates", "").await;
    assert_eq!(status, 200);
    assert_eq!(waiting("updates"), before);
    assert!(
        !logs.text().contains("app request waiting"),
        "{}",
        logs.text()
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn fast_request_arms_no_warning() {
    test_support::install();
    let logs = CapturedLogs::start();
    let before = waiting("other");
    let status = status_of(slow_router("/fast-one", 1), "/fast-one", "").await;
    assert_eq!(status, 200);
    // Wait past the limit: a timer that was left armed would fire now.
    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    assert_eq!(waiting("other"), before);
    assert!(
        !logs.text().contains("app request waiting"),
        "{}",
        logs.text()
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn dropped_request_leaves_no_timer() {
    test_support::install();
    let logs = CapturedLogs::start();
    let before = waiting("pair");
    let (mut client, server) = tokio::io::duplex(64 * 1024);
    let serving = tokio::spawn(async move {
        let _ = axum::serve(OneConnection(Some(server)), slow_router("/pair", 60)).await;
    });
    let request = "GET /pair HTTP/1.1\r\nhost: localhost\r\ncontent-length: 0\r\n\r\n";
    client.write_all(request.as_bytes()).await.unwrap();
    // The client leaves after 2 seconds. The server drops the request before the timer is due.
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    drop(client);
    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    assert_eq!(waiting("pair"), before);
    assert!(
        !logs.text().contains("app request waiting"),
        "{}",
        logs.text()
    );
    serving.abort();
}
