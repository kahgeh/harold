//! The back-swipe script and its static route.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::collections::Collections;
use super::origin::loopback_hosts;
use super::specs::{Specs, SpecsState, listener_hosts, specs_router};
use super::{AppState, WebAppIdentity, router};
use crate::settings::CollectionSettings;
use crate::store::HaroldStore;
use crate::tools::Products;

const LOGIN: &str = "chief@example.com";

struct Served {
    addr: SocketAddr,
    handle: JoinHandle<()>,
    _shutdown: watch::Sender<()>,
}

impl Drop for Served {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

impl Served {
    async fn get(&self, path: &str) -> reqwest::Response {
        reqwest::Client::new()
            .get(format!("http://{}{path}", self.addr))
            .header("Tailscale-User-Login", LOGIN)
            .send()
            .await
            .unwrap()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn web_app_serves_back_swipe_js() {
    let directory =
        std::env::temp_dir().join(format!("harold-back-swipe-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
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
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router(state)).await;
    });
    let server = Served {
        addr,
        handle,
        _shutdown: shutdown,
    };

    let response = server.get("/static/back-swipe.js").await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "text/javascript");
    let cache = response
        .headers()
        .get("cache-control")
        .map(|value| value.to_str().unwrap().to_owned());
    assert!(
        !cache.unwrap_or_default().contains("immutable"),
        "back-swipe.js must not be immutable"
    );
    assert_eq!(
        response.text().await.unwrap(),
        include_str!("../../static/web_app/back-swipe.js")
    );
    let _ = std::fs::remove_dir_all(&directory);
}

#[tokio::test(flavor = "multi_thread")]
async fn specs_listener_answers_404_for_back_swipe_js() {
    let (shutdown, _) = watch::channel(());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = specs_router(SpecsState {
        specs: Arc::new(Specs::new(None)),
        identity: Arc::new(WebAppIdentity::default()),
        collections: Arc::new(Collections::new(Vec::new())),
        hosts: Arc::from(listener_hosts("127.0.0.1:4331", addr)),
    });
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let server = Served {
        addr,
        handle,
        _shutdown: shutdown,
    };
    assert_eq!(server.get("/static/back-swipe.js").await.status(), 404);
}

#[test]
fn back_swipe_script_keeps_its_contract() {
    let script = include_str!("../../static/web_app/back-swipe.js");
    for wanted in [
        "16",
        "80",
        "touchstart",
        "touchmove",
        "passive: false",
        "preventDefault",
        "scrollLeft",
        "[data-back]",
        "touchcancel",
    ] {
        assert!(script.contains(wanted), "back-swipe.js lacks {wanted}");
    }
    for absent in ["eval(", "new Function", "innerHTML", "http://", "https://"] {
        assert!(!script.contains(absent), "back-swipe.js holds {absent}");
    }
}

struct Fixture {
    root: std::path::PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("harold-back-pages-{}", uuid::Uuid::new_v4()));
        let specs = root.join("repo/.superpowers/pod/specs");
        std::fs::create_dir_all(&specs).unwrap();
        std::fs::write(specs.join("a.md"), "# A spec\n").unwrap();
        let pages = root.join("pages");
        std::fs::create_dir_all(&pages).unwrap();
        std::fs::write(pages.join("a.html"), "<title>Alpha</title>").unwrap();
        std::fs::write(pages.join("n.md"), "# Note\n").unwrap();
        Self { root }
    }

    fn specs(&self) -> Arc<Specs> {
        Arc::new(Specs::with_products(Arc::new(OneProduct(
            self.root.join("repo"),
        ))))
    }

    fn collections(&self) -> Arc<Collections> {
        Arc::new(Collections::new(vec![CollectionSettings {
            name: "explainers".into(),
            title: "Explainers".into(),
            folder: self.root.join("pages").to_string_lossy().into_owned(),
            extensions: vec!["html".into(), "md".into()],
            listed: true,
        }]))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

struct OneProduct(std::path::PathBuf);

impl Products for OneProduct {
    fn products(&self) -> Result<Vec<(String, std::path::PathBuf)>, &'static str> {
        Ok(vec![("harold".into(), self.0.clone())])
    }
}

