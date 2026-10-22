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
use super::pages::{Part, older_rows, split_links, thread_window};
use super::push::vapid_key_path;
use super::{AppState, DATASTAR_SHA256, THREAD_DAYS, WebAppIdentity, router, start, start_if_set};
use crate::outbound::fake::CapturedLogs;
use crate::settings::WebAppSettings;
use crate::store::{
    AppMessage, AppReplyState, AppSender, EventVersion, HaroldStore, append_app_reply,
};

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
        ("GET", "/thread/older"),
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
        body.contains(r#"</svg> me · waiting: chief busy</p>"#),
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
        super::idle_agents(&super::idle_grpc()),
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
        super::idle_agents(&super::idle_grpc()),
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
        super::idle_agents(&super::idle_grpc()),
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
        super::idle_agents(&super::idle_grpc()),
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
        super::idle_agents(&super::idle_grpc()),
    )
    .await
    .err()
    .expect("start refuses");
    assert_eq!(error, "web_app.icon cannot be read: /no/such/icon.png");
}

/// The page's `.who` line under each of `texts`, stored as the chief's rows.
async fn thread_body(store: Arc<HaroldStore>) -> String {
    let server = TestServer::serve(store).await;
    let response = get_with_login(&server.url("/")).await;
    assert_eq!(response.status(), 200);
    response.text().await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn each_row_ends_with_its_sender_label_and_a_reply_with_its_mark() {
    let directory = TestDirectory::new();
    let store = directory.store().await;
    store
        .add_chief_app_message("c1", now_ms() - 2000, "from the chief")
        .await
        .unwrap();
    for text in ["plain", "typed", "busy", "nochief", "paused"] {
        append_app_reply(&store, text).await.unwrap();
    }
    store.project_unhandled_events(100).await.unwrap();
    let rows = store.app_messages_since(0).await.unwrap();
    let id = |text: &str| {
        rows.iter()
            .find(|row| row.text == text)
            .unwrap()
            .event_id
            .clone()
    };
    for (text, state) in [
        ("typed", AppReplyState::Typed),
        ("busy", AppReplyState::WaitingChiefBusy),
        ("nochief", AppReplyState::NotSentNoChief),
        ("paused", AppReplyState::NotSentPaused),
    ] {
        store.set_app_reply_state(&id(text), state).await.unwrap();
    }

    let body = thread_body(store).await;

    let row = |text: &str| {
        let start = body.find(&format!(">{text}</p>")).expect(text);
        let end = body[start..].find("</div>").unwrap();
        body[start..start + end].to_owned()
    };
    assert!(row("from the chief").ends_with(r#"<p class="who">chief</p>"#));
    let stored = r#"aria-label="stored by Harold""#;
    let typed = r#"aria-label="typed into the chief""#;
    assert!(row("plain").contains(stored) && row("plain").ends_with("</svg> me</p>"));
    assert!(row("typed").contains(typed) && row("typed").ends_with("</svg> me</p>"));
    assert!(row("busy").contains(typed) && row("busy").ends_with(" me · waiting: chief busy</p>"));
    assert!(
        row("nochief").contains(stored) && row("nochief").ends_with(" me · not sent: no chief</p>")
    );
    assert!(
        row("paused").contains(stored)
            && row("paused").ends_with(" me · not sent: messaging paused</p>")
    );
}

#[test]
fn links_keep_trailing_punctuation_outside() {
    let link = |text: &str| Part {
        text: text.into(),
        link: true,
    };
    let plain = |text: &str| Part {
        text: text.into(),
        link: false,
    };
    assert_eq!(
        split_links("see https://example.com/x. ok"),
        vec![plain("see "), link("https://example.com/x"), plain(". ok")]
    );
    assert_eq!(
        split_links("(http://a.b/c)"),
        vec![plain("("), link("http://a.b/c"), plain(")")]
    );
    assert_eq!(
        split_links("\"http://a.b\""),
        vec![plain("\""), link("http://a.b"), plain("\"")]
    );
    assert_eq!(split_links("http://"), vec![plain("http://")]);
    assert_eq!(split_links("http://?"), vec![plain("http://?")]);
    assert_eq!(
        split_links("naïve ✓ https://é.example/ü"),
        vec![plain("naïve ✓ "), link("https://é.example/ü")]
    );
    assert_eq!(
        split_links("a https://x.y\nhttp://z.w"),
        vec![
            plain("a "),
            link("https://x.y"),
            plain("\n"),
            link("http://z.w")
        ]
    );
    assert_eq!(split_links(""), Vec::<Part>::new());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_link_renders_as_a_safe_anchor_and_markup_stays_text() {
    let directory = TestDirectory::new();
    let store = directory.store().await;
    store
        .add_chief_app_message("l1", now_ms() - 3000, "read https://example.com/x. now")
        .await
        .unwrap();
    store
        .add_chief_app_message(
            "l2",
            now_ms() - 2000,
            "<script>alert(1)</script> javascript:alert(1)",
        )
        .await
        .unwrap();
    store
        .add_chief_app_message("l3", now_ms() - 1000, "https://e.com/\"onmouseover=\"x")
        .await
        .unwrap();

    let body = thread_body(store).await;

    assert!(body.contains(
        r#"read <a href="https://example.com/x" target="_blank" rel="noopener noreferrer">https://example.com/x</a>. now"#
    ), "{body}");
    assert!(!body.contains("<script>alert"), "{body}");
    assert!(body.contains("&#60;script&#62;alert(1)"), "{body}");
    assert!(!body.contains(r#"href="javascript"#), "{body}");
    assert!(
        !body.contains(r#"" onmouseover"#) && !body.contains(r#"onmouseover=""#),
        "{body}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_thread_page_holds_both_pages_both_headers_and_the_page_dots() {
    let directory = TestDirectory::new();
    let identity = WebAppIdentity {
        name: "Chief M4".into(),
        icon: None,
    };
    let server = TestServer::serve_as(directory.store().await, identity).await;

    let page = get_with_login(&server.url("/")).await.text().await.unwrap();

    assert!(page.contains("<h1>Chief M4</h1>"), "{page}");
    assert!(
        page.contains("</svg>Chief M4</a>"),
        "the back button: {page}"
    );
    for wanted in [
        r##"href="#todos""##,
        r##"href="#thread""##,
        r#"id="todo-count""#,
        r#"id="new-count""#,
        r#"id="pages""#,
        r#"<main id="thread""#,
        r#"<section id="todos""#,
        r#"id="page-dots""#,
        r#"id="new-message""#,
        r#"<script src="/static/pages.js" defer></script>"#,
        "<h2>Todos</h2>",
    ] {
        assert!(page.contains(wanted), "{wanted}: {page}");
    }
    let pages = page.find(r#"id="pages""#).unwrap();
    let dots = page.find(r#"id="page-dots""#).unwrap();
    let reply = page.find(r#"id="reply""#).unwrap();
    assert!(pages < dots && dots < reply, "dots sit above the reply box");
}

#[tokio::test(flavor = "multi_thread")]
async fn pages_js_is_served_as_javascript() {
    let directory = TestDirectory::new();
    let server = TestServer::serve(directory.store().await).await;

    let response = get_with_login(&server.url("/static/pages.js")).await;

    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "text/javascript");
    assert!(response.text().await.unwrap().contains("on-todos"));
}

/// `count` chief rows `r001`, `r002`, ... in that order, one second apart, ending now.
async fn add_rows(store: &HaroldStore, count: usize) {
    let end = now_ms();
    for n in 1..=count {
        let at = end - ((count - n) as i64) * 1000;
        store
            .add_chief_app_message(&format!("r{n:03}"), at, &format!("row {n:03}"))
            .await
            .unwrap();
    }
}

fn row(n: usize, at_ms: i64) -> AppMessage {
    AppMessage {
        event_id: format!("r{n:03}"),
        at_ms,
        sender: AppSender::Chief,
        text: format!("row {n:03}"),
        state: None,
    }
}

fn rows(count: usize) -> Vec<AppMessage> {
    (1..=count).map(|n| row(n, n as i64)).collect()
}

fn ids(rows: &[AppMessage]) -> Vec<&str> {
    rows.iter().map(|row| row.event_id.as_str()).collect()
}

#[test]
fn thread_window_starts_at_a_found_from_and_says_when_older_rows_remain() {
    let all = rows(120);

    let window = thread_window(&all, Some("r021"));
    assert_eq!(window.rows.len(), 100);
    assert_eq!(window.rows[0].event_id, "r021");
    assert_eq!(window.rows.last().unwrap().event_id, "r120");
    assert!(window.more);

    let window = thread_window(&all, Some("r001"));
    assert_eq!(window.rows.len(), 120);
    assert!(!window.more, "nothing comes before the first row");
}

#[test]
fn thread_window_gives_the_newest_50_for_a_none_unknown_or_pruned_from() {
    let all = rows(51);
    for from in [None, Some("nope"), Some(""), Some("r000")] {
        let window = thread_window(&all, from);
        assert_eq!(window.rows.len(), 50, "{from:?}");
        assert_eq!(window.rows[0].event_id, "r002", "{from:?}");
        assert!(window.more, "51 rows leave one older: {from:?}");
    }
    let fifty = rows(50);
    let window = thread_window(&fifty, None);
    assert_eq!(window.rows.len(), 50);
    assert!(!window.more, "50 rows leave none older");
    let window = thread_window(&[], None);
    assert!(window.rows.is_empty() && !window.more);
}

#[test]
fn rows_with_the_same_time_keep_the_order_they_were_added() {
    let all: Vec<AppMessage> = (1..=60).map(|n| row(n, 7)).collect();

    let window = thread_window(&all, None);
    assert_eq!(ids(window.rows)[0], "r011");
    assert_eq!(ids(window.rows)[49], "r060");

    let older = older_rows(&all, "r011");
    assert_eq!(ids(older.rows), ids(&all)[..10]);
    assert!(!older.more);
}

#[test]
fn older_rows_gives_up_to_50_rows_before_and_says_when_more_come_before_them() {
    let all = rows(120);

    let older = older_rows(&all, "r071");
    assert_eq!(older.rows.len(), 50);
    assert_eq!(older.rows[0].event_id, "r021");
    assert_eq!(older.rows[49].event_id, "r070");
    assert!(older.more);

    let older = older_rows(&all, "r051");
    assert_eq!(older.rows.len(), 50);
    assert_eq!(older.rows[0].event_id, "r001");
    assert!(!older.more, "exactly 50 before leaves none");

    let older = older_rows(&all, "r052");
    assert_eq!(older.rows.len(), 50);
    assert!(older.more, "51 before leaves one");

    let older = older_rows(&all, "r011");
    assert_eq!(older.rows.len(), 10);
    assert!(!older.more);

    let older = older_rows(&all, "r001");
    assert!(older.rows.is_empty() && !older.more);
}

#[test]
fn older_rows_gives_none_for_an_unknown_before() {
    let all = rows(120);
    let older = older_rows(&all, "gone");
    assert!(older.rows.is_empty() && !older.more);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_thread_page_shows_the_newest_50_and_an_older_line_naming_the_oldest_shown() {
    let directory = TestDirectory::new();
    let store = directory.store().await;
    add_rows(&store, 120).await;

    let body = thread_body(store).await;

    assert_eq!(
        body.matches(r#"class="message chief""#).count(),
        50,
        "{body}"
    );
    assert!(body.contains(r#"id="m-r071""#) && body.contains(r#"id="m-r120""#));
    assert!(!body.contains(r#"id="m-r070""#));
    assert!(
        body.contains(
            r#"<p id="older" class="older" data-on-intersect="@get('/thread/older')">Loading earlier messages…</p>"#
        ),
        "{body}"
    );
    let messages = body.find(r#"<section id="messages""#).unwrap();
    let older = body.find(r#"id="older""#).unwrap();
    let first = body.find(r#"id="m-r071""#).unwrap();
    assert!(messages < older && older < first, "the first child");
    assert!(
        body.contains(r#"data-signals:oldest="&#39;r071&#39;""#),
        "the page holds the oldest signal: {body}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_thread_page_has_no_older_line_with_50_rows() {
    let directory = TestDirectory::new();
    let store = directory.store().await;
    add_rows(&store, 50).await;

    let body = thread_body(store).await;

    assert_eq!(body.matches(r#"class="message chief""#).count(), 50);
    assert!(!body.contains(r#"id="older""#), "{body}");
    assert!(!body.contains("Loading earlier messages"), "{body}");
}

#[tokio::test(flavor = "current_thread")]
async fn the_thread_page_logs_rows_bytes_and_ms_and_no_message_text() {
    let directory = TestDirectory::new();
    let store = directory.store().await;
    add_rows(&store, 60).await;
    let server = TestServer::serve(store).await;
    let logs = CapturedLogs::start();

    let body = get_with_login(&server.url("/")).await.text().await.unwrap();

    let text = logs.text();
    let line = text
        .lines()
        .find(|line| line.contains("app page served"))
        .unwrap_or_else(|| panic!("no served line: {text}"));
    assert!(line.contains("INFO"), "{line}");
    assert!(line.contains("rows=50"), "{line}");
    assert!(line.contains(&format!("bytes={}", body.len())), "{line}");
    assert!(line.contains("ms="), "{line}");
    assert!(!text.contains("row 060"), "{text}");
}

/// The same text on every page and patch, so Datastar never sets up a new watcher.
const OLDER_ATTRIBUTE: &str = r#"data-on-intersect="@get('/thread/older')""#;

/// `GET /thread/older` with the page's signals, as Datastar sends them.
async fn get_older(server: &TestServer, signals: Option<&str>) -> reqwest::Response {
    let mut request = reqwest::Client::new()
        .get(server.url("/thread/older"))
        .header("Tailscale-User-Login", LOGIN);
    if let Some(signals) = signals {
        request = request.query(&[("datastar", signals)]);
    }
    request.send().await.unwrap()
}

/// The events of an SSE response that closes, each as its lines.
async fn all_events(response: reqwest::Response) -> Vec<String> {
    response
        .text()
        .await
        .unwrap()
        .split("\n\n")
        .filter(|event| !event.trim().is_empty())
        .map(str::to_owned)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn thread_older_gives_the_rows_before_then_oldest_and_leaves_the_older_line() {
    let directory = TestDirectory::new();
    let store = directory.store().await;
    add_rows(&store, 120).await;
    let server = TestServer::serve(store).await;

    let response = get_older(&server, Some(r#"{"oldest":"r071"}"#)).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    assert_eq!(
        response.headers()["content-security-policy"],
        CONTENT_SECURITY_POLICY
    );
    let events = all_events(response).await;

    assert_eq!(events.len(), 2, "{events:?}");
    let rows = &events[0];
    assert!(
        rows.starts_with("event: datastar-patch-elements\n"),
        "{rows}"
    );
    assert!(rows.contains("data: selector #older"), "{rows}");
    assert!(rows.contains("data: mode after"), "{rows}");
    assert_eq!(
        rows.matches(r#"class="message chief""#).count(),
        50,
        "{rows}"
    );
    assert!(rows.contains(r#"id="m-r021""#) && rows.contains(r#"id="m-r070""#));
    assert!(rows.find(r#"id="m-r021""#) < rows.find(r#"id="m-r070""#));
    let oldest = &events[1];
    assert!(
        oldest.starts_with("event: datastar-patch-signals\n"),
        "{oldest}"
    );
    assert!(oldest.contains(r#"{"oldest":"r021"}"#), "{oldest}");
}

#[tokio::test(flavor = "multi_thread")]
async fn thread_older_removes_the_older_line_at_the_end() {
    let directory = TestDirectory::new();
    let store = directory.store().await;
    add_rows(&store, 120).await;
    let server = TestServer::serve(store).await;

    let response = get_older(&server, Some(r#"{"oldest":"r021"}"#)).await;
    let events = all_events(response).await;

    assert_eq!(events.len(), 3, "{events:?}");
    assert_eq!(events[0].matches(r#"class="message chief""#).count(), 20);
    assert!(events[0].contains(r#"id="m-r001""#));
    assert!(events[1].contains("data: selector #older"), "{}", events[1]);
    assert!(events[1].contains("data: mode remove"), "{}", events[1]);
    assert!(events[2].contains(r#"{"oldest":"r001"}"#), "{}", events[2]);
}

#[tokio::test(flavor = "multi_thread")]
async fn thread_older_for_a_missing_or_unknown_oldest_removes_the_line_and_adds_no_rows() {
    let directory = TestDirectory::new();
    let store = directory.store().await;
    add_rows(&store, 120).await;
    let server = TestServer::serve(store).await;

    for signals in [
        Some(r#"{"oldest":"gone"}"#),
        Some(r#"{"oldest":""}"#),
        Some(r#"{"reply":""}"#),
        Some("not json"),
        None,
    ] {
        let response = get_older(&server, signals).await;
        assert_eq!(response.status(), 200, "{signals:?}");
        let text = response.text().await.unwrap();
        assert!(!text.contains(r#"class="message"#), "{signals:?}: {text}");
        assert!(text.contains("data: mode remove"), "{signals:?}: {text}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn thread_older_logs_rows_bytes_and_ms_and_no_message_text() {
    let directory = TestDirectory::new();
    let store = directory.store().await;
    add_rows(&store, 120).await;
    let server = TestServer::serve(store).await;
    let logs = CapturedLogs::start();

    let response = get_older(&server, Some(r#"{"oldest":"r071"}"#)).await;
    let body = response.text().await.unwrap();

    let text = logs.text();
    let line = text
        .lines()
        .find(|line| line.contains("app thread older served"))
        .unwrap_or_else(|| panic!("no older served line: {text}"));
    assert!(line.contains("INFO"), "{line}");
    assert!(line.contains("rows=50"), "{line}");
    let bytes: usize = line
        .split("bytes=")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|bytes| bytes.parse().ok())
        .unwrap_or_else(|| panic!("no bytes in {line}"));
    assert!(bytes > 0 && bytes <= body.len(), "{line}");
    assert!(line.contains("ms="), "{line}");
    assert!(!text.contains("row 0"), "{text}");
}

#[tokio::test(flavor = "current_thread")]
async fn thread_older_with_an_unreadable_store_gives_503_and_logs_a_warning() {
    let directory = TestDirectory::new();
    let store = directory.store().await;
    let state = turso::Builder::new_local(
        directory
            .store_path()
            .join("harold-state.db")
            .to_str()
            .unwrap(),
    )
    .build()
    .await
    .unwrap();
    state
        .connect()
        .unwrap()
        .execute("DROP TABLE app_messages", ())
        .await
        .unwrap();
    let server = TestServer::serve(store).await;
    let logs = CapturedLogs::start();

    let response = get_older(&server, Some(r#"{"oldest":"r071"}"#)).await;

    assert_eq!(response.status(), 503);
    let text = logs.text();
    let line = text
        .lines()
        .find(|line| line.contains("app thread older not read"))
        .unwrap_or_else(|| panic!("no not-read line: {text}"));
    assert!(line.contains("WARN"), "{line}");
    assert!(line.contains(r#"reason="store_unread""#), "{line}");
}

/// The first event of `GET /updates` with the page's signals, and how many bytes it took.
async fn first_patch(server: &TestServer, signals: Option<&str>) -> String {
    let mut request = reqwest::Client::new()
        .get(server.url("/updates"))
        .header("Tailscale-User-Login", LOGIN);
    if let Some(signals) = signals {
        request = request.query(&[("datastar", signals)]);
    }
    let mut response = request.send().await.unwrap();
    assert_eq!(response.status(), 200);
    let mut buffer = String::new();
    loop {
        if let Some(end) = buffer.find("\n\n") {
            buffer.truncate(end);
            return buffer;
        }
        let chunk = tokio::time::timeout(Duration::from_secs(5), response.chunk())
            .await
            .expect("an event within five seconds")
            .unwrap()
            .expect("the stream stays open");
        buffer.push_str(std::str::from_utf8(&chunk).unwrap());
    }
}

fn row_count(patch: &str) -> usize {
    patch.matches(r#"class="message chief""#).count()
}

#[tokio::test(flavor = "multi_thread")]
async fn updates_redraws_from_the_oldest_signal_with_the_older_line_and_data_init() {
    let directory = TestDirectory::new();
    let store = directory.store().await;
    add_rows(&store, 120).await;
    let server = TestServer::serve(store).await;

    let first = first_patch(&server, Some(r#"{"oldest":"r020"}"#)).await;

    assert_eq!(row_count(&first), 101, "rows 20 to 120");
    assert!(first.contains(r#"id="m-r020""#) && !first.contains(r#"id="m-r019""#));
    assert!(first.contains(r#"id="m-r120""#));
    assert!(first.contains(OLDER_ATTRIBUTE), "{first}");
    assert!(first.contains(r#"data-init="@get('/updates')""#), "{first}");

    let first = first_patch(&server, Some(r#"{"oldest":"r001"}"#)).await;

    assert_eq!(row_count(&first), 120, "every row");
    assert!(!first.contains(r#"id="older""#), "{first}");
    assert!(first.contains(r#"data-init="@get('/updates')""#), "{first}");
}

#[tokio::test(flavor = "multi_thread")]
async fn updates_draws_the_newest_50_for_a_missing_unknown_pruned_or_malformed_signal() {
    let directory = TestDirectory::new();
    let store = directory.store().await;
    add_rows(&store, 120).await;
    let server = TestServer::serve(store).await;

    for signals in [
        None,
        Some(r#"{"reply":""}"#),
        Some(r#"{"oldest":"unknown"}"#),
        Some(r#"{"oldest":"r000"}"#),
        Some(r#"{"oldest":""}"#),
        Some(r#"{"oldest":7}"#),
        Some("not json"),
    ] {
        let first = first_patch(&server, signals).await;
        assert_eq!(row_count(&first), 50, "{signals:?}");
        assert!(first.contains(r#"id="m-r071""#), "{signals:?}");
        assert!(!first.contains(r#"id="m-r070""#), "{signals:?}");
        assert!(first.contains(OLDER_ATTRIBUTE), "{signals:?}");
        assert!(first.contains(r#"data-init="@get('/updates')""#));
    }
}

#[tokio::test(flavor = "current_thread")]
async fn updates_logs_the_rows_and_bytes_of_its_first_patch() {
    let directory = TestDirectory::new();
    let store = directory.store().await;
    add_rows(&store, 120).await;
    let server = TestServer::serve(store).await;
    let logs = CapturedLogs::start();

    let first = first_patch(&server, None).await;

    let text = logs.text();
    let line = text
        .lines()
        .find(|line| line.contains("app page connected"))
        .unwrap_or_else(|| panic!("no connected line: {text}"));
    assert!(line.contains("rows=50"), "{line}");
    assert!(line.contains("bytes="), "{line}");
    assert!(!text.contains("row 120"), "{text}");
    drop(first);
}

#[tokio::test(flavor = "multi_thread")]
async fn updates_sets_the_oldest_signal_when_the_page_held_another() {
    let directory = TestDirectory::new();
    let store = directory.store().await;
    add_rows(&store, 120).await;
    let server = TestServer::serve(store).await;

    let mut response = reqwest::Client::new()
        .get(server.url("/updates"))
        .query(&[("datastar", r#"{"oldest":"pruned"}"#)])
        .header("Tailscale-User-Login", LOGIN)
        .send()
        .await
        .unwrap();
    let mut buffer = String::new();
    while !buffer.contains(r#"{"oldest":"r071"}"#) {
        let chunk = tokio::time::timeout(Duration::from_secs(5), response.chunk())
            .await
            .expect("the oldest signal within five seconds")
            .unwrap()
            .expect("the stream stays open");
        buffer.push_str(std::str::from_utf8(&chunk).unwrap());
    }
    assert!(buffer.contains("event: datastar-patch-signals"), "{buffer}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_change_to_a_row_older_than_the_window_sends_no_patch() {
    let directory = TestDirectory::new();
    let store = directory.store().await;
    append_app_reply(&store, "an old reply").await.unwrap();
    store.project_unhandled_events(100).await.unwrap();
    let reply = store.app_messages_since(0).await.unwrap().pop().unwrap();
    // 60 newer rows push the reply out of the newest 50.
    let end = now_ms() + 1000;
    for n in 1..=60 {
        store
            .add_chief_app_message(&format!("n{n:03}"), end + n, "newer")
            .await
            .unwrap();
    }
    let server = TestServer::serve(store.clone()).await;
    let mut response = reqwest::Client::new()
        .get(server.url("/updates"))
        .header("Tailscale-User-Login", LOGIN)
        .send()
        .await
        .unwrap();
    // The first patch and the dots/todos state; read until the stream goes quiet.
    let mut seen = String::new();
    while let Ok(Ok(Some(chunk))) =
        tokio::time::timeout(Duration::from_millis(500), response.chunk()).await
    {
        seen.push_str(std::str::from_utf8(&chunk).unwrap());
    }
    assert!(!seen.contains(&format!("m-{}", reply.event_id)), "{seen}");

    store
        .set_app_reply_state(&reply.event_id, AppReplyState::WaitingChiefBusy)
        .await
        .unwrap();

    let after = tokio::time::timeout(Duration::from_millis(500), response.chunk()).await;
    if let Ok(Ok(Some(chunk))) = after {
        let text = String::from_utf8_lossy(&chunk).into_owned();
        assert!(
            text.lines()
                .all(|line| line.starts_with(':') || line.is_empty()),
            "a patch for a row older than the window: {text}"
        );
    }
}
