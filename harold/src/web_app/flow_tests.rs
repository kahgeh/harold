use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::pages::CONTENT_SECURITY_POLICY;
use super::replies::REPLY_MAX_CHARS;
use super::{AppState, router};
use crate::outbound::fake::CapturedLogs;
use crate::store::{AppPushSubscribed, AppReplyState, EventVersion, HaroldStore, append_app_reply};

const LOGIN: &str = "chief@example.com";
const CANARY: &str = "APP-CANARY-7731";
/// How long a test waits for one read of a stream before it fails.
const READ_LIMIT: Duration = Duration::from_secs(5);
const MARKUP: &str = "<script>alert(1)</script> {{x}} $reply @post('/x')";
/// `MARKUP` as askama escapes it: `<` and `>` become `&#60;` and `&#62;`.
const ESCAPED: &str = "&#60;script&#62;alert(1)&#60;/script&#62;";

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("harold-web-flow-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("create test directory");
        Self(path)
    }

    async fn store(&self) -> Arc<HaroldStore> {
        Arc::new(HaroldStore::open(self.0.join("store")).await.unwrap())
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The router on a free local port; it stops when this is dropped.
struct TestServer {
    addr: SocketAddr,
    shutdown: Option<watch::Sender<()>>,
    handle: JoinHandle<()>,
}

impl TestServer {
    async fn serve(store: Arc<HaroldStore>) -> Self {
        let (shutdown, shutdown_rx) = watch::channel(());
        let state = AppState {
            store,
            allowed_login: Arc::from(LOGIN),
            public_key: Arc::from("BPublicKeyForTheTestPage"),
            shutdown: shutdown_rx,
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, router(state)).await;
        });
        Self {
            addr,
            shutdown: Some(shutdown),
            handle,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    async fn post(&self, path: &str, body: impl Into<reqwest::Body>) -> reqwest::Response {
        reqwest::Client::new()
            .post(self.url(path))
            .header("Tailscale-User-Login", LOGIN)
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap()
    }

    async fn get(&self, path: &str) -> reqwest::Response {
        reqwest::Client::new()
            .get(self.url(path))
            .header("Tailscale-User-Login", LOGIN)
            .send()
            .await
            .unwrap()
    }

    /// Opens `GET /updates`.
    async fn updates(&self) -> EventReader {
        let response = self.get("/updates").await;
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["content-type"], "text/event-stream");
        assert_eq!(
            response.headers()["content-security-policy"],
            CONTENT_SECURITY_POLICY
        );
        EventReader {
            response,
            buffer: String::new(),
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Reads one SSE response event by event, each read under `READ_LIMIT`.
struct EventReader {
    response: reqwest::Response,
    buffer: String,
}

impl EventReader {
    /// The next event, its lines without the blank line that ends it; None when the
    /// response has ended. Keep-alive comments are skipped.
    async fn next(&mut self) -> Option<String> {
        loop {
            if let Some(end) = self.buffer.find("\n\n") {
                let event = self.buffer[..end].to_owned();
                self.buffer.drain(..end + 2);
                if event.lines().all(|line| line.starts_with(':')) {
                    continue;
                }
                return Some(event);
            }
            let chunk = tokio::time::timeout(READ_LIMIT, self.response.chunk())
                .await
                .expect("an event within the read limit")
                .expect("the stream reads");
            let bytes = chunk?;
            self.buffer.push_str(std::str::from_utf8(&bytes).unwrap());
        }
    }

    /// Fails when an event arrives within `wait`.
    async fn expect_quiet(&mut self, wait: Duration) {
        if let Ok(event) = tokio::time::timeout(wait, self.next()).await {
            panic!("no further event was expected, got {event:?}");
        }
    }
}

/// Every event in the store's log, as (type, payload).
async fn events(store: &HaroldStore) -> Vec<(String, serde_json::Value)> {
    store
        .stream()
        .load_after_version(EventVersion::start(), 100)
        .await
        .unwrap()
        .into_iter()
        .map(|event| (event.r#type.as_str().to_owned(), event.payload))
        .collect()
}

fn reply_body(text: &str) -> String {
    serde_json::json!({ "reply": text }).to_string()
}

fn subscription_body(endpoint: &str, p256dh: &str, auth: &str) -> String {
    serde_json::json!({ "endpoint": endpoint, "keys": { "p256dh": p256dh, "auth": auth } })
        .to_string()
}

fn now_ms() -> i64 {
    super::route::now_ms()
}

/// Current-thread runtime: the server's handlers run on this thread, so `CapturedLogs`
/// sees their lines.
#[tokio::test(flavor = "current_thread")]
async fn a_reply_is_stored_as_to_chief_and_logged_by_length_only() {
    let logs = CapturedLogs::start();
    let directory = TestDirectory::new();
    let store = directory.store().await;
    let server = TestServer::serve(store.clone()).await;

    let response = server
        .post("/replies", format!(r#"{{"reply":"  {CANARY}  "}}"#))
        .await;

    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    assert_eq!(
        response.headers()["content-security-policy"],
        CONTENT_SECURITY_POLICY
    );
    let body = response.text().await.unwrap();
    assert!(body.contains("event: datastar-patch-signals"), "{body}");
    assert!(body.contains(r#"data: signals {"reply":""}"#), "{body}");

    let stored = events(&store).await;
    assert_eq!(stored.len(), 1, "{stored:?}");
    assert_eq!(stored[0].0, "InboundMessageReceived");
    assert_eq!(
        stored[0].1,
        serde_json::json!({ "text": CANARY, "forwarded": false, "to_chief": true })
    );
    let captured = logs.text();
    assert!(captured.contains("app reply stored"), "{captured}");
    assert!(captured.contains("chars=15"), "{captured}");
    assert!(!captured.contains(CANARY), "{captured}");
}

#[tokio::test(flavor = "multi_thread")]
async fn blank_or_over_long_replies_are_refused_and_store_nothing() {
    let directory = TestDirectory::new();
    let store = directory.store().await;
    let server = TestServer::serve(store.clone()).await;
    let over_long = "é".repeat(REPLY_MAX_CHARS + 1);
    let refused = [
        reply_body(""),
        reply_body("   "),
        reply_body(&over_long),
        r#"{"reply": "#.to_owned(),
        r#"{"other":"text"}"#.to_owned(),
    ];

    for body in refused {
        let response = server.post("/replies", body.clone()).await;
        assert_eq!(response.status(), 400, "{body:.40}");
        assert!(response.bytes().await.unwrap().is_empty(), "{body:.40}");
    }
    assert!(events(&store).await.is_empty(), "a refusal stores no event");

    let longest = "é".repeat(REPLY_MAX_CHARS);
    let response = server.post("/replies", reply_body(&longest)).await;
    assert_eq!(response.status(), 200);
    let stored = events(&store).await;
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].1["text"], longest.as_str());
    assert_eq!(REPLY_MAX_CHARS, 1000);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_subscription_is_stored_from_the_pages_post() {
    let directory = TestDirectory::new();
    let store = directory.store().await;
    let server = TestServer::serve(store.clone()).await;
    let endpoint = "https://web.push.apple.com/QGuQyavXutnMH";
    let p256dh =
        "BNcRdreALRFXTkOOUHK1EtK2wtaz5Ry4YfYCA_0QTpQtUbVlUls0VJXg7A8u-Ts1XbjhazAkj7I99e8QcYP7DkM";
    let auth = "tBHItJI5svbpez7KI4CCXg==";

    let too_big = serde_json::json!({
        "endpoint": endpoint,
        "keys": { "p256dh": p256dh, "auth": auth },
        "padding": "a".repeat(4 * 1024),
    })
    .to_string();
    let refused = [
        subscription_body("http://web.push.apple.com/x", p256dh, auth),
        subscription_body(endpoint, "", auth),
        subscription_body(endpoint, p256dh, ""),
        subscription_body(endpoint, "not base64url!", auth),
        subscription_body(endpoint, &"A".repeat(257), auth),
        subscription_body(
            &format!("https://{}", "a".repeat(2048 - "https://".len() + 1)),
            p256dh,
            auth,
        ),
        r#"{"endpoint":"https://web.push.apple.com/x"}"#.to_owned(),
        too_big,
    ];
    for body in refused {
        let response = server.post("/push/subscription", body.clone()).await;
        assert_eq!(response.status(), 400, "{body:.60}");
    }
    assert!(events(&store).await.is_empty(), "a refusal stores no event");

    let response = server
        .post(
            "/push/subscription",
            subscription_body(endpoint, p256dh, auth),
        )
        .await;

    assert_eq!(response.status(), 204);
    assert_eq!(
        response.headers()["content-security-policy"],
        CONTENT_SECURITY_POLICY
    );
    store.project_unhandled_events(100).await.unwrap();
    assert_eq!(
        store.app_push_subscriptions().await.unwrap(),
        [AppPushSubscribed {
            endpoint: endpoint.into(),
            p256dh: p256dh.into(),
            auth: auth.into(),
        }]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_stream_starts_with_the_whole_thread() {
    let directory = TestDirectory::new();
    let store = directory.store().await;
    let now = now_ms();
    store
        .add_chief_app_message("e1", now - 1000, "first")
        .await
        .unwrap();
    store
        .add_chief_app_message("e2", now, "second")
        .await
        .unwrap();
    let server = TestServer::serve(store).await;

    let mut updates = server.updates().await;
    let first = updates.next().await.expect("the first event");

    assert!(
        first.starts_with("event: datastar-patch-elements\n"),
        "{first}"
    );
    assert!(first.contains(r#"id="messages""#), "{first}");
    assert!(first.contains(r#"data-init="@get('/updates')""#), "{first}");
    assert!(
        !first.contains("data: mode"),
        "the default outer mode: {first}"
    );
    let one = first.find(r#"id="m-e1""#).expect("the first row");
    let two = first.find(r#"id="m-e2""#).expect("the second row");
    assert!(one < two, "oldest first");
    updates.expect_quiet(Duration::from_millis(300)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_stored_while_the_stream_is_open_arrives_as_one_patch() {
    let directory = TestDirectory::new();
    let store = directory.store().await;
    append_app_reply(&store, "a reply").await.unwrap();
    store.project_unhandled_events(100).await.unwrap();
    let reply = store.app_messages_since(0).await.unwrap().pop().unwrap();
    let server = TestServer::serve(store.clone()).await;
    let mut updates = server.updates().await;
    updates.next().await.expect("the whole thread");

    store
        .add_chief_app_message("e9", now_ms(), "hello")
        .await
        .unwrap();

    let added = updates.next().await.expect("the new row");
    assert!(
        added.starts_with("event: datastar-patch-elements\n"),
        "{added}"
    );
    assert!(added.contains("data: selector #messages"), "{added}");
    assert!(added.contains("data: mode append"), "{added}");
    assert!(added.contains(r#"id="m-e9""#), "{added}");
    assert!(added.contains("hello"), "{added}");
    assert!(!added.contains("a reply"), "only the new row: {added}");

    store
        .set_app_reply_state(&reply.event_id, AppReplyState::WaitingChiefBusy)
        .await
        .unwrap();

    let changed = updates.next().await.expect("the changed row");
    assert!(
        changed.starts_with("event: datastar-patch-elements\n"),
        "{changed}"
    );
    assert!(!changed.contains("data: mode"), "morphed by id: {changed}");
    assert!(
        !changed.contains("data: selector"),
        "morphed by id: {changed}"
    );
    assert!(
        changed.contains(&format!(r#"id="m-{}""#, reply.event_id)),
        "{changed}"
    );
    assert!(changed.contains("waiting: chief busy"), "{changed}");
    assert!(
        !changed.contains("hello"),
        "only the changed row: {changed}"
    );
    updates.expect_quiet(Duration::from_millis(300)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn markup_in_a_reply_is_escaped_in_the_page_and_the_stream() {
    let directory = TestDirectory::new();
    let store = directory.store().await;
    store
        .add_chief_app_message("e1", now_ms(), MARKUP)
        .await
        .unwrap();
    let server = TestServer::serve(store.clone()).await;

    let page = server.get("/").await.text().await.unwrap();
    assert!(page.contains(ESCAPED), "{page}");
    assert!(!page.contains("<script>alert"), "{page}");
    assert!(!page.contains("@post('/x')"), "{page}");
    // The page's own Datastar expressions are fixed template text.
    assert!(
        page.contains(r#"<form id="reply" class="bar bottom" data-on:submit="@post('/replies')">"#),
        "{page}"
    );
    assert!(page.contains("data-bind:reply"), "{page}");
    assert!(page.contains(r#"maxlength="1000""#), "{page}");
    assert!(
        page.contains(r#"placeholder="Reply to the chief""#),
        "{page}"
    );
    assert!(
        page.contains(r#"<section id="messages" data-init="@get('/updates')">"#),
        "{page}"
    );

    let mut updates = server.updates().await;
    let first = updates.next().await.expect("the whole thread");
    assert!(first.contains(ESCAPED), "{first}");
    assert!(!first.contains("<script>alert"), "{first}");
    assert!(!first.contains("@post('/x')"), "{first}");

    store
        .add_chief_app_message("e2", now_ms(), MARKUP)
        .await
        .unwrap();
    let added = updates.next().await.expect("the new row");
    assert!(added.contains(ESCAPED), "{added}");
    assert!(!added.contains("<script>alert"), "{added}");
    assert!(!added.contains("@post('/x')"), "{added}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_stream_ends_at_shutdown() {
    let directory = TestDirectory::new();
    let store = directory.store().await;
    let mut server = TestServer::serve(store).await;
    let mut updates = server.updates().await;
    updates.next().await.expect("the whole thread");

    drop(server.shutdown.take());

    let ended = tokio::time::timeout(Duration::from_secs(2), updates.next())
        .await
        .expect("the stream ends within 2 s");
    assert_eq!(ended, None);
}
