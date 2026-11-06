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
    let (mut client, server) = tokio::io::duplex(64 * 1024);
    let serving = tokio::spawn(async move {
        let _ = axum::serve(OneConnection(Some(server)), app).await;
    });
    let request = format!(
        "GET {path} HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n{extra_headers}\r\n"
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
