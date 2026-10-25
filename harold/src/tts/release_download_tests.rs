//! The release download against a fake HTTP server on the loopback address.

use super::*;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Clone)]
enum Reply {
    Body(Vec<u8>),
    Redirect(String),
    Status(u16),
}

#[derive(Debug, Clone)]
struct Seen {
    path: String,
    headers: Vec<(String, String)>,
}

struct Fake {
    port: u16,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Fake {
    async fn start(routes: HashMap<String, Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let routes = routes.clone();
                let log = Arc::clone(&log);
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut buffer = [0u8; 1024];
                    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                        let Ok(read) = stream.read(&mut buffer).await else {
                            return;
                        };
                        if read == 0 {
                            return;
                        }
                        request.extend_from_slice(&buffer[..read]);
                    }
                    let text = String::from_utf8_lossy(&request).to_string();
                    let mut lines = text.lines();
                    let path = lines
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .unwrap_or_default()
                        .to_string();
                    let headers = lines
                        .take_while(|line| !line.is_empty())
                        .filter_map(|line| line.split_once(':'))
                        .map(|(name, value)| (name.trim().to_lowercase(), value.trim().to_string()))
                        .collect();
                    log.lock().unwrap().push(Seen {
                        path: path.clone(),
                        headers,
                    });
                    let reply = routes.get(&path).cloned().unwrap_or(Reply::Status(404));
                    let (head, body) = match reply {
                        Reply::Body(body) => (
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            ),
                            body,
                        ),
                        Reply::Redirect(to) => (
                            format!(
                                "HTTP/1.1 302 Found\r\nLocation: {to}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            ),
                            Vec::new(),
                        ),
                        Reply::Status(code) => (
                            format!(
                                "HTTP/1.1 {code} Nope\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            ),
                            Vec::new(),
                        ),
                    };
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.write_all(&body).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        Self { port, seen }
    }

    fn paths(&self) -> Vec<String> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .map(|s| s.path.clone())
            .collect()
    }
}

fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Two small files with their pins, and the routes that serve them from `/release`.
fn files(model: &[u8], voices: &[u8]) -> Vec<ReleaseFile> {
    let leak = |text: String| -> &'static str { Box::leak(text.into_boxed_str()) };
    vec![
        ReleaseFile {
            name: "kokoro-v1.0.onnx",
            size: model.len() as u64,
            sha256: leak(digest(model)),
        },
        ReleaseFile {
            name: "voices-v1.0.bin",
            size: voices.len() as u64,
            sha256: leak(digest(voices)),
        },
    ]
}

fn scratch() -> (PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!("harold-release-{}", uuid::Uuid::new_v4()));
    (root.join("model"), root.join("record.json"))
}

fn direct_routes(model: &[u8], voices: &[u8]) -> HashMap<String, Reply> {
    HashMap::from([
        (
            "/release/kokoro-v1.0.onnx".to_string(),
            Reply::Body(model.to_vec()),
        ),
        (
            "/release/voices-v1.0.bin".to_string(),
            Reply::Body(voices.to_vec()),
        ),
    ])
}

#[tokio::test]
async fn it_asks_only_for_the_two_fixed_urls_and_sends_no_token_or_cookie() {
    let (model, voices) = (b"model bytes".to_vec(), b"voice bytes!".to_vec());
    let fake = Fake::start(direct_routes(&model, &voices)).await;
    let downloader = ReleaseDownloader::for_test(
        format!("http://127.0.0.1:{}/release", fake.port),
        files(&model, &voices),
        vec!["127.0.0.1".into()],
    );
    let (folder, record) = scratch();

    let bytes = downloader
        .download_all(&folder, &record, |_| {})
        .await
        .unwrap();

    assert_eq!(bytes, (model.len() + voices.len()) as u64);
    assert_eq!(
        fake.paths(),
        ["/release/kokoro-v1.0.onnx", "/release/voices-v1.0.bin"]
    );
    for seen in fake.seen.lock().unwrap().iter() {
        for (name, _) in &seen.headers {
            assert!(
                !["authorization", "cookie", "proxy-authorization", "referer"]
                    .contains(&name.as_str()),
                "sent {name}"
            );
        }
    }
    assert_eq!(
        std::fs::read(folder.join("kokoro-v1.0.onnx")).unwrap(),
        model
    );
    assert_eq!(
        std::fs::read(folder.join("voices-v1.0.bin")).unwrap(),
        voices
    );
    assert!(!folder.join("kokoro-v1.0.onnx.part").exists());
    let record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&record).unwrap()).unwrap();
    assert_eq!(record["model"], MODEL_ID);
    assert_eq!(record["commit"], RELEASE_TAG);
    assert_eq!(record["files"][0]["sha256"], digest(&model));
    assert_eq!(record["files"][1]["size"], voices.len());
}

