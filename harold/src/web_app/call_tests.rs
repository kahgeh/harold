//! The call routes on a real server, with the phone's calls on fake ports: a fake worker
//! that answers at once, a fake chief pane, and real time.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use reqwest::{Method, StatusCode};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep, timeout};

use askama::Template;

use super::{AppCalls, EndDeskCall, PhoneCallApi, refusal_status};
use crate::outbound::fake::CapturedLogs;
use crate::outbound::{AppDelivery, AppRoute};
use crate::settings::WebAppSettings;
use crate::store::{AppSender, EventVersion, HaroldStore, append_app_push_subscribed};
use crate::telemetry::test_support;
use crate::voice_input::tests::{FakePorts, loud, quiet, text, typed};
use crate::voice_input::{
    AudioRefused, CallPorts, CallState, LineTaker, PIECE_LIMIT_BYTES, PhoneCalls, PhoneSound,
    PhoneSpeech, PhoneView, Said, Speaker,
};
use crate::web_app::fake_push::FakePushService;
use crate::web_app::origin::loopback_hosts;
use crate::web_app::pages::{CONTENT_SECURITY_POLICY, CallSection};
use crate::web_app::push::{Pusher, VapidKey, vapid_key_path};
use crate::web_app::route::WebAppRoute;
use crate::web_app::{AppState, router, start};

const LOGIN: &str = "chief@example.com";
const CANARY: &str = "CALL-CANARY-5520";
const NOT_SET_UP: &str = "Voice is not set up on the Mac";
/// How long a test waits for anything before it fails.
const LIMIT: Duration = Duration::from_secs(5);
/// Samples in one posted piece: a quarter of a second at 16 kHz.
const PIECE_SAMPLES: usize = 4_000;
const PIECE_MS: u64 = 250;
const MARKUP: &str = "<script>alert(1)</script> {{x}} $reply @post('/x')";
/// `MARKUP` as askama escapes it: `<` and `>` become `&#60;` and `&#62;`.
const ESCAPED: &str = "&#60;script&#62;alert(1)&#60;/script&#62;";

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("harold-web-call-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("create test directory");
        Self(path)
    }

    fn store_path(&self) -> PathBuf {
        self.0.join("store")
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
    async fn serve(store: Arc<HaroldStore>, calls: Option<Arc<dyn PhoneCallApi>>) -> Self {
        let (shutdown, shutdown_rx) = watch::channel(());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = AppState {
            store,
            allowed_login: Arc::from(LOGIN),
            public_key: Arc::from("BPublicKeyForTheTestPage"),
            loopback_hosts: Arc::from(loopback_hosts("127.0.0.1:0", addr)),
            shutdown: shutdown_rx,
            calls,
        };
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

    /// Sends `method` to `path` with only the given headers, the login not included.
    async fn send_with(
        &self,
        method: Method,
        path: &str,
        headers: &[(&str, &str)],
        body: Vec<u8>,
    ) -> reqwest::Response {
        let mut request = reqwest::Client::new().request(method, self.url(path));
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        timeout(LIMIT, request.body(body).send())
            .await
            .expect("an answer within the limit")
            .unwrap()
    }

    /// Sends `method` to `path` as the app's own page does: the login, same-origin, and
    /// the audio's media type.
    async fn send(&self, method: Method, path: &str, body: Vec<u8>) -> reqwest::Response {
        let origin = format!("http://{}", self.addr);
        let headers = [
            ("tailscale-user-login", LOGIN),
            ("sec-fetch-site", "same-origin"),
            ("origin", origin.as_str()),
            ("content-type", "application/octet-stream"),
        ];
        self.send_with(method, path, &headers, body).await
    }

    async fn get(&self, path: &str) -> reqwest::Response {
        self.send_with(
            Method::GET,
            path,
            &[("tailscale-user-login", LOGIN)],
            Vec::new(),
        )
        .await
    }

    async fn post_audio(&self, at_ms: u64, bytes: Vec<u8>) -> StatusCode {
        let path = format!("/call/audio?at={at_ms}");
        self.send(Method::POST, &path, bytes).await.status()
    }

    /// Opens `GET /updates`.
    async fn updates(&self) -> EventReader {
        let response = self.get("/updates").await;
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["content-type"], "text/event-stream");
        EventReader {
            response,
            buffer: String::new(),
        }
    }
}

