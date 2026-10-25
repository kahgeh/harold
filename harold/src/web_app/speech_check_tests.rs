//! The speech check page, its files and `POST /speech-check/runs`.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::origin::loopback_hosts;
use super::speech_check::{MAX_BODY_BYTES, parse_run};
use super::{AppState, router};
use crate::outbound::fake::CapturedLogs;
use crate::store::HaroldStore;

const LOGIN: &str = "chief@example.com";
const ROUTES: [(&str, &str); 2] = [("GET", "/speech-check"), ("POST", "/speech-check/runs")];

/// A whole run as the page posts it, every field present.
const FULL_RUN: &str = r#"{
    "standalone": true,
    "check0_offline": true, "check0_started": true, "check0_results": false, "check0_error": "network",
    "check1_started": true, "check1_results": true, "check1_first_result_ms": 640,
    "check2_started": true, "check2_results": true,
    "check3_started": 4, "check3_errors": ["not-allowed"],
    "check4_results": true,
    "check4_final_at_ms": [3100, null, 3300], "check4_gap_ms": [900, null, 850],
    "check5_started": true, "check5_kept_running": false, "check5_playback_ok": true,
    "check5_words_heard": 3, "check5_error": "aborted",
    "check6_restarted": true, "check6_heard": true
}"#;

/// A run of exactly 4 KB: a short one, padded with spaces.
fn at_4_kb() -> String {
    let run = r#"{"standalone":true}"#;
    format!("{run}{}", " ".repeat(MAX_BODY_BYTES - run.len()))
}

