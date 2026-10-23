use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::origin::loopback_hosts;
use super::pair::{Attempt, NotSetUp, Pairing, devices_path};
use super::{AppState, WebAppIdentity, router};
use crate::outbound::fake::CapturedLogs;
use crate::store::HaroldStore;

const LOGIN: &str = "chief@example.com";
/// The pairing code of the tests. It must appear nowhere in a log, a page or a command's text.
const CODE: &str = "Test-Code-7731";
const OTHER_CODE: &str = "Another9Code";

struct Folder(PathBuf);

impl Folder {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("harold-pair-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn store_path(&self) -> PathBuf {
        self.0.join("store")
    }

    fn code_file(&self) -> PathBuf {
        self.0.join("pairing.env")
    }

    fn lock_file(&self) -> PathBuf {
        devices_path(&self.store_path()).with_file_name("pairing-lock.json")
    }

    /// Writes the pairing file at mode 0600.
    fn write_code(&self, text: &str) {
        std::fs::write(self.code_file(), text).unwrap();
        std::fs::set_permissions(self.code_file(), std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn set_code(&self, code: &str) {
        self.write_code(&format!("HAROLD_PAIRING_CODE={code}\n"));
    }
}

impl Drop for Folder {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A clock the test moves.
#[derive(Clone)]
struct TestClock(Arc<Mutex<Instant>>);

impl TestClock {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(Instant::now())))
    }

    fn advance(&self, by: Duration) {
        *self.0.lock().unwrap() += by;
    }

    fn clock(&self) -> super::Clock {
        let now = Arc::clone(&self.0);
        Arc::new(move || *now.lock().unwrap())
    }
}

fn pairing(folder: &Folder, clock: &TestClock) -> Pairing {
    Pairing::load(
        devices_path(&folder.store_path()),
        folder.code_file(),
        true,
        clock.clock(),
    )
    .unwrap()
}

struct Server {
    addr: SocketAddr,
    pairing: Arc<Pairing>,
    handle: JoinHandle<()>,
    _shutdown: watch::Sender<()>,
}

