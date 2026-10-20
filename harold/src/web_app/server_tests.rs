use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::origin::loopback_hosts;
use super::pages::CONTENT_SECURITY_POLICY;
use super::push::vapid_key_path;
use super::{AppState, DATASTAR_SHA256, THREAD_DAYS, WebAppIdentity, router, start, start_if_set};
use crate::outbound::fake::CapturedLogs;
use crate::settings::WebAppSettings;
use crate::store::{AppReplyState, EventVersion, HaroldStore, append_app_reply};

const LOGIN: &str = "chief@example.com";
const PUBLIC_KEY: &str = "BPublicKeyForTheTestPage";
const CANARY: &str = "APP-CANARY-7731";
const DAY_MS: i64 = 24 * 60 * 60 * 1000;

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("harold-web-app-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("create test directory");
        Self(path)
    }

    /// The store's folder. The VAPID key goes beside it, in this directory.
    fn store_path(&self) -> PathBuf {
        self.0.join("store")
    }

    async fn store(&self) -> Arc<HaroldStore> {
        Arc::new(HaroldStore::open(self.store_path()).await.unwrap())
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn now_ms() -> i64 {
    super::route::now_ms()
}

/// The router on a free local port; it stops when this is dropped.
struct TestServer {
    addr: SocketAddr,
    _shutdown: watch::Sender<()>,
    handle: JoinHandle<()>,
}

impl TestServer {
    async fn serve(store: Arc<HaroldStore>) -> Self {
        Self::serve_as(store, WebAppIdentity::default()).await
    }

    async fn serve_as(store: Arc<HaroldStore>, identity: WebAppIdentity) -> Self {
        let (shutdown, shutdown_rx) = watch::channel(());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = AppState::for_tests(
            store,
            LOGIN,
            PUBLIC_KEY,
            Arc::from(loopback_hosts("127.0.0.1:0", addr)),
            shutdown_rx,
            None,
            identity,
        );
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, router(state)).await;
        });
        Self {
            addr,
            _shutdown: shutdown,
            handle,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

fn settings(listen: &str) -> WebAppSettings {
    WebAppSettings {
        listen: listen.into(),
        allowed_login: LOGIN.into(),
        app_name: None,
        icon: None,
    }
}

async fn get_with_login(url: &str) -> reqwest::Response {
    reqwest::Client::new()
        .get(url)
        .header("Tailscale-User-Login", LOGIN)
        .send()
        .await
        .unwrap()
}

/// Every event in the store's log.
async fn event_count(store: &HaroldStore) -> usize {
    store
        .stream()
        .load_after_version(EventVersion::start(), 100)
        .await
        .unwrap()
        .len()
}

/// Current-thread runtime: the server's middleware runs on this thread, so `CapturedLogs`
/// sees its lines, and the exported records catch any line from any thread.
#[tokio::test(flavor = "current_thread")]
async fn every_route_refuses_a_missing_or_wrong_login() {
    crate::telemetry::test_support::install();
    let logs = CapturedLogs::start();
    let directory = TestDirectory::new();
    let store = directory.store().await;
    let server = TestServer::serve(store.clone()).await;
    let client = reqwest::Client::new();
    let reply = format!(r#"{{"reply":"{CANARY}"}}"#);
    let routes = [
        ("GET", "/"),
        ("GET", "/updates"),
        ("POST", "/replies"),
        ("POST", "/push/subscription"),
        ("GET", "/static/app.css"),
        ("GET", "/static/icon.png"),
        ("GET", "/nope"),
    ];

    for login in [None, Some("someone@else")] {
        for (method, path) in routes {
            let mut request = match method {
                "GET" => client.get(server.url(path)),
                _ => client.post(server.url(path)).body(reply.clone()),
            };
            if let Some(login) = login {
                request = request.header("Tailscale-User-Login", login);
            }
            let response = request.send().await.unwrap();
            assert_eq!(response.status(), 403, "{method} {path} with {login:?}");
            assert!(
                response.bytes().await.unwrap().is_empty(),
                "{method} {path}"
            );
        }
    }

    let with_login = get_with_login(&server.url("/nope")).await;
    assert_eq!(with_login.status(), 404, "with the login, an unknown path");

    assert_eq!(event_count(&store).await, 0, "a refusal stores no event");
    assert!(store.app_messages_since(0).await.unwrap().is_empty());
    // Other tests export records at the same time; neither string appears anywhere else.
    let exported = format!("{:?}", crate::telemetry::test_support::logs());
    let captured = logs.text();
    for named in [CANARY, "someone@else"] {
        assert!(
            !exported.contains(named),
            "an exported record names {named}"
        );
        assert!(!captured.contains(named), "a log line names {named}");
    }
    for (_, path) in routes.iter().filter(|(_, path)| *path != "/") {
        assert!(!captured.contains(path), "a log line names {path}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_thread_page_lists_the_last_30_days_oldest_first() {
    let directory = TestDirectory::new();
    let store = directory.store().await;
    let now = now_ms();
    store
        .add_chief_app_message("old", now - 31 * DAY_MS, "thirty-one days ago")
        .await
        .unwrap();
    store
        .add_chief_app_message("two", now - 2 * DAY_MS, "two days ago")
        .await
        .unwrap();
    store
        .add_chief_app_message("one", now - DAY_MS, "one day ago")
        .await
        .unwrap();
    append_app_reply(&store, "a reply").await.unwrap();
    store.project_unhandled_events(100).await.unwrap();
    let reply = store.app_messages_since(0).await.unwrap().pop().unwrap();
    store
        .set_app_reply_state(&reply.event_id, AppReplyState::WaitingChiefBusy)
        .await
        .unwrap();
    let server = TestServer::serve(store).await;

    let response = get_with_login(&server.url("/")).await;

    assert_eq!(response.status(), 200);
    let headers = response.headers().clone();
    assert_eq!(headers["content-type"], "text/html; charset=utf-8");
    assert_eq!(headers["content-security-policy"], CONTENT_SECURITY_POLICY);
    let body = response.text().await.unwrap();
    assert!(!body.contains("thirty-one days ago"), "{body}");
    let two = body
        .find("two days ago")
        .expect("the row from two days ago");
    let one = body.find("one day ago").expect("the row from one day ago");
    let reply_at = body.find("a reply").expect("the reply");
    assert!(two < one && one < reply_at, "oldest first");
    assert!(
        body.contains(r#"id="m-two" class="message chief""#),
        "{body}"
    );
    assert!(
        body.contains(&format!(
            r#"id="m-{}" class="message user""#,
            reply.event_id
        )),
        "{body}"
    );
    assert!(
        body.contains(r#"<p class="state">waiting: chief busy</p>"#),
        "{body}"
    );
    assert!(body.contains("Turn on alerts"));
    assert!(body.contains(PUBLIC_KEY));
    for absent in ["http://", "https://", "<script>", "style="] {
        assert!(!body.contains(absent), "the page holds {absent}");
    }
    assert_eq!(THREAD_DAYS, 30);
}

#[tokio::test(flavor = "multi_thread")]
async fn static_files_are_served_with_their_types_and_the_policy() {
    let directory = TestDirectory::new();
    let server = TestServer::serve(directory.store().await).await;

    for (name, content_type) in [
        ("app.css", "text/css"),
        ("alerts.js", "text/javascript"),
        ("sw.js", "text/javascript"),
        ("datastar-1.0.2.js", "text/javascript"),
        ("manifest.webmanifest", "application/manifest+json"),
    ] {
        let response = get_with_login(&server.url(&format!("/static/{name}"))).await;
        assert_eq!(response.status(), 200, "{name}");
        let headers = response.headers().clone();
        assert_eq!(headers["content-type"], content_type, "{name}");
        assert_eq!(
            headers["content-security-policy"], CONTENT_SECURITY_POLICY,
            "{name}"
        );
        assert_eq!(
            headers.get("service-worker-allowed").is_some(),
            name == "sw.js",
            "{name}"
        );
        let bytes = response.bytes().await.unwrap();
        assert!(!bytes.is_empty(), "{name}");
        if name == "sw.js" {
            assert_eq!(headers["service-worker-allowed"], "/");
        }
        if name == "datastar-1.0.2.js" {
            assert_eq!(hex::encode(Sha256::digest(&bytes)), DATASTAR_SHA256);
        }
    }

    for path in [
        "/static/../Cargo.toml",
        "/static/other.js",
        "/static/%2e%2e%2fCargo.toml",
    ] {
        let response = get_with_login(&server.url(path)).await;
        assert_eq!(response.status(), 404, "{path}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn no_section_starts_nothing() {
    let directory = TestDirectory::new();
    let store = directory.store().await;
    let (_shutdown, shutdown_rx) = watch::channel(());

    let running = start_if_set(
        None,
        store,
        &directory.store_path(),
        shutdown_rx,
        None,
        super::idle_hub(),
        None,
        &super::idle_grpc(),
    )
    .await;

    assert!(running.is_none());
    assert!(!vapid_key_path(&directory.store_path()).exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn start_serves_until_shutdown() {
    let directory = TestDirectory::new();
    let store = directory.store().await;
    let (shutdown, shutdown_rx) = watch::channel(());

    let running = start(
        &settings("127.0.0.1:0"),
        store,
        &directory.store_path(),
        shutdown_rx,
        None,
        super::idle_hub(),
        None,
        &super::idle_grpc(),
    )
    .await
    .expect("the app starts");

    let response = get_with_login(&format!("http://{}/", running.local_addr)).await;
    assert_eq!(response.status(), 200);
    assert!(vapid_key_path(&directory.store_path()).exists());

    drop(shutdown);
    tokio::time::timeout(Duration::from_secs(2), running.handle)
        .await
        .expect("the app stops within 2 s")
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_address_in_use_runs_without_the_app() {
    let directory = TestDirectory::new();
    let store = directory.store().await;
    let taken = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = taken.local_addr().unwrap();
    let (_shutdown, shutdown_rx) = watch::channel(());

    let running = start_if_set(
        Some(&settings(&addr.to_string())),
        store,
        &directory.store_path(),
        shutdown_rx,
        None,
        super::idle_hub(),
        None,
        &super::idle_grpc(),
    )
    .await;

    assert!(running.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn old_thread_rows_are_pruned_at_start() {
    let directory = TestDirectory::new();
    let store = directory.store().await;
    let now = now_ms();
    store
        .add_chief_app_message("old", now - 31 * DAY_MS, "thirty-one days ago")
        .await
        .unwrap();
    store
        .add_chief_app_message("new", now - DAY_MS, "one day ago")
        .await
        .unwrap();
    let (shutdown, shutdown_rx) = watch::channel(());

    let running = start(
        &settings("127.0.0.1:0"),
        store.clone(),
        &directory.store_path(),
        shutdown_rx,
        None,
        super::idle_hub(),
        None,
        &super::idle_grpc(),
    )
    .await
    .expect("the app starts");

    let ids: Vec<String> = store
        .app_messages_since(0)
        .await
        .unwrap()
        .into_iter()
        .map(|message| message.event_id)
        .collect();
    assert_eq!(ids, ["new"]);
    drop(shutdown);
    let _ = tokio::time::timeout(Duration::from_secs(2), running.handle).await;
}

fn named(name: &str, icon: bool) -> WebAppIdentity {
    WebAppIdentity {
        name: name.into(),
        icon: icon.then(|| crate::settings::WebAppIcon {
            bytes: b"\x89PNG-bytes".to_vec(),
            side: 512,
        }),
    }
}

async fn body_of(server: &TestServer, path: &str) -> String {
    get_with_login(&server.url(path))
        .await
        .text()
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_icon_is_served_as_a_png_only_when_one_is_set() {
    let directory = TestDirectory::new();
    let other = TestDirectory::new();
    let with = TestServer::serve_as(directory.store().await, named("Chief M4", true)).await;
    let response = get_with_login(&with.url("/static/icon.png")).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "image/png");
    assert_eq!(response.bytes().await.unwrap().as_ref(), b"\x89PNG-bytes");

    let without = TestServer::serve(other.store().await).await;
    let response = get_with_login(&without.url("/static/icon.png")).await;
    assert_eq!(response.status(), 404);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_thread_page_carries_the_name_and_links_the_icon_only_when_set() {
    let directory = TestDirectory::new();
    let other = TestDirectory::new();
    let with = TestServer::serve_as(directory.store().await, named("Chief M4", true)).await;
    let page = body_of(&with, "/").await;
    assert!(page.contains("<title>Chief M4</title>"), "{page}");
    assert!(page.contains(r#"name="apple-mobile-web-app-title" content="Chief M4""#));
    assert!(page.contains(r#"<link rel="apple-touch-icon" href="/static/icon.png">"#));

    let without = TestServer::serve(other.store().await).await;
    let page = body_of(&without, "/").await;
    assert!(page.contains("<title>Chief</title>"));
    assert!(page.contains(r#"name="apple-mobile-web-app-title" content="Chief""#));
    assert!(!page.contains("apple-touch-icon"));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_manifest_is_built_from_the_name_and_the_icon() {
    let directory = TestDirectory::new();
    let other = TestDirectory::new();
    let with = TestServer::serve_as(directory.store().await, named("M4 \"<x>\"", true)).await;
    let manifest: serde_json::Value =
        serde_json::from_str(&body_of(&with, "/static/manifest.webmanifest").await).unwrap();
    assert_eq!(manifest["name"], "M4 \"<x>\"");
    assert_eq!(manifest["short_name"], "M4 \"<x>\"");
    assert_eq!(manifest["start_url"], "/");
    assert_eq!(manifest["icons"][0]["src"], "/static/icon.png");
    assert_eq!(manifest["icons"][0]["sizes"], "512x512");
    assert_eq!(manifest["icons"][0]["type"], "image/png");

    let without = TestServer::serve(other.store().await).await;
    let manifest: serde_json::Value =
        serde_json::from_str(&body_of(&without, "/static/manifest.webmanifest").await).unwrap();
    assert_eq!(manifest["name"], "Chief");
    assert_eq!(manifest["short_name"], "Chief");
    assert!(manifest.get("icons").is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn start_refuses_an_icon_that_cannot_be_read() {
    let directory = TestDirectory::new();
    let mut settings = settings("127.0.0.1:0");
    settings.icon = Some("/no/such/icon.png".into());
    let (_shutdown, shutdown_rx) = watch::channel(());
    let error = start(
        &settings,
        directory.store().await,
        &directory.store_path(),
        shutdown_rx,
        None,
        super::idle_hub(),
        None,
        &super::idle_grpc(),
    )
    .await
    .err()
    .expect("start refuses");
    assert_eq!(error, "web_app.icon cannot be read: /no/such/icon.png");
}