struct TestDirectory(std::path::PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("harold-web-speech-check-{}", uuid::Uuid::new_v4()));
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

    /// Posts `body` to the runs route as `speech-check.js` does.
    async fn post(&self, body: impl Into<reqwest::Body>) -> reqwest::Response {
        reqwest::Client::new()
            .post(self.url("/speech-check/runs"))
            .header("Tailscale-User-Login", LOGIN)
            .header("sec-fetch-site", "same-origin")
            .header("origin", format!("http://{}", self.addr))
            .header("datastar-request", "true")
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
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

#[test]
fn a_whole_run_and_a_part_run_are_read() {
    let run = parse_run(FULL_RUN.as_bytes()).expect("a whole run");
    assert!(run.check0_started && !run.check0_results);
    assert_eq!(run.check1_first_result_ms, Some(640));
    assert_eq!(run.check3_started, 4);
    assert_eq!(run.check4_final_at_ms, [Some(3100), None, Some(3300)]);
    assert!(parse_run(b"{}").is_ok(), "a run that stopped at once");
    assert!(parse_run(br#"{"standalone":false,"check0_error":"network"}"#).is_ok());
}

#[test]
fn a_body_with_anything_but_booleans_numbers_and_error_names_is_refused() {
    let refused = [
        // A word the user said, in a field of its own, in a known field, or as a name.
        r#"{"transcript":"ten eleven twelve"}"#,
        r#"{"check1_error":"ten eleven twelve"}"#,
        r#"{"check1_error":"Network"}"#,
        r#"{"check3_errors":["not-allowed","hello"]}"#,
        r#"{"check1_started":"yes"}"#,
        r#"{"check1_started":1}"#,
        r#"{"check1_first_result_ms":"640"}"#,
        r#"{"check1_first_result_ms":6.5}"#,
        r#"{"check1_first_result_ms":-1}"#,
        r#"{"check1_first_result_ms":600001}"#,
        r#"{"check1_first_result_ms":{"a":1}}"#,
        r#"{"check3_started":6}"#,
        r#"{"check3_started":-1}"#,
        r#"{"check3_errors":["network","network","network","network","network","network"]}"#,
        r#"{"check4_final_at_ms":[1,2,3,4]}"#,
        r#"{"check4_gap_ms":[1,"2"]}"#,
        r#"{"check5_words_heard":21}"#,
        r#"{"standalone":null}"#,
        r#"[]"#,
        r#""ok""#,
        r#"{"standalone":true"#,
        "",
    ];
    for body in refused {
        assert!(parse_run(body.as_bytes()).is_err(), "{body}");
    }
}

#[test]
fn a_body_over_4_kb_is_refused() {
    let padded = format!(r#"{{"standalone":true{}}}"#, " ".repeat(MAX_BODY_BYTES + 1));
    assert!(parse_run(padded.as_bytes()).is_err());
    let exact = at_4_kb();
    assert_eq!(exact.len(), MAX_BODY_BYTES);
    assert!(parse_run(exact.as_bytes()).is_ok());
}

#[tokio::test(flavor = "current_thread")]
async fn a_run_is_answered_204_and_logged_as_one_line_with_its_fields() {
    let logs = CapturedLogs::start();
    let directory = TestDirectory::new();
    let server = TestServer::serve(directory.store().await).await;

    let response = server.post(FULL_RUN).await;
    assert_eq!(response.status(), 204);
    assert!(response.bytes().await.unwrap().is_empty());

    let captured = logs.text();
    let line = captured
        .lines()
        .find(|line| line.contains("app speech check"))
        .unwrap_or_else(|| panic!("no line: {captured}"));
    for field in [
        "standalone=true",
        "check0_offline=true",
        "check0_started=true",
        "check0_results=false",
        r#"check0_error="network""#,
        "check1_first_result_ms=640",
        "check2_results=true",
        "check3_started=4",
        r#"check3_errors="not-allowed""#,
        r#"check4_final_at_ms="3100,-,3300""#,
        r#"check4_gap_ms="900,-,850""#,
        "check5_kept_running=false",
        "check5_playback_ok=true",
        "check5_words_heard=3",
        r#"check5_error="aborted""#,
        "check6_restarted=true",
        "check6_heard=true",
    ] {
        assert!(line.contains(field), "{field} missing from {line}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn anything_else_gets_400_over_4_kb_gets_413_and_neither_is_logged_as_a_run() {
    let logs = CapturedLogs::start();
    let directory = TestDirectory::new();
    let server = TestServer::serve(directory.store().await).await;

    for body in [
        r#"{"transcript":"ten eleven twelve"}"#,
        r#"{"check1_error":"ten eleven twelve"}"#,
        r#"{"check1_started":"yes"}"#,
        "not json",
        "",
    ] {
        let response = server.post(body).await;
        assert_eq!(response.status(), 400, "{body}");
        assert!(response.bytes().await.unwrap().is_empty());
    }
    let long = format!(
        r#"{{"standalone":true,"x":"{}"}}"#,
        "a".repeat(MAX_BODY_BYTES)
    );
    let response = server.post(long).await;
    assert_eq!(response.status(), 413);
    let exact = format!(
        r#"{{"standalone":true}}{}"#,
        " ".repeat(MAX_BODY_BYTES - 21)
    );
    assert_eq!(server.post(exact).await.status(), 204, "4 KB is allowed");

    let captured = logs.text();
    assert_eq!(
        captured
            .lines()
            .filter(|line| line.contains("app speech check standalone"))
            .count(),
        1,
        "only the 4 KB run was logged: {captured}"
    );
    assert!(!captured.contains("ten eleven twelve"), "{captured}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_page_and_the_runs_route_have_the_login_and_origin_checks() {
    let directory = TestDirectory::new();
    let server = TestServer::serve(directory.store().await).await;
    let client = reqwest::Client::new();

    for login in [None, Some("someone@else")] {
        for (method, path) in ROUTES {
            let mut request = match method {
                "GET" => client.get(server.url(path)),
                _ => client
                    .post(server.url(path))
                    .header("sec-fetch-site", "same-origin")
                    .header("content-type", "application/json")
                    .body(FULL_RUN),
            };
            if let Some(login) = login {
                request = request.header("Tailscale-User-Login", login);
            }
            assert_eq!(
                request.send().await.unwrap().status(),
                403,
                "{method} {path} {login:?}"
            );
        }
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
            .post(server.url("/speech-check/runs"))
            .header("Tailscale-User-Login", LOGIN)
            .body(FULL_RUN);
        for (name, value) in &headers {
            request = request.header(*name, *value);
        }
        assert_eq!(request.send().await.unwrap().status(), 403, "{headers:?}");
    }
    assert_eq!(server.post(FULL_RUN).await.status(), 204);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_page_lists_the_seven_checks_and_loads_only_its_own_script() {
    let directory = TestDirectory::new();
    let server = TestServer::serve(directory.store().await).await;

    let page = server.get("/speech-check").await;
    assert_eq!(page.status(), 200);
    assert!(
        page.headers()["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("script-src 'self'")
    );
    let html = page.text().await.unwrap();
    for check in 0..=6 {
        assert!(
            html.contains(&format!(r#"data-check="{check}""#)),
            "check {check}"
        );
    }
    for wanted in [
        "Recognition with the network off",
        r#"src="/static/speech-check.js""#,
        r#"id="speech-go""#,
        "never gets a word you say",
    ] {
        assert!(html.contains(wanted), "{wanted}");
    }
    assert!(!html.contains("<script>"), "no inline script");
    assert!(!html.contains("style="), "no inline style");

    let script = server.get("/static/speech-check.js").await;
    assert_eq!(script.status(), 200);
    assert_eq!(script.headers()["content-type"], "text/javascript");
    let sentence = server.get("/static/check-sentence.wav").await;
    assert_eq!(sentence.status(), 200);
    assert_eq!(sentence.headers()["content-type"], "audio/wav");
    let wav = sentence.bytes().await.unwrap();
    assert_eq!(&wav[..4], b"RIFF");
    assert!(
        (150_000..250_000).contains(&wav.len()),
        "about 190 KB, got {}",
        wav.len()
    );
}