/// Reads one SSE response event by event.
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
            let chunk = timeout(LIMIT, self.response.chunk())
                .await
                .expect("an event within the limit")
                .expect("the stream reads");
            self.buffer.push_str(std::str::from_utf8(&chunk?).unwrap());
        }
    }

    /// The first event from now that holds `wanted`.
    async fn until(&mut self, wanted: &str) -> String {
        loop {
            let event = self.next().await.expect("the stream goes on");
            if event.contains(wanted) {
                return event;
            }
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

fn bytes(samples: &[i16]) -> Vec<u8> {
    samples
        .iter()
        .flat_map(|sample| sample.to_le_bytes())
        .collect()
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

/// The phone's calls on fake ports behind the app's routes.
struct Phone {
    directory: TestDirectory,
    store: Arc<HaroldStore>,
    base: FakePorts,
    calls: Arc<AppCalls<FakePorts>>,
    /// For each time the desk call was ended: how many workers had started when it was.
    desk_ends: Arc<Mutex<Vec<usize>>>,
    server: TestServer,
    /// When the last `POST /call` was sent, which pieces are stamped from, and where the
    /// audio posted so far ends, in milliseconds from then.
    clock: Mutex<(Instant, u64)>,
}

impl Phone {
    async fn new(set_up: bool) -> Self {
        let directory = TestDirectory::new();
        let store = Arc::new(HaroldStore::open(directory.store_path()).await.unwrap());
        let base = FakePorts::new();
        let desk_ends: Arc<Mutex<Vec<usize>>> = Arc::default();
        let end_desk: EndDeskCall = {
            let base = base.clone();
            let desk_ends = Arc::clone(&desk_ends);
            Arc::new(move || {
                let base = base.clone();
                let desk_ends = Arc::clone(&desk_ends);
                Box::pin(async move {
                    // A desk call takes a moment to end and clean up.
                    sleep(Duration::from_millis(50)).await;
                    let starts = base.worker_log().starts;
                    desk_ends.lock().unwrap().push(starts);
                })
            })
        };
        let phone = PhoneCalls::new(base.clone(), set_up);
        let calls = Arc::new(AppCalls::new(phone, Arc::clone(&store), end_desk));
        let api: Arc<dyn PhoneCallApi> = calls.clone();
        let server = TestServer::serve(Arc::clone(&store), Some(api)).await;
        Self {
            directory,
            store,
            base,
            calls,
            desk_ends,
            server,
            clock: Mutex::new((Instant::now(), 0)),
        }
    }

    /// `POST /call`, which must start the call, and half a second of background.
    async fn start(&self) {
        let sent = Instant::now();
        let response = self.server.send(Method::POST, "/call", Vec::new()).await;
        assert_eq!(response.status(), 204);
        // The call opened after `sent`, so a piece stamped with the time since `sent` was
        // recorded after it opened, and is heard.
        *self.clock.lock().unwrap() = (sent, 0);
        self.post(quiet(500)).await;
    }

    /// Milliseconds since `POST /call` was sent, as the page stamps a piece.
    fn now_ms(&self) -> u64 {
        u64::try_from(self.clock.lock().unwrap().0.elapsed().as_millis()).unwrap()
    }

    /// Posts `samples` in quarter-second pieces with rising `at`, each of which the call
    /// must take. They are recorded from now, or from where the audio posted before ends
    /// when that is later: the test posts faster than a phone records, and the core mutes
    /// audio by when it was recorded.
    async fn post(&self, samples: Vec<i16>) {
        let from = {
            let now = self.now_ms();
            let mut clock = self.clock.lock().unwrap();
            let from = clock.1.max(now);
            let pieces = u64::try_from(samples.len().div_ceil(PIECE_SAMPLES)).unwrap();
            clock.1 = from + PIECE_MS * pieces;
            from
        };
        for (n, piece) in samples.chunks(PIECE_SAMPLES).enumerate() {
            let at = from + PIECE_MS * u64::try_from(n).unwrap();
            assert_eq!(self.server.post_audio(at, bytes(piece)).await, 204);
        }
    }

    /// One second of speech and the 1.2 s of silence that ends it.
    async fn speak(&self) {
        let mut samples = loud(1000);
        samples.extend(quiet(1200));
        self.post(samples).await;
    }

    async fn wait_for_typed(&self, transcript: &str) {
        let want = typed("%2", transcript);
        let panes = self.base.fake_panes();
        let deadline = Instant::now() + LIMIT;
        while panes.keys() != want {
            if Instant::now() >= deadline {
                let transcribed = self.base.worker_log().transcribed.clone();
                let status = self.calls.view().borrow().clone();
                panic!(
                    "not typed within {LIMIT:?}: keys {:?}, transcribed {transcribed:?}, {status:?}",
                    panes.keys()
                );
            }
            sleep(Duration::from_millis(20)).await;
        }
    }

    /// Says the chief's `line` into the phone's calls from a blocking thread, as the
    /// projector does.
    fn say(&self, line: &str) -> JoinHandle<Option<bool>> {
        let calls = Arc::clone(&self.calls);
        let line = line.to_owned();
        tokio::task::spawn_blocking(move || {
            LineTaker::say_blocking(calls.as_ref(), &line, Speaker::Chief)
        })
    }

    /// The id of the chief's line once it is offered to the phone.
    async fn offered_line(&self, line: &str) -> String {
        let mut view = self.calls.view();
        let wanted = Said::Line {
            text: line.to_owned(),
            by: Speaker::Chief,
        };
        let offered = timeout(
            LIMIT,
            view.wait_for(|view| view.sound.as_ref().is_some_and(|s| s.said == wanted)),
        )
        .await
        .expect("the line is offered within the limit")
        .expect("the view is open");
        // Copied out, so no borrow of the view is held while the line answers.
        offered.sound.clone().unwrap().id
    }

    fn call_folders(&self) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(self.base.calls_dir()) else {
            return Vec::new();
        };
        entries
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("call-"))
            })
            .collect()
    }
}