impl Server {
    async fn start(folder: &Folder, pairing: Pairing) -> Self {
        let store = Arc::new(HaroldStore::open(folder.store_path()).await.unwrap());
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
        )
        .with_pairing(pairing);
        let pairing = Arc::clone(&state.pairing);
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, router(state)).await;
        });
        Self {
            addr,
            pairing,
            handle,
            _shutdown: shutdown,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    /// A request as the app's own page makes it: the login, same-origin and a JSON type.
    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        reqwest::Client::new()
            .request(method, self.url(path))
            .header("Tailscale-User-Login", LOGIN)
            .header("Origin", format!("http://{}", self.addr))
            .header("Content-Type", "application/json")
    }

    fn get(&self, path: &str) -> reqwest::RequestBuilder {
        self.request(reqwest::Method::GET, path)
    }

    async fn pair_with(&self, code: &str) -> reqwest::Response {
        self.request(reqwest::Method::POST, "/pair")
            .header("User-Agent", "Mozilla/5.0 (iPhone; CPU iPhone OS 18_0)")
            .body(serde_json::json!({ "code": code }).to_string())
            .send()
            .await
            .unwrap()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

fn cookie_of(response: &reqwest::Response) -> String {
    response
        .headers()
        .get("set-cookie")
        .expect("a cookie")
        .to_str()
        .unwrap()
        .to_owned()
}

/// The `harold_device=<secret>` part of a `Set-Cookie`.
fn pair_of(cookie: &str) -> String {
    cookie.split(';').next().unwrap().to_owned()
}

#[tokio::test(flavor = "current_thread")]
async fn an_unpaired_page_request_gets_the_pair_screen_and_an_api_request_401() {
    let folder = Folder::new();
    folder.set_code(CODE);
    let server = Server::start(&folder, pairing(&folder, &TestClock::new())).await;

    let page = server.get("/").send().await.unwrap();
    assert_eq!(page.status(), 200);
    let html = page.text().await.unwrap();
    assert!(html.contains("Pair this phone"));
    assert!(
        !html.contains("id=\"messages\""),
        "no thread on the pair screen"
    );

    for (method, path) in [
        (reqwest::Method::GET, "/updates"),
        (reqwest::Method::GET, "/replies/photo/abc"),
        (reqwest::Method::GET, "/thread/older"),
        (reqwest::Method::GET, "/telemetry/traces"),
        (reqwest::Method::POST, "/replies"),
        (reqwest::Method::POST, "/call"),
        (reqwest::Method::GET, "/nope"),
        (reqwest::Method::POST, "/"),
    ] {
        let response = server.request(method.clone(), path).send().await.unwrap();
        assert_eq!(response.status(), 401, "{method} {path}");
        assert!(response.bytes().await.unwrap().is_empty());
    }
    // A cookie that names no device is refused the same way.
    let response = server
        .get("/updates")
        .header("Cookie", "harold_device=deadbeef")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
}

#[tokio::test(flavor = "current_thread")]
async fn the_right_code_sets_the_cookie_and_the_cookie_opens_the_app() {
    let folder = Folder::new();
    folder.set_code(CODE);
    let server = Server::start(&folder, pairing(&folder, &TestClock::new())).await;

    let response = server.pair_with(CODE).await;
    assert_eq!(response.status(), 204);
    let cookie = cookie_of(&response);
    assert!(cookie.starts_with("harold_device="));
    for attribute in [
        "HttpOnly",
        "Secure",
        "SameSite=Lax",
        "Path=/",
        "Max-Age=34560000",
    ] {
        assert!(cookie.contains(attribute), "{attribute} in {cookie}");
    }
    let secret = pair_of(&cookie);
    assert_eq!(secret.len(), "harold_device=".len() + 64, "32 random bytes");

    let page = server
        .get("/")
        .header("Cookie", &secret)
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
    assert!(page.text().await.unwrap().contains("id=\"messages\""));
    let routed = server
        .get("/thread/older")
        .header("Cookie", format!("theme=dark; {secret}; other=1"))
        .send()
        .await
        .unwrap();
    assert_eq!(routed.status(), 200);

    let devices = server.pairing.devices();
    assert_eq!(devices.len(), 1);
    assert!(
        devices[0].name.starts_with("iPhone, paired "),
        "{}",
        devices[0].name
    );
}

#[tokio::test(flavor = "current_thread")]
async fn devices_json_holds_fingerprints_only_at_mode_0600() {
    let folder = Folder::new();
    folder.set_code(CODE);
    let server = Server::start(&folder, pairing(&folder, &TestClock::new())).await;
    let secret = pair_of(&cookie_of(&server.pair_with(CODE).await))
        .strip_prefix("harold_device=")
        .unwrap()
        .to_owned();

    let path = devices_path(&folder.store_path());
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(!text.contains(&secret), "the secret is not in the file");
    assert!(!text.contains(CODE));
    let fingerprint = hex::encode(sha2::Sha256::digest(secret.as_bytes()));
    assert!(text.contains(&fingerprint));
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let parent = path.parent().unwrap();
    assert_eq!(
        std::fs::metadata(parent).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(folder.lock_file())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(
        !std::fs::read_to_string(folder.lock_file())
            .unwrap()
            .contains(CODE)
    );
}

use sha2::Digest;

#[tokio::test(flavor = "current_thread")]
async fn the_paired_list_survives_a_restart_and_a_removed_device_is_refused() {
    let folder = Folder::new();
    folder.set_code(CODE);
    let clock = TestClock::new();
    let server = Server::start(&folder, pairing(&folder, &clock)).await;
    let secret = pair_of(&cookie_of(&server.pair_with(CODE).await));
    drop(server);

    let server = Server::start(&folder, pairing(&folder, &clock)).await;
    let name = server.pairing.devices()[0].name.clone();
    let kept = server
        .get("/thread/older")
        .header("Cookie", &secret)
        .send()
        .await
        .unwrap();
    assert_eq!(kept.status(), 200, "still paired after a restart");

    assert_eq!(server.pairing.unpair("nobody"), Ok(false));
    assert_eq!(server.pairing.unpair(&name), Ok(true));
    let refused = server
        .get("/thread/older")
        .header("Cookie", &secret)
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), 401, "the next request after unpairing");
    let page = server
        .get("/")
        .header("Cookie", &secret)
        .send()
        .await
        .unwrap();
    assert!(page.text().await.unwrap().contains("Pair this phone"));
}