#[tokio::test]
async fn a_file_with_the_wrong_sha256_is_deleted_and_no_record_is_made() {
    let (model, voices) = (b"model bytes".to_vec(), b"voice bytes!".to_vec());
    let mut routes = direct_routes(&model, &voices);
    // Same length, other bytes.
    routes.insert(
        "/release/kokoro-v1.0.onnx".into(),
        Reply::Body(b"MODEL bytes".to_vec()),
    );
    let fake = Fake::start(routes).await;
    let downloader = ReleaseDownloader::for_test(
        format!("http://127.0.0.1:{}/release", fake.port),
        files(&model, &voices),
        vec!["127.0.0.1".into()],
    );
    let (folder, record) = scratch();

    let result = downloader.download_all(&folder, &record, |_| {}).await;

    assert_eq!(result, Err("check_failed"));
    assert!(!folder.join("kokoro-v1.0.onnx").exists());
    assert!(!folder.join("kokoro-v1.0.onnx.part").exists());
    assert!(!record.exists());
    // The second file is not asked for after the first fails.
    assert_eq!(fake.paths(), ["/release/kokoro-v1.0.onnx"]);
}

#[tokio::test]
async fn a_body_longer_than_the_pinned_size_is_refused() {
    let (model, voices) = (b"model bytes".to_vec(), b"voice bytes!".to_vec());
    let mut routes = direct_routes(&model, &voices);
    routes.insert(
        "/release/kokoro-v1.0.onnx".into(),
        Reply::Body(b"model bytes and more".to_vec()),
    );
    let fake = Fake::start(routes).await;
    let downloader = ReleaseDownloader::for_test(
        format!("http://127.0.0.1:{}/release", fake.port),
        files(&model, &voices),
        vec!["127.0.0.1".into()],
    );
    let (folder, record) = scratch();

    let result = downloader.download_all(&folder, &record, |_| {}).await;

    assert_eq!(result, Err("check_failed"));
    assert!(!folder.join("kokoro-v1.0.onnx").exists());
    assert!(!folder.join("kokoro-v1.0.onnx.part").exists());
}

#[tokio::test]
async fn a_redirect_to_another_host_is_refused_and_not_followed() {
    let (model, voices) = (b"model bytes".to_vec(), b"voice bytes!".to_vec());
    let elsewhere = Fake::start(direct_routes(&model, &voices)).await;
    let mut routes = HashMap::new();
    // `localhost` is not `127.0.0.1`: another host.
    routes.insert(
        "/release/kokoro-v1.0.onnx".to_string(),
        Reply::Redirect(format!(
            "http://localhost:{}/release/kokoro-v1.0.onnx",
            elsewhere.port
        )),
    );
    let fake = Fake::start(routes).await;
    let downloader = ReleaseDownloader::for_test(
        format!("http://127.0.0.1:{}/release", fake.port),
        files(&model, &voices),
        vec!["127.0.0.1".into()],
    );
    let (folder, record) = scratch();

    let result = downloader.download_all(&folder, &record, |_| {}).await;

    assert_eq!(result, Err("check_failed"));
    assert!(elsewhere.paths().is_empty(), "the redirect was followed");
    assert!(!folder.join("kokoro-v1.0.onnx").exists());
}

