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
use crate::store::HaroldStore;

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