#[tokio::test(flavor = "current_thread")]
async fn the_code_matches_ignoring_spaces_hyphens_and_case() {
    let folder = Folder::new();
    folder.set_code("Ab-12 Cd34");
    let pairing = pairing(&folder, &TestClock::new());
    for typed in ["ab12cd34", "AB-12-CD-34", " ab 12 cd 34 ", "aB12Cd34"] {
        assert!(
            matches!(pairing.attempt(typed, "d"), Attempt::Paired(_)),
            "{typed}"
        );
    }
    assert_eq!(pairing.attempt("ab12cd35", "d"), Attempt::Wrong);
    assert_eq!(pairing.attempt("", "d"), Attempt::Wrong);
    assert_eq!(pairing.attempt("ab12cd3", "d"), Attempt::Wrong);
    // Four devices of one name get numbered names.
    let names: Vec<String> = pairing.devices().into_iter().map(|d| d.name).collect();
    assert_eq!(names.len(), 4);
    assert!(names.contains(&"d".to_owned()) && names.contains(&"d 2".to_owned()));
}

#[tokio::test(flavor = "current_thread")]
async fn five_wrong_tries_lock_pairing_for_good_even_for_the_right_code() {
    let logs = CapturedLogs::start();
    let folder = Folder::new();
    folder.set_code(CODE);
    let server = Server::start(&folder, pairing(&folder, &TestClock::new())).await;

    for _ in 0..4 {
        let response = server.pair_with("wrong-guess").await;
        assert_eq!(response.status(), 400);
        assert_eq!(response.text().await.unwrap(), "That code didn't work");
    }
    assert_eq!(
        server.pair_with("wrong-guess").await.status(),
        400,
        "the fifth"
    );
    let locked = server.pair_with(CODE).await;
    assert_eq!(locked.status(), 423);
    assert_eq!(locked.text().await.unwrap(), "Pairing is locked on the Mac");
    assert!(server.pairing.devices().is_empty());

    let text = logs.text();
    assert_eq!(text.matches("app pairing code failed").count(), 5, "{text}");
    assert_eq!(text.matches("app pairing locked").count(), 1, "{text}");
    assert!(text.contains("tries=5"));
    assert!(
        !text.contains(CODE) && !text.contains("wrong-guess"),
        "{text}"
    );

    // The lock survives a restart.
    drop(server);
    let server = Server::start(&folder, pairing(&folder, &TestClock::new())).await;
    assert_eq!(server.pair_with(CODE).await.status(), 423);
}

#[tokio::test(flavor = "multi_thread")]
async fn ten_posts_at_once_get_at_most_five_tries() {
    let folder = Folder::new();
    folder.set_code(CODE);
    let server = Arc::new(Server::start(&folder, pairing(&folder, &TestClock::new())).await);
    let mut posts = Vec::new();
    for _ in 0..10 {
        let server = Arc::clone(&server);
        posts.push(tokio::spawn(async move {
            server.pair_with("not-the-code").await.status().as_u16()
        }));
    }
    let mut statuses = Vec::new();
    for post in posts {
        statuses.push(post.await.unwrap());
    }
    assert_eq!(
        statuses.iter().filter(|s| **s == 400).count(),
        5,
        "{statuses:?}"
    );
    assert_eq!(
        statuses.iter().filter(|s| **s == 423).count(),
        5,
        "{statuses:?}"
    );
    assert_eq!(server.pair_with(CODE).await.status(), 423);
}