#[tokio::test]
async fn a_redirect_to_the_release_host_is_followed() {
    let (model, voices) = (b"model bytes".to_vec(), b"voice bytes!".to_vec());
    let storage = Fake::start(HashMap::from([
        ("/blob/model".to_string(), Reply::Body(model.clone())),
        ("/blob/voices".to_string(), Reply::Body(voices.clone())),
    ]))
    .await;
    let fake = Fake::start(HashMap::from([
        (
            "/release/kokoro-v1.0.onnx".to_string(),
            Reply::Redirect(format!("http://127.0.0.1:{}/blob/model", storage.port)),
        ),
        (
            "/release/voices-v1.0.bin".to_string(),
            Reply::Redirect(format!("http://127.0.0.1:{}/blob/voices", storage.port)),
        ),
    ]))
    .await;
    // The release is asked for as `localhost`, the redirect leads to the allowed `127.0.0.1`.
    let downloader = ReleaseDownloader::for_test(
        format!("http://localhost:{}/release", fake.port),
        files(&model, &voices),
        vec!["127.0.0.1".into()],
    );
    let (folder, record) = scratch();

    let result = downloader.download_all(&folder, &record, |_| {}).await;

    assert!(result.is_ok(), "{result:?}");
    assert_eq!(
        std::fs::read(folder.join("voices-v1.0.bin")).unwrap(),
        voices
    );
    assert!(
        storage
            .seen
            .lock()
            .unwrap()
            .iter()
            .all(|seen| !seen.headers.iter().any(|(name, _)| name == "authorization"))
    );
}

#[tokio::test]
async fn a_missing_file_is_a_network_failure_and_nothing_is_kept() {
    let fake = Fake::start(HashMap::new()).await;
    let downloader = ReleaseDownloader::for_test(
        format!("http://127.0.0.1:{}/release", fake.port),
        files(b"a", b"b"),
        vec!["127.0.0.1".into()],
    );
    let (folder, record) = scratch();

    let result = downloader.download_all(&folder, &record, |_| {}).await;

    assert_eq!(result, Err("network"));
    assert!(!record.exists());
}

#[tokio::test]
async fn an_unreachable_server_is_a_network_failure() {
    let downloader = ReleaseDownloader::for_test(
        "http://127.0.0.1:1/release".into(),
        files(b"a", b"b"),
        vec!["127.0.0.1".into()],
    );
    let (folder, record) = scratch();

    assert_eq!(
        downloader.download_all(&folder, &record, |_| {}).await,
        Err("network")
    );
}

#[tokio::test]
async fn pins_that_are_not_recorded_refuse_the_download_before_it_asks_for_anything() {
    let fake = Fake::start(HashMap::new()).await;
    let downloader = ReleaseDownloader::for_test(
        format!("http://127.0.0.1:{}/release", fake.port),
        FILES.to_vec(),
        vec!["127.0.0.1".into()],
    );
    let (folder, record) = scratch();

    let result = downloader.download_all(&folder, &record, |_| {}).await;

    assert_eq!(result, Err("check_failed"));
    assert!(fake.paths().is_empty());
}

#[test]
fn the_production_download_asks_for_the_two_fixed_urls_of_the_pinned_release() {
    assert_eq!(
        RELEASE_BASE,
        "https://github.com/thewh1teagle/kokoro-onnx/releases/download/model-files-v1.0"
    );
    assert_eq!(MODEL_FILE, "kokoro-v1.0.onnx");
    assert_eq!(VOICES_FILE, "voices-v1.0.bin");
    assert_eq!(total_size(), 325_532_387 + 28_214_398);
    let production = ReleaseDownloader::production();
    assert_eq!(production.base, RELEASE_BASE);
    assert_eq!(production.files, FILES.to_vec());
}

#[tokio::test]
async fn progress_counts_the_bytes_of_the_run() {
    let (model, voices) = (b"model bytes".to_vec(), b"voice bytes!".to_vec());
    let fake = Fake::start(direct_routes(&model, &voices)).await;
    let downloader = ReleaseDownloader::for_test(
        format!("http://127.0.0.1:{}/release", fake.port),
        files(&model, &voices),
        vec!["127.0.0.1".into()],
    );
    let (folder, record) = scratch();
    let last = std::sync::atomic::AtomicU64::new(0);

    downloader
        .download_all(&folder, &record, |bytes| {
            last.store(bytes, std::sync::atomic::Ordering::SeqCst);
        })
        .await
        .unwrap();

    assert_eq!(
        last.load(std::sync::atomic::Ordering::SeqCst),
        (model.len() + voices.len()) as u64
    );
}
