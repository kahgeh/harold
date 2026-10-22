//! The photo routes: the upload, the reply that names it, the row that shows it, and the
//! prune that deletes it.

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::origin::loopback_hosts;
use super::replies::{PHOTO_MAX_BYTES, REPLY_MAX_CHARS};
use super::{AppState, prune, router};
use crate::outbound::fake::CapturedLogs;
use crate::store::{EventVersion, HaroldStore};

const LOGIN: &str = "chief@example.com";
const CANARY: &str = "APP-CANARY-7731";
const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// A JPEG's first bytes, then `len - 4` filler bytes.
fn jpeg(len: usize) -> Vec<u8> {
    let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xE0];
    bytes.resize(len, 7);
    bytes
}

fn png(len: usize) -> Vec<u8> {
    let mut bytes = vec![0x89, 0x50, 0x4E, 0x47];
    bytes.resize(len, 7);
    bytes
}

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("harold-web-photo-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("create test directory");
        Self(path)
    }

    async fn store(&self) -> Arc<HaroldStore> {
        Arc::new(HaroldStore::open(self.0.join("store")).await.unwrap())
    }

    /// Where the photos go: `app-photos/` beside the store's folder.
    fn photos(&self) -> PathBuf {
        self.0.join("app-photos")
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

    /// Posts `bytes` to the photo route with the headers `photo.js` sends.
    async fn upload(&self, bytes: Vec<u8>) -> reqwest::Response {
        reqwest::Client::new()
            .post(self.url("/replies/photo"))
            .header("Tailscale-User-Login", LOGIN)
            .header("sec-fetch-site", "same-origin")
            .header("origin", format!("http://{}", self.addr))
            .header("content-type", "application/octet-stream")
            .body(bytes)
            .send()
            .await
            .unwrap()
    }

    /// Uploads a JPEG and gives the id Harold answered.
    async fn upload_jpeg(&self) -> String {
        let response = self.upload(jpeg(100)).await;
        assert_eq!(response.status(), 200);
        response.text().await.unwrap()
    }

    /// Posts a reply as Datastar's `@post` sends it.
    async fn reply(&self, reply: &str, photo: Option<&str>) -> reqwest::Response {
        let body = match photo {
            Some(photo) => serde_json::json!({ "reply": reply, "photo": photo }),
            None => serde_json::json!({ "reply": reply }),
        };
        reqwest::Client::new()
            .post(self.url("/replies"))
            .header("Tailscale-User-Login", LOGIN)
            .header("sec-fetch-site", "same-origin")
            .header("origin", format!("http://{}", self.addr))
            .header("datastar-request", "true")
            .header("content-type", "application/json")
            .body(body.to_string())
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

fn files_in(folder: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(folder) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

async fn events(store: &HaroldStore) -> Vec<serde_json::Value> {
    store
        .stream()
        .load_after_version(EventVersion::start(), 100)
        .await
        .unwrap()
        .into_iter()
        .map(|event| event.payload)
        .collect()
}

fn now_ms() -> i64 {
    super::route::now_ms()
}

/// Current-thread runtime: the server's handlers run on this thread, so `CapturedLogs`
/// sees their lines.
#[tokio::test(flavor = "current_thread")]
async fn a_jpeg_and_a_png_are_stored_under_app_photos_at_mode_0600_and_the_id_is_answered() {
    let logs = CapturedLogs::start();
    let directory = TestDirectory::new();
    let server = TestServer::serve(directory.store().await).await;

    let jpeg_response = server.upload(jpeg(1500)).await;
    assert_eq!(jpeg_response.status(), 200);
    assert_eq!(jpeg_response.headers()["content-type"], "text/plain");
    let jpeg_id = jpeg_response.text().await.unwrap();
    let png_id = server.upload(png(900)).await.text().await.unwrap();

    for id in [&jpeg_id, &png_id] {
        assert_eq!(id.len(), 36, "{id}");
        assert!(uuid::Uuid::parse_str(id).is_ok(), "{id}");
    }
    let jpeg_path = directory.photos().join(format!("{jpeg_id}.jpg"));
    let png_path = directory.photos().join(format!("{png_id}.png"));
    assert_eq!(std::fs::read(&jpeg_path).unwrap(), jpeg(1500));
    assert_eq!(std::fs::read(&png_path).unwrap(), png(900));
    assert_eq!(mode(&jpeg_path), 0o600);
    assert_eq!(mode(&png_path), 0o600);
    assert_eq!(mode(&directory.photos()), 0o700);
    assert_eq!(files_in(&directory.photos()).len(), 2);

    let captured = logs.text();
    assert!(captured.contains("app photo stored"), "{captured}");
    assert!(captured.contains("photo_bytes=1500"), "{captured}");
    assert!(captured.contains(r#"kind="jpeg""#), "{captured}");
    assert!(captured.contains(r#"kind="png""#), "{captured}");
    assert!(!captured.contains("app photo not shrunk"), "{captured}");
    for named in [jpeg_id.as_str(), png_id.as_str(), "app-photos"] {
        assert!(!captured.contains(named), "a log line names {named}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_4_mb_photo_is_stored_and_logged_as_not_shrunk_and_the_limit_is_5_mb() {
    let logs = CapturedLogs::start();
    let directory = TestDirectory::new();
    let server = TestServer::serve(directory.store().await).await;

    // Past axum's default 2 MB body limit.
    let response = server.upload(jpeg(4 * 1024 * 1024)).await;
    assert_eq!(response.status(), 200);
    let captured = logs.text();
    assert!(captured.contains("app photo not shrunk"), "{captured}");
    assert!(captured.contains("photo_bytes=4194304"), "{captured}");
    assert_eq!(files_in(&directory.photos()).len(), 1);

    // 5 MB itself is taken, one byte more is not.
    assert_eq!(server.upload(jpeg(PHOTO_MAX_BYTES)).await.status(), 200);
    let refused = server.upload(jpeg(PHOTO_MAX_BYTES + 1)).await;
    assert_eq!(refused.status(), 413);
    assert!(refused.bytes().await.unwrap().is_empty());
    let captured = logs.text();
    assert!(captured.contains(r#"reason="too_large""#), "{captured}");
    assert!(captured.contains("photo_bytes=5242881"), "{captured}");
    assert_eq!(files_in(&directory.photos()).len(), 2, "nothing was kept");
}

#[tokio::test(flavor = "current_thread")]
async fn a_gif_text_or_nothing_is_refused_with_400_and_a_log_line() {
    let logs = CapturedLogs::start();
    let directory = TestDirectory::new();
    let server = TestServer::serve(directory.store().await).await;

    for body in [
        b"GIF89a....".to_vec(),
        CANARY.as_bytes().to_vec(),
        Vec::new(),
    ] {
        let response = server.upload(body).await;
        assert_eq!(response.status(), 400);
        assert!(response.bytes().await.unwrap().is_empty());
    }

    let captured = logs.text();
    assert_eq!(
        captured.matches(r#"reason="not_an_image""#).count(),
        3,
        "{captured}"
    );
    assert!(captured.contains("photo_bytes=10"), "{captured}");
    assert!(!captured.contains(CANARY), "{captured}");
    assert!(files_in(&directory.photos()).is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn a_photo_that_cannot_be_written_gets_503_and_a_log_line() {
    let logs = CapturedLogs::start();
    let directory = TestDirectory::new();
    // A file where the folder belongs: the write cannot be made.
    std::fs::write(directory.photos(), b"in the way").unwrap();
    let server = TestServer::serve(directory.store().await).await;

    let response = server.upload(jpeg(100)).await;

    assert_eq!(response.status(), 503);
    let captured = logs.text();
    assert!(captured.contains(r#"reason="store_failed""#), "{captured}");
    assert!(captured.contains("photo_bytes=100"), "{captured}");
    assert!(!captured.contains("app photo stored"), "{captured}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_photo_routes_have_the_login_host_and_origin_checks() {
    let directory = TestDirectory::new();
    let store = directory.store().await;
    let server = TestServer::serve(store.clone()).await;
    let id = server.upload_jpeg().await;
    let client = reqwest::Client::new();
    let stored = files_in(&directory.photos());

    // No login, or another login.
    for login in [None, Some("someone@else")] {
        for (method, path) in [
            ("POST", "/replies/photo".to_owned()),
            ("GET", format!("/replies/photo/{id}")),
        ] {
            let mut request = match method {
                "GET" => client.get(server.url(&path)),
                _ => client
                    .post(server.url(&path))
                    .header("sec-fetch-site", "same-origin")
                    .header("content-type", "application/octet-stream")
                    .body(jpeg(100)),
            };
            if let Some(login) = login {
                request = request.header("Tailscale-User-Login", login);
            }
            let response = request.send().await.unwrap();
            assert_eq!(response.status(), 403, "{method} {path} with {login:?}");
            assert!(response.bytes().await.unwrap().is_empty());
        }
    }

    // A post that is not from the app's own page, or whose Host is not the app's.
    for headers in [
        vec![("content-type", "application/octet-stream")],
        vec![
            ("sec-fetch-site", "cross-site"),
            ("content-type", "application/octet-stream"),
        ],
        vec![("sec-fetch-site", "same-origin")],
        vec![
            ("sec-fetch-site", "same-origin"),
            ("content-type", "image/jpeg"),
        ],
        vec![
            ("host", "evil.example"),
            ("sec-fetch-site", "same-origin"),
            ("content-type", "application/octet-stream"),
        ],
    ] {
        let mut request = client
            .post(server.url("/replies/photo"))
            .header("Tailscale-User-Login", LOGIN)
            .body(jpeg(100));
        for (name, value) in &headers {
            request = request.header(*name, *value);
        }
        assert_eq!(request.send().await.unwrap().status(), 403, "{headers:?}");
    }
    let host = client
        .get(server.url(&format!("/replies/photo/{id}")))
        .header("Tailscale-User-Login", LOGIN)
        .header("host", "evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(host.status(), 403);
    assert_eq!(files_in(&directory.photos()), stored, "nothing was stored");
}

#[tokio::test(flavor = "current_thread")]
async fn a_blank_reply_with_a_fresh_photo_is_stored_with_the_photo_in_its_payload() {
    let logs = CapturedLogs::start();
    let directory = TestDirectory::new();
    let store = directory.store().await;
    let server = TestServer::serve(store.clone()).await;
    let id = server.upload_jpeg().await;

    let response = server.reply("   ", Some(&id)).await;

    assert_eq!(response.status(), 200);
    let body = response.text().await.unwrap();
    assert!(
        body.contains(r#"data: signals {"reply":"","photo":""}"#),
        "{body}"
    );
    let payloads = events(&store).await;
    assert_eq!(
        payloads,
        [serde_json::json!({ "text": "", "to_chief": true, "photo": format!("{id}.jpg") })]
    );
    let captured = logs.text();
    assert!(captured.contains("app reply stored"), "{captured}");
    assert!(captured.contains("chars=0"), "{captured}");
    assert!(captured.contains("photo_bytes=100"), "{captured}");
    assert!(!captured.contains(&id), "{captured}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reply_with_words_and_a_photo_keeps_both() {
    let directory = TestDirectory::new();
    let store = directory.store().await;
    let server = TestServer::serve(store.clone()).await;
    let id = server.upload_jpeg().await;

    assert_eq!(server.reply(CANARY, Some(&id)).await.status(), 200);

    assert_eq!(
        events(&store).await,
        [serde_json::json!({ "text": CANARY, "to_chief": true, "photo": format!("{id}.jpg") })]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_blank_reply_without_a_photo_or_with_an_unknown_or_used_one_gets_400() {
    let logs = CapturedLogs::start();
    let directory = TestDirectory::new();
    let store = directory.store().await;
    let server = TestServer::serve(store.clone()).await;
    let id = server.upload_jpeg().await;

    assert_eq!(server.reply("", None).await.status(), 400);
    assert_eq!(server.reply("  ", Some("")).await.status(), 400);
    let unknown = uuid::Uuid::new_v4().to_string();
    for photo in [
        unknown.as_str(),
        "not-a-uuid",
        "../../etc/passwd",
        &format!("{id}.jpg"),
    ] {
        assert_eq!(
            server.reply(CANARY, Some(photo)).await.status(),
            400,
            "{photo}"
        );
    }
    assert!(events(&store).await.is_empty(), "nothing was stored");

    // The upload is still there for a reply that names it, and only once.
    assert_eq!(server.reply("", Some(&id)).await.status(), 200);
    assert_eq!(server.reply("", Some(&id)).await.status(), 400);
    assert_eq!(server.reply(CANARY, Some(&id)).await.status(), 400);
    assert_eq!(events(&store).await.len(), 1);

    let captured = logs.text();
    assert!(captured.contains(r#"reason="unknown_photo""#), "{captured}");
    assert!(!captured.contains(CANARY), "{captured}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reply_over_the_length_limit_with_a_photo_is_refused_and_keeps_the_upload() {
    let directory = TestDirectory::new();
    let store = directory.store().await;
    let server = TestServer::serve(store.clone()).await;
    let id = server.upload_jpeg().await;

    let long = "x".repeat(REPLY_MAX_CHARS + 1);
    assert_eq!(server.reply(&long, Some(&id)).await.status(), 400);
    assert_eq!(server.reply("fits", Some(&id)).await.status(), 200);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_photo_is_served_with_its_type_and_a_private_day_of_caching() {
    let directory = TestDirectory::new();
    let server = TestServer::serve(directory.store().await).await;
    let jpeg_id = server.upload_jpeg().await;
    let png_id = server.upload(png(300)).await.text().await.unwrap();

    let response = server.get(&format!("/replies/photo/{jpeg_id}")).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "image/jpeg");
    assert_eq!(
        response.headers()["cache-control"],
        "private, max-age=86400"
    );
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    assert_eq!(response.bytes().await.unwrap().to_vec(), jpeg(100));

    let response = server.get(&format!("/replies/photo/{png_id}")).await;
    assert_eq!(response.headers()["content-type"], "image/png");
    assert_eq!(response.bytes().await.unwrap().to_vec(), png(300));

    let unknown = uuid::Uuid::new_v4();
    for path in [
        format!("/replies/photo/{unknown}"),
        "/replies/photo/nope".to_owned(),
        "/replies/photo/..%2F..%2Fetc%2Fpasswd".to_owned(),
        format!("/replies/photo/{jpeg_id}.jpg"),
        format!("/replies/photo/{}", jpeg_id.to_uppercase().replace('-', "")),
    ] {
        assert_eq!(server.get(&path).await.status(), 404, "{path}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_photo_whose_file_is_gone_gets_404() {
    let directory = TestDirectory::new();
    let server = TestServer::serve(directory.store().await).await;
    let id = server.upload_jpeg().await;
    std::fs::remove_file(directory.photos().join(format!("{id}.jpg"))).unwrap();

    assert_eq!(
        server.get(&format!("/replies/photo/{id}")).await.status(),
        404
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_thread_shows_a_photo_above_the_words_and_removed_when_the_file_is_gone() {
    let directory = TestDirectory::new();
    let store = directory.store().await;
    let server = TestServer::serve(store.clone()).await;
    let id = server.upload_jpeg().await;
    assert_eq!(server.reply("Look at this", Some(&id)).await.status(), 200);
    store.project_unhandled_events(10).await.unwrap();

    let page = server.get("/").await.text().await.unwrap();

    let photo = format!(
        r#"<a class="photo" href="/replies/photo/{id}"><img src="/replies/photo/{id}" loading="lazy" alt="Your photo"></a><p class="text">Look at this</p>"#
    );
    assert!(page.contains(&photo), "{page}");
    assert!(!page.contains("photo removed"), "{page}");

    // Once the prune has deleted the file, the row says so.
    store
        .delete_app_photo_upload(&format!("{id}.jpg"))
        .await
        .unwrap();
    let page = server.get("/").await.text().await.unwrap();
    assert!(
        page.contains(
            r#"<p class="photo-removed">photo removed</p><p class="text">Look at this</p>"#
        ),
        "{page}"
    );
    assert!(!page.contains(r#"class="photo""#), "{page}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_row_without_a_photo_has_no_photo_markup() {
    let directory = TestDirectory::new();
    let store = directory.store().await;
    store
        .add_chief_app_message("e1", now_ms(), "hello")
        .await
        .unwrap();
    let server = TestServer::serve(store).await;

    let page = server.get("/").await.text().await.unwrap();

    assert!(
        page.contains(r#"class="message chief"><p class="text">hello"#),
        "{page}"
    );
    assert!(!page.contains("photo removed"), "{page}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_reply_form_has_the_text_box_the_photo_button_and_send_in_that_order() {
    let directory = TestDirectory::new();
    let server = TestServer::serve(directory.store().await).await;

    let page = server.get("/").await.text().await.unwrap();

    let form = &page[page.find(r#"<form id="reply""#).unwrap()..];
    let form = &form[..form.find("</form>").unwrap()];
    let text_box = form.find("data-bind:reply").expect("the text box");
    let photo_button = form
        .find(r#"aria-label="Add a photo""#)
        .expect("the photo button");
    let send = form.find(r#"aria-label="Send""#).expect("Send");
    assert!(text_box < photo_button && photo_button < send, "{form}");
    let send_button = &form[form[..send].rfind("<button").unwrap()..];
    let send_button = &send_button[..send_button.find("</button>").unwrap()];
    assert!(send_button.contains(r#"type="submit""#), "{send_button}");
    assert!(
        send_button.contains(r#"data-attr:disabled="$sending""#),
        "{send_button}"
    );
    assert!(send_button.contains("icon-button"), "{send_button}");
    assert!(
        !send_button.contains(">Send<"),
        "Send shows no word: {send_button}"
    );
    assert!(
        form.contains(r#"<input type="file" id="photo-file" accept="image/*" hidden>"#),
        "{form}"
    );
    assert!(
        form.contains(r#"<input type="hidden" id="photo-id" data-bind:photo>"#),
        "{form}"
    );
    // The chosen strip is above the bar, hidden until a photo is chosen.
    let strip = page
        .find(r#"<div id="photo-chosen" class="photo-chosen""#)
        .unwrap();
    assert!(strip < page.find(r#"<form id="reply""#).unwrap());
    assert!(
        page[strip..]
            .starts_with(r#"<div id="photo-chosen" class="photo-chosen" role="status" hidden>"#)
    );
    assert!(
        page.contains(r#"<script src="/static/photo.js" defer></script>"#),
        "{page}"
    );

    let script = server.get("/static/photo.js").await;
    assert_eq!(script.status(), 200);
    assert_eq!(script.headers()["content-type"], "text/javascript");
}

#[tokio::test(flavor = "current_thread")]
async fn the_prune_deletes_expired_photos_and_unused_uploads_and_nothing_else() {
    let logs = CapturedLogs::start();
    let directory = TestDirectory::new();
    let store = directory.store().await;
    let photos = directory.photos();
    std::fs::create_dir_all(&photos).unwrap();
    let now = now_ms();
    // (file, recorded at, the reply row's time when a reply named it)
    let uploads = [
        ("expired.jpg", now - 31 * DAY_MS, Some(now - 31 * DAY_MS)),
        ("kept.jpg", now - DAY_MS, Some(now - DAY_MS)),
        ("stale.jpg", now - 2 * 60 * 60 * 1000, None),
        ("fresh.jpg", now - 60 * 1000, None),
    ];
    for (file, at_ms, row) in uploads {
        let id = file.trim_end_matches(".jpg");
        std::fs::write(photos.join(file), jpeg(10)).unwrap();
        store.add_app_photo_upload(id, file, at_ms).await.unwrap();
        if let Some(row_ms) = row {
            store.claim_app_photo_upload(id).await.unwrap();
            store
                .add_user_app_row_for_test(&format!("row-{id}"), row_ms, "", Some(file))
                .await
                .unwrap();
        }
    }
    // A file Harold has no record of is not its to delete.
    std::fs::write(photos.join("stray.txt"), b"not a photo").unwrap();

    prune(&store).await;

    assert_eq!(files_in(&photos), ["fresh.jpg", "kept.jpg", "stray.txt"]);
    let rows = store.app_messages_since(0).await.unwrap();
    assert_eq!(rows.len(), 1, "the 31-day row went with its photo");
    assert_eq!(rows[0].photo.as_ref().map(|photo| photo.kept), Some(true));
    let captured = logs.text();
    assert!(captured.contains("app photos pruned"), "{captured}");
    assert!(captured.contains(r#"kind="expired" count=1"#), "{captured}");
    assert!(captured.contains(r#"kind="unused" count=1"#), "{captured}");
    for named in ["expired", "stale", "stray"] {
        assert!(
            !captured.replace("kind=\"expired\"", "").contains(named),
            "a log line names {named}: {captured}"
        );
    }

    // Nothing left to delete: no line.
    let again = CapturedLogs::start();
    prune(&store).await;
    assert!(
        !again.text().contains("app photos pruned"),
        "{}",
        again.text()
    );
    assert_eq!(files_in(&photos), ["fresh.jpg", "kept.jpg", "stray.txt"]);
}