#[tokio::test(flavor = "current_thread")]
async fn changing_the_code_or_unlocking_clears_the_lock_and_a_pairing_resets_the_count() {
    let folder = Folder::new();
    folder.set_code(CODE);
    let pairing = pairing(&folder, &TestClock::new());
    for _ in 0..5 {
        pairing.attempt("nope-nope", "d");
    }
    assert_eq!(pairing.attempt(CODE, "d"), Attempt::Locked);
    assert!(pairing.status().locked);
    assert_eq!(pairing.status().tries, 5);

    // A new code in the file starts the count again at once.
    folder.set_code(OTHER_CODE);
    assert!(!pairing.status().locked);
    assert_eq!(pairing.status().tries, 0);
    assert!(matches!(
        pairing.attempt(OTHER_CODE, "d"),
        Attempt::Paired(_)
    ));

    // `--unlock` clears it for the same code.
    for _ in 0..5 {
        pairing.attempt("nope-nope", "d");
    }
    assert_eq!(pairing.attempt(OTHER_CODE, "d"), Attempt::Locked);
    super::pair::unlock(&folder.store_path()).unwrap();
    assert!(!pairing.status().locked);
    assert_eq!(pairing.status().tries, 0);
    assert!(matches!(
        pairing.attempt(OTHER_CODE, "d"),
        Attempt::Paired(_)
    ));

    // A success sets the count back to 0.
    for _ in 0..4 {
        pairing.attempt("nope-nope", "d");
    }
    assert_eq!(pairing.status().tries, 4);
    assert!(matches!(
        pairing.attempt(OTHER_CODE, "d"),
        Attempt::Paired(_)
    ));
    assert_eq!(pairing.status().tries, 0);
}