async fn web_app_with(fixture: &Fixture) -> Served {
    let store = Arc::new(HaroldStore::open(fixture.root.join("store")).await.unwrap());
    let (shutdown, shutdown_rx) = watch::channel(());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut state = AppState::for_tests(
        store,
        LOGIN,
        "BPublicKey",
        Arc::from(loopback_hosts("127.0.0.1:0", addr)),
        shutdown_rx,
        None,
        WebAppIdentity::default(),
    );
    state.specs = fixture.specs();
    state.collections = fixture.collections();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router(state)).await;
    });
    Served {
        addr,
        handle,
        _shutdown: shutdown,
    }
}

async fn desk_with(fixture: &Fixture) -> Served {
    let (shutdown, _) = watch::channel(());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = specs_router(SpecsState {
        specs: fixture.specs(),
        identity: Arc::new(WebAppIdentity::default()),
        collections: fixture.collections(),
        hosts: Arc::from(listener_hosts("127.0.0.1:4331", addr)),
    });
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Served {
        addr,
        handle,
        _shutdown: shutdown,
    }
}

/// Path, the back address, whether the page holds `Harold`.
const PAGES: [(&str, &str, bool); 7] = [
    ("/c", "/", false),
    ("/c/explainers", "/c", true),
    ("/c/explainers/n.md", "/c/explainers", true),
    ("/c/explainers/a.html", "/c/explainers", true),
    ("/specs", "/c", true),
    ("/specs/harold/a", "/", false),
    ("/specs/harold/missing", "/specs", true),
];

const HAROLD: &str = r#"<a href="/" id="to-chat" class="harold-button">Harold</a>"#;

#[tokio::test(flavor = "multi_thread")]
async fn each_page_in_scope_has_one_data_back_with_its_address() {
    let fixture = Fixture::new();
    let server = web_app_with(&fixture).await;
    for (path, back, _) in PAGES {
        let body = server.get(path).await.text().await.unwrap();
        assert_eq!(body.matches("data-back=").count(), 1, "{path}: {body}");
        assert!(body.contains(&format!(r#" data-back="{back}""#)), "{path}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn harold_control_is_only_where_the_table_says() {
    let fixture = Fixture::new();
    let server = web_app_with(&fixture).await;
    for (path, _, harold) in PAGES {
        let body = server.get(path).await.text().await.unwrap();
        assert_eq!(body.matches(r#"id="to-chat""#).count(), usize::from(harold));
        assert_eq!(body.contains(HAROLD), harold, "{path}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn script_is_named_only_for_the_web_app() {
    let fixture = Fixture::new();
    let web = web_app_with(&fixture).await;
    let desk = desk_with(&fixture).await;
    let script = r#"<script src="/static/back-swipe.js" defer></script>"#;
    for (path, _, _) in PAGES {
        let body = web.get(path).await.text().await.unwrap();
        assert!(body.contains(script), "web app {path}");
        let body = desk.get(path).await.text().await.unwrap();
        assert!(!body.contains("back-swipe.js"), "desk {path}");
    }
    let thread = web.get("/").await.text().await.unwrap();
    assert!(!thread.contains("back-swipe.js"), "the thread");
}

#[tokio::test(flavor = "multi_thread")]
async fn desk_listener_pages_hold_no_harold() {
    let fixture = Fixture::new();
    let desk = desk_with(&fixture).await;
    for path in [
        "/specs",
        "/specs/harold/a",
        "/c",
        "/c/explainers",
        "/c/explainers/n.md",
        "/c/explainers/a.html",
    ] {
        let response = desk.get(path).await;
        assert_eq!(response.status(), 200, "{path}");
        let body = response.text().await.unwrap();
        assert!(!body.contains("harold-button"), "{path}");
        assert!(!body.contains("data-back"), "{path}");
    }
}

#[test]
fn css_gives_44px_targets_and_an_ellipsis() {
    let css = include_str!("../../static/web_app/app.css");
    let rule = |selector: &str| -> String {
        let start = css
            .find(selector)
            .unwrap_or_else(|| panic!("app.css lacks {selector}"));
        let rest = &css[start..];
        rest[..rest.find('}').unwrap()].to_owned()
    };
    assert!(css.contains(".harold-button"));
    let shared = rule(".todos-button,\n.back-button,\n.harold-button {");
    assert!(shared.contains("min-height: var(--target)"), "{shared}");
    assert!(rule(".back-label {").contains("text-overflow: ellipsis"));
    let target = css
        .split("--target: ")
        .nth(1)
        .and_then(|rest| rest.split("px").next())
        .and_then(|px| px.parse::<u32>().ok())
        .expect("app.css sets --target in px");
    assert!(target >= 44, "--target is {target}px");
}
