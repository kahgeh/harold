//! The dictation check route and the reply bar's mic, stop button and listening strip.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::origin::loopback_hosts;
use super::{AppState, router};
use crate::outbound::fake::CapturedLogs;
use crate::store::HaroldStore;

const LOGIN: &str = "chief@example.com";
const RESULTS: [&str; 6] = [
    "missing",
    "ok",
    "not_allowed",
    "service_not_allowed",
    "network",
    "no_speech",
];

struct TestDirectory(std::path::PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("harold-web-dictation-{}", uuid::Uuid::new_v4()));
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
    _shutdown: watch::Sender<()>,
    handle: JoinHandle<()>,
}

impl TestServer {
    async fn serve(store: Arc<HaroldStore>) -> Self {
        let (shutdown, shutdown_rx) = watch::channel(());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = AppState::for_tests(
            store,
            LOGIN,
            "BPublicKeyForTheTestPage",
            Arc::from(loopback_hosts("127.0.0.1:0", addr)),
            shutdown_rx,
            None,
            Default::default(),
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

    /// Posts `body` to the check as `dictation.js` sends it.
    async fn check(&self, body: &str) -> reqwest::Response {
        reqwest::Client::new()
            .post(self.url("/dictation/check"))
            .header("Tailscale-User-Login", LOGIN)
            .header("sec-fetch-site", "same-origin")
            .header("origin", format!("http://{}", self.addr))
            .header("datastar-request", "true")
            .header("content-type", "application/json")
            .body(body.to_owned())
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
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn each_known_result_is_logged_with_its_word_and_answered_204() {
    let logs = CapturedLogs::start();
    let directory = TestDirectory::new();
    let server = TestServer::serve(directory.store().await).await;

    for result in RESULTS.into_iter().chain(["audio_capture"]) {
        let response = server.check(&format!(r#"{{"result":"{result}"}}"#)).await;
        assert_eq!(response.status(), 204, "{result}");
        assert!(response.bytes().await.unwrap().is_empty());
        let captured = logs.text();
        assert!(
            captured.contains(&format!(r#"app dictation check result="{result}""#))
                || captured.contains(&format!("app dictation check result={result}")),
            "{result}: {captured}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn an_unknown_word_a_long_body_or_bad_json_gets_400_and_no_log_line() {
    let logs = CapturedLogs::start();
    let directory = TestDirectory::new();
    let server = TestServer::serve(directory.store().await).await;
    let long = format!(r#"{{"result":"ok","note":"{}"}}"#, "x".repeat(60));
    assert!(long.len() > 64);

    for body in [
        r#"{"result":"fine"}"#,
        r#"{"result":""}"#,
        r#"{"result":"OK"}"#,
        r#"{"result":3}"#,
        r#"{}"#,
        r#"{"result":"ok""#,
        "ok",
        "",
        long.as_str(),
    ] {
        let response = server.check(body).await;
        assert_eq!(response.status(), 400, "{body}");
        assert!(response.bytes().await.unwrap().is_empty());
    }
    assert!(
        !logs.text().contains("app dictation check"),
        "{}",
        logs.text()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_check_has_the_login_host_and_origin_checks() {
    let directory = TestDirectory::new();
    let server = TestServer::serve(directory.store().await).await;
    let client = reqwest::Client::new();
    let body = r#"{"result":"ok"}"#;

    for login in [None, Some("someone@else")] {
        let mut request = client
            .post(server.url("/dictation/check"))
            .header("sec-fetch-site", "same-origin")
            .header("content-type", "application/json")
            .body(body);
        if let Some(login) = login {
            request = request.header("Tailscale-User-Login", login);
        }
        assert_eq!(request.send().await.unwrap().status(), 403, "{login:?}");
    }
    for headers in [
        vec![("content-type", "application/json")],
        vec![
            ("sec-fetch-site", "cross-site"),
            ("content-type", "application/json"),
        ],
        vec![("sec-fetch-site", "same-origin")],
        vec![
            ("sec-fetch-site", "same-origin"),
            ("content-type", "text/plain"),
        ],
        vec![
            ("host", "evil.example"),
            ("sec-fetch-site", "same-origin"),
            ("content-type", "application/json"),
        ],
    ] {
        let mut request = client
            .post(server.url("/dictation/check"))
            .header("Tailscale-User-Login", LOGIN)
            .body(body);
        for (name, value) in &headers {
            request = request.header(*name, *value);
        }
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), 403, "{headers:?}");
        assert!(response.bytes().await.unwrap().is_empty());
    }
}

/// The reply box is a `textarea` that sends on Enter without Shift and is 3 lines tall.
pub(super) fn assert_reply_box_is_a_textarea(form: &str) {
    let start = form.find("<textarea").expect("the reply box is a textarea");
    let tag = &form[start..form[start..].find('>').unwrap() + start];
    assert!(tag.contains("data-bind:reply"), "{tag}");
    assert!(tag.contains(r#"enterkeyhint="send""#), "{tag}");
    assert!(tag.contains(r#"rows="3""#), "{tag}");
    assert!(
        tag.contains("evt.key === 'Enter' && !evt.shiftKey && !evt.isComposing"),
        "{tag}"
    );
    assert!(
        tag.contains("if (!$sending) el.form.requestSubmit()"),
        "Enter while a send runs must not send again: {tag}"
    );
    assert!(!tag.contains("style.height"), "{tag}");
    assert!(!form.contains(r#"<input type="text""#), "{form}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_bar_holds_send_the_mic_and_the_stop_button_and_the_listening_strip_sits_above_it() {
    let directory = TestDirectory::new();
    let server = TestServer::serve(directory.store().await).await;

    let page = server.get("/").await.text().await.unwrap();

    let form = &page[page.find(r#"<form id="reply""#).unwrap()..];
    let form = &form[..form.find("</form>").unwrap()];
    let button_with = |label: &str| -> String {
        let at = form
            .find(&format!(r#"aria-label="{label}""#))
            .unwrap_or_else(|| panic!("no {label} button: {form}"));
        let start = form[..at].rfind("<button").unwrap();
        let rest = &form[start..];
        rest[..rest.find("</button>").unwrap()].to_owned()
    };
    let text_box = form.find("data-bind:reply").expect("the text box");
    assert_reply_box_is_a_textarea(form);
    let photo = form.find(r#"aria-label="Add a photo""#).unwrap();
    let mic = form.find(r#"aria-label="Dictate a reply""#).unwrap();
    let stop = form.find(r#"aria-label="Stop listening""#).unwrap();
    let send = form.find(r#"aria-label="Send""#).unwrap();
    assert!(
        text_box < photo && photo < mic.min(stop).min(send),
        "{form}"
    );

    // The mic shows only while dictation may work, the box is blank and no photo is chosen.
    let mic = button_with("Dictate a reply");
    assert!(mic.contains(r#"type="button""#), "{mic}");
    assert!(mic.contains("icon-button"), "{mic}");
    assert!(
        mic.contains(
            r#"data-attr:hidden="!($dictation && !$listening && $reply.trim() === '' && $photo === '')""#
        ),
        "{mic}"
    );
    assert!(
        mic.contains(" hidden "),
        "hidden until Datastar shows it: {mic}"
    );
    // The stop button shows while listening.
    let stop = button_with("Stop listening");
    assert!(stop.contains(r#"type="button""#), "{stop}");
    assert!(stop.contains(r#"data-attr:hidden="!$listening""#), "{stop}");
    assert!(
        stop.contains(" hidden "),
        "hidden until Datastar shows it: {stop}"
    );
    // Send shows otherwise.
    let send = button_with("Send");
    assert!(send.contains(r#"type="submit""#), "{send}");
    assert!(
        send.contains(
            r#"data-attr:hidden="!!($listening || ($dictation && $reply.trim() === '' && $photo === ''))""#
        ),
        "{send}"
    );
    assert!(send.contains(r#"data-attr:disabled="$sending""#), "{send}");

    // The signals dictation.js sets, as hidden inputs Datastar binds.
    assert!(
        form.contains(r#"<input type="hidden" id="dictation-on" data-bind:dictation>"#),
        "{form}"
    );
    assert!(
        form.contains(r#"<input type="hidden" id="dictation-listening" data-bind:listening>"#),
        "{form}"
    );

    // The strip is above the bar, hidden until it listens.
    let strip = page
        .find(r#"<p id="listening-note" class="listening-note""#)
        .unwrap();
    assert!(strip < page.find(r#"<form id="reply""#).unwrap());
    assert!(
        page[strip..].starts_with(
            r#"<p id="listening-note" class="listening-note" role="status" hidden><span class="listen-dot"></span><span id="listening-text">Listening</span></p>"#
        ),
        "{}",
        &page[strip..strip + 200]
    );
    assert!(
        page.contains(r#"<script src="/static/dictation.js" defer></script>"#),
        "{page}"
    );

    let script = server.get("/static/dictation.js").await;
    assert_eq!(script.status(), 200);
    assert_eq!(script.headers()["content-type"], "text/javascript");
}