async fn finished<T>(handle: JoinHandle<T>) -> T {
    timeout(LIMIT, handle)
        .await
        .expect("done within the limit")
        .unwrap()
}

// ---------------------------------------------------------------------------
// Acceptance
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_phone_turn_is_typed_and_the_chiefs_line_is_offered_and_not_pushed() {
    let push = FakePushService::start(201).await;
    let phone = Phone::new(true).await;
    append_app_push_subscribed(&phone.store, &push.subscription(1))
        .await
        .unwrap();
    phone.store.project_unhandled_events(100).await.unwrap();
    phone
        .base
        .worker_log()
        .script
        .push_back(text("status of the pod"));
    let mut updates = phone.server.updates().await;

    phone.start().await;
    phone.speak().await;
    phone.wait_for_typed("status of the pod").await;

    let said = phone.say("Two specs wait.");
    let id = phone.offered_line("Two specs wait.").await;
    // The line is announced on the event stream, for the phone to fetch.
    let announced = updates.until(&format!(r#"data-harold-sound="{id}""#)).await;
    assert!(announced.contains(r#"id="call""#), "{announced}");
    assert!(announced.contains("Two specs wait."), "{announced}");
    let answer = phone.server.get(&format!("/call/answer/{id}")).await;
    assert_eq!(answer.status(), 200);
    assert_eq!(answer.headers()["content-type"], "audio/wav");
    // The fake worker writes the line's text as the sound's file.
    assert_eq!(answer.bytes().await.unwrap(), b"Two specs wait.".as_slice());
    let played = format!("/call/played?id={id}");
    let response = phone.server.send(Method::POST, &played, Vec::new()).await;
    assert_eq!(response.status(), 204);

    assert_eq!(finished(said).await, Some(true));
    let messages = phone.store.app_messages_since(0).await.unwrap();
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert_eq!(messages[0].sender, AppSender::Chief);
    assert_eq!(messages[0].text, "Two specs wait.");
    assert!(
        messages[0].event_id.starts_with("call-line-"),
        "{}",
        messages[0].event_id
    );
    assert!(
        push.requests().is_empty(),
        "a line played on the phone is not pushed"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn after_delete_the_chiefs_next_line_goes_to_the_thread_with_a_push() {
    let push = FakePushService::start(201).await;
    let phone = Phone::new(true).await;
    append_app_push_subscribed(&phone.store, &push.subscription(1))
        .await
        .unwrap();
    phone.store.project_unhandled_events(100).await.unwrap();
    phone.start().await;
    phone.speak().await;
    phone.wait_for_typed("hello there").await;
    let said = phone.say("Two specs wait.");
    let id = phone.offered_line("Two specs wait.").await;

    let response = phone.server.send(Method::DELETE, "/call", Vec::new()).await;

    assert_eq!(response.status(), 204);
    assert!(
        finished(said).await.is_some(),
        "the call had taken the line"
    );
    assert!(
        phone.call_folders().is_empty(),
        "{:?}",
        phone.call_folders()
    );
    assert_eq!(phone.calls.view().borrow().state, CallState::Off);
    let answer = phone.server.get(&format!("/call/answer/{id}")).await;
    assert_eq!(answer.status(), 404);
    assert_eq!(finished(phone.say("All green.")).await, None);

    let key = VapidKey::load_or_create(&vapid_key_path(&phone.directory.0.join("harold.db")))
        .expect("create the VAPID key");
    let route = Arc::new(WebAppRoute::new(
        Arc::clone(&phone.store),
        Arc::new(Pusher::new(key, LOGIN)),
    ));
    let delivered =
        tokio::task::spawn_blocking(move || route.deliver("chief-line-13", "All green."));
    assert_eq!(finished(delivered).await, Ok(AppDelivery::Pushed));
    assert_eq!(push.requests().len(), 1);
    let messages = phone.store.app_messages_since(0).await.unwrap();
    assert!(
        messages.iter().any(|message| message.text == "All green."),
        "{messages:?}"
    );
}

/// Current-thread runtime: the server's handlers and the call run on this thread, so
/// `CapturedLogs` sees their lines, and the exported records catch any line from any thread.
#[tokio::test(flavor = "current_thread")]
async fn call_canary_reaches_no_log_event_row_or_export() {
    test_support::install();
    let logs = CapturedLogs::start();
    let phone = Phone::new(true).await;
    phone.base.worker_log().script.push_back(text(CANARY));

    phone.start().await;
    phone.speak().await;
    phone.wait_for_typed(CANARY).await;
    let response = phone.server.send(Method::DELETE, "/call", Vec::new()).await;
    assert_eq!(response.status(), 204);

    let captured = logs.text();
    assert!(captured.contains("app call started"), "{captured}");
    assert!(captured.contains("spoken prompt typed"), "{captured}");
    assert!(
        captured.contains("app call ended reason=\"stopped\""),
        "{captured}"
    );
    assert!(!captured.contains(CANARY), "{captured}");
    assert!(events(&phone.store).await.is_empty());
    phone.store.project_unhandled_events(100).await.unwrap();
    assert!(phone.store.app_messages_since(0).await.unwrap().is_empty());
    assert!(!test_support::exported_text().contains(CANARY));
}

// ---------------------------------------------------------------------------
// The inputs the acceptance criteria leave out
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn audio_with_no_call_open_is_refused_and_queues_nothing() {
    let phone = Phone::new(true).await;
    phone.base.worker_log().script.push_back(text("after"));
    // Background, a turn and the silence that ends it, stamped a second in: the next call
    // would hear it as a turn, were any of it kept for that call.
    let mut speech = quiet(500);
    speech.extend(loud(1000));
    speech.extend(quiet(1500));
    let post_refused = || async {
        for (n, piece) in speech.chunks(PIECE_SAMPLES).enumerate() {
            let at = 1000 + PIECE_MS * u64::try_from(n).unwrap();
            assert_eq!(phone.server.post_audio(at, bytes(piece)).await, 409);
        }
    };
    post_refused().await;
    // Pieces still in flight after End.
    phone.start().await;
    let response = phone.server.send(Method::DELETE, "/call", Vec::new()).await;
    assert_eq!(response.status(), 204);
    post_refused().await;

    phone.start().await;
    phone.post(quiet(1500)).await;
    sleep(Duration::from_millis(500)).await;
    assert!(phone.base.worker_log().transcribed.is_empty());
    assert!(phone.base.fake_panes().keys().is_empty());

    // The call hears what is posted to it.
    phone.speak().await;
    phone.wait_for_typed("after").await;
    assert_eq!(phone.base.worker_log().transcribed.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn malformed_audio_is_refused_and_the_call_goes_on() {
    let phone = Phone::new(true).await;
    phone.base.worker_log().script.push_back(text("still here"));
    phone.start().await;
    let at = phone.now_ms();

    assert_eq!(phone.server.post_audio(at, vec![0; 3]).await, 400);
    let too_large = vec![0; PIECE_LIMIT_BYTES + 2];
    assert_eq!(too_large.len(), 32_002);
    assert_eq!(phone.server.post_audio(at, too_large).await, 413);
    for path in [
        "/call/audio",
        "/call/audio?at=",
        "/call/audio?at=soon",
        "/call/audio?at=-5",
    ] {
        let response = phone.server.send(Method::POST, path, vec![0; 8_000]).await;
        assert_eq!(response.status(), 400, "{path}");
    }

    phone.speak().await;
    phone.wait_for_typed("still here").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn starting_a_phone_call_ends_the_desk_call_first() {
    let phone = Phone::new(true).await;

    phone.start().await;

    assert_eq!(*phone.desk_ends.lock().unwrap(), [0]);
    assert_eq!(phone.base.worker_log().starts, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_post_call_answers_the_open_call() {
    let phone = Phone::new(true).await;

    phone.start().await;
    phone.start().await;

    assert_eq!(phone.base.worker_log().starts, 1);
    assert_eq!(phone.calls.view().borrow().state, CallState::Listening);
}

#[tokio::test(flavor = "multi_thread")]
async fn post_call_says_why_no_call_started() {
    async fn refused(server: &TestServer) -> (StatusCode, String, String) {
        let response = server.send(Method::POST, "/call", Vec::new()).await;
        let status = response.status();
        let content_type = response.headers()["content-type"]
            .to_str()
            .unwrap()
            .to_owned();
        (status, content_type, response.text().await.unwrap())
    }

    let no_chief = Phone::new(true).await;
    no_chief.base.fake_panes().set(|s| s.view.panes.clear());
    let (status, content_type, body) = refused(&no_chief.server).await;
    assert_eq!(status, 409);
    assert!(content_type.starts_with("text/plain"), "{content_type}");
    assert_eq!(body, "There's no chief running.");

    // The call's folder cannot be made: the voice fails.
    let failing = Phone::new(true).await;
    let calls_dir = failing.base.calls_dir();
    std::fs::create_dir_all(calls_dir.parent().unwrap()).unwrap();
    std::fs::write(&calls_dir, "not a folder").unwrap();
    let (status, _, body) = refused(&failing.server).await;
    assert_eq!(status, 409);
    assert_eq!(body, "The voice failed. Call ended.");

    let not_set_up = Phone::new(false).await;
    let (status, content_type, body) = refused(&not_set_up.server).await;
    assert_eq!(status, 409);
    assert!(content_type.starts_with("text/plain"), "{content_type}");
    assert_eq!(body, NOT_SET_UP);
    assert!(not_set_up.desk_ends.lock().unwrap().is_empty());

    let directory = TestDirectory::new();
    let store = Arc::new(HaroldStore::open(directory.store_path()).await.unwrap());
    let no_calls = TestServer::serve(store, None).await;
    let (status, _, body) = refused(&no_calls).await;
    assert_eq!(status, 409);
    assert_eq!(body, NOT_SET_UP);
    for (method, path) in [
        (Method::POST, "/call/audio?at=0"),
        (Method::POST, "/call/played?id=x"),
        (Method::POST, "/call/talk-now"),
        (Method::DELETE, "/call"),
    ] {
        let response = no_calls.send(method, path, vec![0; 2]).await;
        assert_eq!(response.status(), 409, "{path}");
    }
    assert_eq!(no_calls.get("/call/answer/x").await.status(), 409);
}

#[tokio::test(flavor = "multi_thread")]
async fn every_call_route_needs_the_login_and_the_apps_own_origin() {
    let phone = Phone::new(true).await;
    let server = &phone.server;
    let origin = format!("http://{}", server.addr);
    let routes = [
        (Method::POST, "/call"),
        (Method::POST, "/call/audio?at=0"),
        (Method::POST, "/call/played?id=x"),
        (Method::POST, "/call/talk-now"),
        (Method::DELETE, "/call"),
        (Method::GET, "/call/answer/x"),
    ];
    let page = [
        ("sec-fetch-site", "same-origin"),
        ("origin", origin.as_str()),
        ("content-type", "application/octet-stream"),
    ];
    let login = ("tailscale-user-login", LOGIN);
    let cross_site = [
        login,
        ("sec-fetch-site", "cross-site"),
        ("origin", "https://evil.example"),
        ("content-type", "application/octet-stream"),
    ];
    let simple = [
        login,
        ("sec-fetch-site", "same-origin"),
        ("content-type", "text/plain"),
    ];
    let foreign_host: Vec<_> = page
        .iter()
        .copied()
        .chain([login, ("host", "evil.example")])
        .collect();

    for (method, path) in routes {
        let reads_only = method == Method::GET;
        let mut refusals: Vec<&[(&str, &str)]> = vec![&page, &foreign_host];
        if !reads_only {
            refusals.extend([&cross_site[..], &simple[..]]);
        }
        for headers in refusals {
            let response = server
                .send_with(method.clone(), path, headers, bytes(&quiet(250)))
                .await;
            assert_eq!(response.status(), 403, "{method} {path} with {headers:?}");
            assert!(response.bytes().await.unwrap().is_empty());
        }
    }

    assert!(phone.desk_ends.lock().unwrap().is_empty());
    assert_eq!(phone.base.worker_log().starts, 0);
    assert_eq!(phone.calls.view().borrow().state, CallState::Off);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_audio_post_from_the_page_passes_the_origin_check() {
    let phone = Phone::new(true).await;
    let server = &phone.server;
    let origin = format!("http://{}", server.addr);
    let login = ("tailscale-user-login", LOGIN);
    let page_posts: [&[(&str, &str)]; 2] = [
        &[
            login,
            ("sec-fetch-site", "same-origin"),
            ("content-type", "application/octet-stream"),
        ],
        &[
            login,
            ("origin", origin.as_str()),
            ("content-type", "Application/Octet-Stream; charset=binary"),
        ],
    ];

    for headers in page_posts {
        let response = server
            .send_with(Method::POST, "/call/audio?at=0", headers, vec![0; 8_000])
            .await;
        assert_eq!(response.status(), 409, "no call is open: {headers:?}");
    }
    phone.start().await;
    for headers in page_posts {
        let response = server
            .send_with(Method::POST, "/call/audio?at=600", headers, vec![0; 8_000])
            .await;
        assert_eq!(response.status(), 204, "{headers:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn talk_now_takes_back_the_offered_answer() {
    let phone = Phone::new(true).await;
    phone.start().await;
    phone.speak().await;
    phone.wait_for_typed("hello there").await;
    let said = phone.say("A long answer.");
    let id = phone.offered_line("A long answer.").await;

    let response = phone
        .server
        .send(Method::POST, "/call/talk-now", Vec::new())
        .await;

    assert_eq!(response.status(), 204);
    assert_eq!(finished(said).await, Some(true));
    let answer = phone.server.get(&format!("/call/answer/{id}")).await;
    assert_eq!(answer.status(), 404);
    assert_eq!(phone.calls.view().borrow().sound, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_ends_an_open_phone_call() {
    let directory = TestDirectory::new();
    let store = Arc::new(HaroldStore::open(directory.store_path()).await.unwrap());
    let base = FakePorts::new();
    let end_desk: EndDeskCall = Arc::new(|| Box::pin(async {}));
    let calls = Arc::new(AppCalls::new(
        PhoneCalls::new(base.clone(), true),
        Arc::clone(&store),
        end_desk,
    ));
    let (shutdown, shutdown_rx) = watch::channel(());
    let settings = WebAppSettings {
        listen: "127.0.0.1:0".into(),
        allowed_login: LOGIN.into(),
    };
    let api: Arc<dyn PhoneCallApi> = calls.clone();
    let running = start(
        &settings,
        Arc::clone(&store),
        &directory.store_path(),
        shutdown_rx,
        Some(api),
    )
    .await
    .expect("the app starts");
    let started = calls.start().await;
    assert_eq!(started.state, CallState::Listening);

    drop(shutdown);

    finished(running.handle).await;
    let view = calls.view().borrow().clone();
    assert_eq!(view.state, CallState::Off);
    assert_eq!(view.ended, Some("shutdown"));
}

#[test]
fn each_audio_refusal_has_its_status() {
    assert_eq!(refusal_status(AudioRefused::NoCall), 409);
    assert_eq!(refusal_status(AudioRefused::Malformed), 400);
    assert_eq!(refusal_status(AudioRefused::TooLarge), 413);
    assert_eq!(refusal_status(AudioRefused::Full), 503);
}

// ---------------------------------------------------------------------------
// The call screen
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn the_thread_page_offers_a_call_or_says_voice_is_not_set_up() {
    let offered = r#"<button type="button" id="call-start">📞 Call</button>"#;
    let refused =
        r#"<button type="button" id="call-start" disabled>Voice is not set up on the Mac</button>"#;

    let set_up = Phone::new(true).await;
    let page = set_up.server.get("/").await.text().await.unwrap();
    assert!(page.contains(offered), "{page}");
    assert!(!page.contains(NOT_SET_UP), "{page}");
    assert!(
        page.contains(r#"<script src="/static/call.js" defer></script>"#),
        "{page}"
    );
    assert!(page.contains(r#"id="call""#), "{page}");
    assert!(page.contains("Call with the chief · 0:00"), "{page}");

    let not_set_up = Phone::new(false).await;
    let page = not_set_up.server.get("/").await.text().await.unwrap();
    assert!(page.contains(refused), "{page}");
    assert!(!page.contains("📞 Call"), "{page}");

    let directory = TestDirectory::new();
    let store = Arc::new(HaroldStore::open(directory.store_path()).await.unwrap());
    let no_calls = TestServer::serve(store, None).await;
    let page = no_calls.get("/").await.text().await.unwrap();
    assert!(page.contains(refused), "{page}");
    assert!(page.contains(r#"id="call""#), "{page}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_call_scripts_are_served_as_javascript_under_the_policy() {
    let directory = TestDirectory::new();
    let store = Arc::new(HaroldStore::open(directory.store_path()).await.unwrap());
    let server = TestServer::serve(store, None).await;

    for (name, file) in [
        (
            "call.js",
            include_bytes!("../../static/web_app/call.js").as_slice(),
        ),
        (
            "call-worklet.js",
            include_bytes!("../../static/web_app/call-worklet.js").as_slice(),
        ),
    ] {
        let response = server.get(&format!("/static/{name}")).await;
        assert_eq!(response.status(), 200, "{name}");
        let headers = response.headers().clone();
        assert_eq!(headers["content-type"], "text/javascript", "{name}");
        assert_eq!(
            headers["content-security-policy"], CONTENT_SECURITY_POLICY,
            "{name}"
        );
        assert_eq!(response.bytes().await.unwrap(), file, "{name}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_call_view_change_reaches_the_stream_as_a_patch_of_call() {
    let phone = Phone::new(true).await;
    let mut updates = phone.server.updates().await;
    let thread = updates.next().await.expect("the whole thread");
    assert!(thread.contains(r#"id="messages""#), "{thread}");
    let first = updates.next().await.expect("the call section");
    assert!(first.contains(r#"id="call""#), "{first}");
    assert!(first.contains(r#"data-harold-state="off""#), "{first}");

    phone.start().await;
    let listening = updates.until(r#"data-harold-state="listening""#).await;
    assert!(
        listening.starts_with("event: datastar-patch-elements\n"),
        "{listening}"
    );
    assert!(listening.contains(r#"id="call""#), "{listening}");
    assert!(
        !listening.contains("data: selector"),
        "morphed by id: {listening}"
    );
    assert!(
        !listening.contains("data: mode"),
        "morphed by id: {listening}"
    );
    assert!(listening.contains("Listening"), "{listening}");

    phone.speak().await;
    phone.wait_for_typed("hello there").await;
    let said = phone.say(MARKUP);
    let id = phone.offered_line(MARKUP).await;

    let offered = updates.until(&format!(r#"data-harold-sound="{id}""#)).await;
    assert!(offered.contains(r#"id="call""#), "{offered}");
    assert!(offered.contains(ESCAPED), "{offered}");
    assert!(!offered.contains("<script>alert"), "{offered}");
    assert!(!offered.contains("@post('/x')"), "{offered}");

    let played = format!("/call/played?id={id}");
    let response = phone.server.send(Method::POST, &played, Vec::new()).await;
    assert_eq!(response.status(), 204);
    assert_eq!(finished(said).await, Some(true));
}

fn view(state: CallState) -> PhoneView {
    PhoneView {
        state,
        transcript: None,
        sound: None,
        speech: None,
        ended: None,
    }
}

fn sound(id: &str, said: Said) -> Option<PhoneSound> {
    Some(PhoneSound {
        id: id.to_owned(),
        said,
    })
}

fn render(view: &PhoneView) -> String {
    CallSection::from(view).render().unwrap()
}

#[test]
fn the_call_section_shows_each_state_in_the_specs_words() {
    let hint = r#"<p class="call-hint">Say "bye" or tap End to hang up</p>"#;
    let end = r#"<button type="button" id="call-end" class="call-end">End</button>"#;
    let talk_now =
        r#"<button type="button" id="call-talk-now" class="call-talk-now">Talk now</button>"#;

    for state in [CallState::Off, CallState::NotSetUp] {
        let html = render(&view(state));
        assert!(
            html.starts_with(r#"<section id="call" class="call""#),
            "{html}"
        );
        assert!(html.contains(r#"data-harold-state="off""#), "{html}");
        assert!(html.contains(r#"data-harold-sound="""#), "{html}");
        assert!(html.contains(r#"data-harold-speech="""#), "{html}");
        assert!(!html.contains("call-title"), "{html}");
        assert!(!html.contains(talk_now), "{html}");
    }

    for state in [CallState::Listening, CallState::Transcribing] {
        let mut listening = view(state);
        listening.transcript = Some("what is left on the pod".into());
        let html = render(&listening);
        assert!(html.contains(r#"data-harold-state="listening""#), "{html}");
        assert!(
            html.contains(r#"<h2 class="call-title">Listening</h2>"#),
            "{html}"
        );
        assert!(html.contains(hint), "{html}");
        assert!(html.contains("You said:"), "{html}");
        assert!(html.contains(r#""🎙 what is left on the pod""#), "{html}");
        assert!(html.contains(end), "{html}");
        assert!(!html.contains(talk_now), "{html}");
        assert!(!html.contains(r#"class="bars""#), "{html}");
    }

    // A tick plays while the chief thinks, and leaves the title as it was.
    let mut thinking = view(CallState::Waiting);
    thinking.sound = sound("t1", Said::Tick);
    let html = render(&thinking);
    assert!(html.contains(r#"data-harold-state="thinking""#), "{html}");
    assert!(html.contains(r#"data-harold-sound="t1""#), "{html}");
    assert!(
        html.contains(r#"<h2 class="call-title">Chief is thinking</h2>"#),
        "{html}"
    );
    assert!(html.contains(hint), "{html}");
    assert!(!html.contains("call-line"), "{html}");
    assert!(!html.contains(talk_now), "{html}");

    let mut chief = view(CallState::Speaking);
    chief.sound = sound(
        "s1",
        Said::Line {
            text: "Two specs wait.".into(),
            by: Speaker::Chief,
        },
    );
    let html = render(&chief);
    assert!(html.contains(r#"data-harold-state="speaking""#), "{html}");
    assert!(html.contains(r#"data-harold-sound="s1""#), "{html}");
    assert!(
        html.contains(r#"<h2 class="call-title">Chief speaking</h2>"#),
        "{html}"
    );
    assert!(
        html.contains(r#"<p class="call-line">"Two specs wait."</p>"#),
        "{html}"
    );
    assert!(html.contains("📞"), "{html}");
    assert_eq!(html.matches("<i></i>").count(), 5, "five bars: {html}");
    assert!(html.contains(talk_now), "{html}");
    assert!(html.contains(end), "{html}");
    assert!(!html.contains(hint), "{html}");

    let mut phrase = view(CallState::Speaking);
    phrase.sound = sound("s2", Said::Phrase("Sorry, I didn't catch that.".into()));
    let html = render(&phrase);
    assert!(
        html.contains(r#"<h2 class="call-title">Harold speaking</h2>"#),
        "{html}"
    );
    assert!(html.contains("Sorry, I didn&#39;t catch that."), "{html}");

    let mut speech = view(CallState::Speaking);
    speech.speech = Some(PhoneSpeech {
        id: "p1".into(),
        text: "Bye.".into(),
    });
    let html = render(&speech);
    assert!(html.contains(r#"data-harold-speech="p1""#), "{html}");
    assert!(html.contains(r#"data-harold-speech-text="Bye.""#), "{html}");
    assert!(html.contains(r#"data-harold-sound="""#), "{html}");
    assert!(
        html.contains(r#"<h2 class="call-title">Harold speaking</h2>"#),
        "{html}"
    );

    // Every text value is escaped, in the attributes and in the screen's words.
    let mut markup = view(CallState::Speaking);
    markup.speech = Some(PhoneSpeech {
        id: "p2".into(),
        text: MARKUP.into(),
    });
    markup.transcript = Some(MARKUP.into());
    let html = render(&markup);
    assert!(html.contains(ESCAPED), "{html}");
    assert!(!html.contains("<script>alert"), "{html}");
    assert!(!html.contains("@post('/x')"), "{html}");
    let mut heard = view(CallState::Listening);
    heard.transcript = Some(MARKUP.into());
    let html = render(&heard);
    assert!(html.contains(ESCAPED), "{html}");
    assert!(!html.contains("<script>alert"), "{html}");
}