#[tokio::test(flavor = "current_thread")]
async fn a_pairing_file_that_cannot_be_used_refuses_with_503_and_one_warning() {
    let logs = CapturedLogs::start();
    let folder = Folder::new();
    let server = Server::start(&folder, pairing(&folder, &TestClock::new())).await;

    // No file.
    for _ in 0..3 {
        let response = server.pair_with(CODE).await;
        assert_eq!(response.status(), 503);
        assert_eq!(
            response.text().await.unwrap(),
            "Pairing isn't set up on the Mac"
        );
    }
    assert_eq!(logs.text().matches("app pairing not set up").count(), 1);
    assert!(logs.text().contains("reason=\"missing\"") || logs.text().contains("reason=missing"));

    // No code line, an empty one, a short one, and a file others can read.
    folder.write_code("# nothing here\nOTHER=1\n");
    assert_eq!(server.pair_with(CODE).await.status(), 503);
    assert_eq!(server.pairing.status().not_set_up, Some(NotSetUp::NoCode));
    folder.write_code("HAROLD_PAIRING_CODE=\n");
    assert_eq!(server.pairing.status().not_set_up, Some(NotSetUp::NoCode));
    folder.write_code("HAROLD_PAIRING_CODE=ab-c1\n");
    assert_eq!(server.pair_with("ab-c1").await.status(), 503);
    assert_eq!(server.pairing.status().not_set_up, Some(NotSetUp::TooShort));
    folder.set_code(CODE);
    std::fs::set_permissions(folder.code_file(), std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(server.pair_with(CODE).await.status(), 503);
    assert_eq!(server.pairing.status().not_set_up, Some(NotSetUp::Mode));
    assert!(server.pairing.devices().is_empty());

    let text = logs.text();
    for reason in ["missing", "no_code", "too_short", "mode"] {
        assert_eq!(
            text.matches(&format!("reason=\"{reason}\""))
                .count()
                .max(text.matches(&format!("reason={reason}")).count()),
            1,
            "{reason}: {text}"
        );
    }
    assert!(!text.contains(CODE) && !text.contains("ab-c1"), "{text}");

    // A fixed file works at once, with no restart.
    std::fs::set_permissions(folder.code_file(), std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(server.pair_with(CODE).await.status(), 204);
}

#[tokio::test(flavor = "current_thread")]
async fn a_changed_code_works_at_once_and_no_log_page_or_status_holds_a_code() {
    let logs = CapturedLogs::start();
    let folder = Folder::new();
    folder.set_code(CODE);
    let server = Server::start(&folder, pairing(&folder, &TestClock::new())).await;
    assert_eq!(server.pair_with("Different-Code").await.status(), 400);
    folder.set_code(OTHER_CODE);
    assert_eq!(
        server.pair_with(CODE).await.status(),
        400,
        "the old code is gone"
    );
    let ok = server.pair_with(OTHER_CODE).await;
    assert_eq!(ok.status(), 204);

    let page = server.get("/").send().await.unwrap().text().await.unwrap();
    let status = format!("{:?}", server.pairing.status());
    let devices = format!("{:?}", server.pairing.devices());
    let all = format!("{}{page}{status}{devices}", logs.text());
    for code in [CODE, OTHER_CODE, "Different-Code"] {
        assert!(!all.contains(code), "{code}");
    }
    for part in ["Test-Code", "7731", "Another9"] {
        assert!(!all.contains(part), "{part}");
    }
    assert!(
        !std::fs::read_to_string(folder.lock_file())
            .unwrap()
            .contains("7731")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_body_that_is_not_the_code_json_is_refused_and_not_counted() {
    let folder = Folder::new();
    folder.set_code(CODE);
    let server = Server::start(&folder, pairing(&folder, &TestClock::new())).await;
    for body in ["", "not json", "{}", "{\"code\": 5}"] {
        let response = server
            .request(reqwest::Method::POST, "/pair")
            .body(body.to_owned())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400, "{body}");
    }
    assert_eq!(server.pairing.status().tries, 0);
    // A post from another site is kept out by the origin check, as for every route.
    let cross = reqwest::Client::new()
        .post(server.url("/pair"))
        .header("Tailscale-User-Login", LOGIN)
        .header("Origin", "http://evil.example")
        .header("Content-Type", "application/json")
        .body(format!("{{\"code\":\"{CODE}\"}}"))
        .send()
        .await
        .unwrap();
    assert_eq!(cross.status(), 403);
    assert_eq!(server.pair_with(CODE).await.status(), 204);
}

#[tokio::test(flavor = "current_thread")]
async fn last_seen_is_written_at_most_once_an_hour() {
    let folder = Folder::new();
    folder.set_code(CODE);
    let clock = TestClock::new();
    let server = Server::start(&folder, pairing(&folder, &clock)).await;
    let secret = pair_of(&cookie_of(&server.pair_with(CODE).await));
    let path = devices_path(&folder.store_path());
    let written = || std::fs::read_to_string(&path).unwrap();
    let paired = written();

    for _ in 0..5 {
        clock.advance(Duration::from_secs(60));
        let response = server
            .get("/thread/older")
            .header("Cookie", &secret)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
    }
    assert_eq!(
        written(),
        paired,
        "steady requests write nothing within the hour"
    );
    let in_memory = server.pairing.devices()[0].last_seen_ms;

    clock.advance(Duration::from_secs(60 * 60));
    server
        .get("/thread/older")
        .header("Cookie", &secret)
        .send()
        .await
        .unwrap();
    let after = written();
    assert_ne!(after, paired, "an hour later the list is written");
    assert!(after.contains(&format!(
        "\"last_seen_ms\": {}",
        server.pairing.devices()[0].last_seen_ms
    )));
    assert!(server.pairing.devices()[0].last_seen_ms >= in_memory);
    // And the next request within the hour does not write again.
    clock.advance(Duration::from_secs(60));
    server
        .get("/thread/older")
        .header("Cookie", &secret)
        .send()
        .await
        .unwrap();
    assert_eq!(written(), after);
}

#[tokio::test(flavor = "current_thread")]
async fn only_the_exact_open_paths_pass_without_a_cookie() {
    let folder = Folder::new();
    folder.set_code(CODE);
    let server = Server::start(&folder, pairing(&folder, &TestClock::new())).await;
    let open = [
        "/static/app.css",
        "/static/datastar-1.0.2.js",
        "/static/alerts.js",
        "/static/call.js",
        "/static/call-worklet.js",
        "/static/dictation.js",
        "/static/echo-test.js",
        "/static/pages.js",
        "/static/photo.js",
        "/static/pair.js",
        "/static/sw.js",
        "/static/manifest.webmanifest",
    ];
    for path in open {
        assert_eq!(
            server.get(path).send().await.unwrap().status(),
            200,
            "{path}"
        );
    }
    // The icon is open too; with no icon set the file itself is a 404, not a 401.
    assert_eq!(
        server
            .get("/static/icon.png")
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    for path in [
        "/static/telemetry.js",
        "/static/speech-check.js",
        "/speech-check",
        "/static/otel-2.12.0-8513c042.js",
        "/static/nope.js",
        "/static/app.css/",
        "/static//app.css",
        "/replies/photo/1",
        "/updates",
        "/todos/answer",
        "/echo-test",
        "/call/answer/1",
        "/push/subscription",
        "/page/seen",
    ] {
        let response = server.get(path).send().await.unwrap();
        assert_eq!(response.status(), 401, "{path}");
    }
    // The open list is for reading: a post to it is no different from any other.
    let post = server
        .request(reqwest::Method::POST, "/static/app.css")
        .send()
        .await
        .unwrap();
    assert_ne!(post.status(), 500);
}

#[tokio::test(flavor = "current_thread")]
async fn a_refusal_is_logged_once_per_reason_per_hour_without_the_cookie() {
    let logs = CapturedLogs::start();
    let folder = Folder::new();
    folder.set_code(CODE);
    let clock = TestClock::new();
    let server = Server::start(&folder, pairing(&folder, &clock)).await;
    for _ in 0..3 {
        server.get("/updates").send().await.unwrap();
        server
            .get("/updates")
            .header("Cookie", "harold_device=secretvalue123")
            .send()
            .await
            .unwrap();
    }
    let text = logs.text();
    assert_eq!(text.matches("app device refused").count(), 2, "{text}");
    assert!(text.contains("no_cookie") && text.contains("unknown_device"));
    assert!(!text.contains("secretvalue123"));
    clock.advance(Duration::from_secs(60 * 60));
    server.get("/updates").send().await.unwrap();
    assert_eq!(logs.text().matches("app device refused").count(), 3);
}

#[tokio::test(flavor = "current_thread")]
async fn require_pairing_false_passes_everything_and_warns_at_start() {
    let logs = CapturedLogs::start();
    let folder = Folder::new();
    let off = Pairing::load(
        devices_path(&folder.store_path()),
        folder.code_file(),
        false,
        TestClock::new().clock(),
    )
    .unwrap();
    super::pair::warn_when_off(&off);
    assert_eq!(logs.text().matches("web app allows any device").count(), 1);
    let server = Server::start(&folder, off).await;
    for path in ["/", "/updates", "/thread/older"] {
        assert_eq!(
            server.get(path).send().await.unwrap().status(),
            200,
            "{path}"
        );
    }
    assert_eq!(server.pair_with(CODE).await.status(), 404);

    let on = pairing(&folder, &TestClock::new());
    let before = logs.text().matches("web app allows any device").count();
    super::pair::warn_when_off(&on);
    assert_eq!(
        logs.text().matches("web app allows any device").count(),
        before
    );
}

#[test]
fn a_devices_file_that_cannot_be_read_stops_the_start_rather_than_pairing_nobody() {
    let folder = Folder::new();
    let path = devices_path(&folder.store_path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "{ not json").unwrap();
    let error = Pairing::load(
        path.clone(),
        folder.code_file(),
        true,
        TestClock::new().clock(),
    )
    .err()
    .expect("an error");
    assert!(error.contains("devices.json"));
    assert!(!error.contains("not json"));
}

#[test]
fn the_app_commands_report_the_status_without_the_code() {
    let folder = Folder::new();
    folder.set_code(CODE);
    let pairing = Arc::new(pairing(&folder, &TestClock::new()));
    pairing.attempt("wrong-one", "d");
    let status = pairing.status();
    assert_eq!(
        (status.not_set_up, status.locked, status.tries),
        (None, false, 1)
    );
    assert!(!format!("{status:?}").contains("7731"));
    let _: &Path = &folder.0;
}

/// `harold app` against the real gRPC service, on a free local port.
#[tokio::test(flavor = "multi_thread")]
async fn harold_app_lists_unpairs_and_unlocks_through_the_service_without_the_code() {
    use crate::cli::{AppCommand, app};
    let folder = Folder::new();
    folder.set_code(CODE);
    let pairing = Arc::new(pairing(&folder, &TestClock::new()));
    let port = {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        probe.local_addr().unwrap().port()
    };
    let service = super::pair::AppService {
        pairing: Some(Arc::clone(&pairing)),
    };
    let server = tokio::spawn(async move {
        let _ = tonic::transport::Server::builder()
            .add_service(harold_api::harold::harold_app_server::HaroldAppServer::new(
                service,
            ))
            .serve(format!("127.0.0.1:{port}").parse().unwrap())
            .await;
    });
    let address = format!("127.0.0.1:{port}");
    let utc = time::UtcOffset::UTC;
    // The server needs a moment to listen.
    let mut status = Err(String::new());
    for _ in 0..50 {
        status = app(&address, &AppCommand::Pair { unlock: false }, utc).await;
        if status.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let status = status.unwrap();
    assert!(status.contains("pairing is set up"), "{status}");
    assert!(status.contains("locked: no") && status.contains("wrong tries: 0 of 5"));

    assert_eq!(
        app(&address, &AppCommand::Devices, utc).await.unwrap(),
        "no paired devices"
    );
    assert!(matches!(
        pairing.attempt(CODE, "iPhone, paired 23 Oct"),
        Attempt::Paired(_)
    ));
    let listed = app(&address, &AppCommand::Devices, utc).await.unwrap();
    assert!(
        listed.starts_with("iPhone, paired 23 Oct (paired 20"),
        "{listed}"
    );

    for _ in 0..5 {
        pairing.attempt("wrong-code", "d");
    }
    let locked = app(&address, &AppCommand::Pair { unlock: false }, utc)
        .await
        .unwrap();
    assert!(locked.contains("locked: yes") && locked.contains("wrong tries: 5 of 5"));
    super::pair::unlock(&folder.store_path()).unwrap();
    let unlocked = app(&address, &AppCommand::Pair { unlock: true }, utc)
        .await
        .unwrap();
    assert!(
        unlocked.contains("unlocked") && unlocked.contains("locked: no"),
        "{unlocked}"
    );

    let missing = app(
        &address,
        &AppCommand::Unpair {
            name: "nobody".into(),
        },
        utc,
    )
    .await
    .unwrap_err();
    assert_eq!(missing, "no device is called nobody");
    let gone = app(
        &address,
        &AppCommand::Unpair {
            name: "iPhone, paired 23 Oct".into(),
        },
        utc,
    )
    .await
    .unwrap();
    assert_eq!(gone, "unpaired iPhone, paired 23 Oct");
    assert!(pairing.devices().is_empty());

    // A file that cannot be used is reported by reason, never by its text.
    folder.write_code("HAROLD_PAIRING_CODE=abc\n");
    let short = app(&address, &AppCommand::Pair { unlock: false }, utc)
        .await
        .unwrap();
    assert!(
        short.contains("pairing is not set up: the pairing code has fewer than 6"),
        "{short}"
    );
    assert!(!short.contains("abc") && !short.contains("7731"));
    for text in [&status, &listed, &locked, &unlocked, &short] {
        assert!(!text.contains(CODE));
    }
    server.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn the_app_commands_are_refused_when_the_web_app_is_not_running() {
    use harold_api::harold::harold_app_server::HaroldApp;
    let service = super::pair::AppService { pairing: None };
    let error = service
        .list_devices(tonic::Request::new(
            harold_api::harold::ListDevicesRequest {},
        ))
        .await
        .unwrap_err();
    assert_eq!(error.code(), tonic::Code::FailedPrecondition);
}
