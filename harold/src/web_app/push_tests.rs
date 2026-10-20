use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::{PushOutcome, Pusher, VapidKey, alert_body, base64url, vapid_key_path};
use crate::outbound::fake::CapturedLogs;
use crate::store::AppPushSubscribed;
use crate::web_app::fake_push::FakePushService;

const LOGIN: &str = "chief@example.com";

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!("harold-push-{}", uuid::Uuid::new_v4())))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A Harold key in a fresh folder, which is removed with the returned guard.
fn fresh_key() -> (VapidKey, TestDirectory) {
    let directory = TestDirectory::new();
    let key = VapidKey::load_or_create(&vapid_key_path(&directory.path().join("harold.db")))
        .expect("create the VAPID key");
    (key, directory)
}

/// Runs a push on a blocking thread, as the delivery loop does.
async fn push(pusher: &Arc<Pusher>, subscription: &AppPushSubscribed, body: &str) -> PushOutcome {
    let pusher = pusher.clone();
    let subscription = subscription.clone();
    let body = body.to_owned();
    tokio::task::spawn_blocking(move || pusher.push(&subscription, &body))
        .await
        .expect("push thread")
}

/// Decodes base64url, with or without padding (tests only).
fn base64url_decode(text: &str) -> Vec<u8> {
    let mut bits: u32 = 0;
    let mut count = 0;
    let mut bytes = Vec::new();
    for character in text.trim_end_matches('=').bytes() {
        let value = match character {
            b'A'..=b'Z' => character - b'A',
            b'a'..=b'z' => character - b'a' + 26,
            b'0'..=b'9' => character - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            other => panic!("not base64url: {other}"),
        };
        bits = (bits << 6) | u32::from(value);
        count += 6;
        if count >= 8 {
            count -= 8;
            bytes.push((bits >> count) as u8);
            bits &= (1 << count) - 1;
        }
    }
    bytes
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after 1970")
        .as_secs()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_push_is_encrypted_and_signed_for_the_push_services_origin() {
    let service = FakePushService::start(201).await;
    let (key, _directory) = fresh_key();
    let public_key = key.public_key_base64url();
    let pusher = Arc::new(Pusher::new(key, LOGIN));
    let subscription = service.subscription(1);
    let before = now_seconds();

    let outcome = push(&pusher, &subscription, "APP-CANARY-7731 from the chief").await;

    assert_eq!(outcome, PushOutcome::Accepted);
    let requests = service.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.path, "/push/1");
    assert_eq!(request.header("content-encoding"), Some("aes128gcm"));
    let lengths: Vec<&(String, String)> = request
        .headers
        .iter()
        .filter(|(name, _)| name == "content-length")
        .collect();
    assert_eq!(lengths.len(), 1, "{lengths:?}");
    assert_eq!(lengths[0].1, request.body.len().to_string());
    assert!(!request.body.is_empty());
    let canary = b"APP-CANARY-7731";
    assert!(
        !request
            .body
            .windows(canary.len())
            .any(|window| window == canary)
    );

    let authorization = request.header("authorization").expect("authorization");
    let signed = authorization
        .strip_prefix("vapid t=")
        .expect("vapid authorization");
    let (token, public) = signed.split_once(", k=").expect("t= and k=");
    assert_eq!(public, public_key);
    let payload = token.split('.').nth(1).expect("JWT payload");
    let claims: serde_json::Value =
        serde_json::from_slice(&base64url_decode(payload)).expect("JWT claims");
    let origin = service.endpoint.trim_end_matches("/push");
    assert!(origin.starts_with("http://127.0.0.1:"));
    assert_eq!(claims["aud"], origin);
    assert_eq!(claims["sub"], format!("mailto:{LOGIN}"));
    let expires = claims["exp"].as_u64().expect("exp");
    assert!(before < expires && expires <= now_seconds() + 86_400);
}

#[tokio::test(flavor = "multi_thread")]
async fn gone_and_refused_and_unreachable_are_told_apart() {
    let service = FakePushService::start(201).await;
    let (key, _directory) = fresh_key();
    let pusher = Arc::new(Pusher::new(key, LOGIN));
    let subscription = service.subscription(1);

    for (status, expected) in [
        (404, PushOutcome::Gone),
        (410, PushOutcome::Gone),
        (400, PushOutcome::Refused),
        (413, PushOutcome::Refused),
        (500, PushOutcome::Refused),
    ] {
        service.set_status(status);
        assert_eq!(
            push(&pusher, &subscription, "hello").await,
            expected,
            "status {status}"
        );
    }

    let closed_port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("address")
        .port();
    let closed = AppPushSubscribed {
        endpoint: format!("http://127.0.0.1:{closed_port}/push/1"),
        ..subscription
    };
    assert_eq!(
        push(&pusher, &closed, "hello").await,
        PushOutcome::Unreachable
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn each_push_outcome_adds_one_to_the_push_counter() {
    let service = FakePushService::start(201).await;
    let (key, _directory) = fresh_key();
    let pusher = Arc::new(Pusher::new(key, LOGIN));
    let subscription = service.subscription(1);
    let closed_port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("address")
        .port();
    let closed = AppPushSubscribed {
        endpoint: format!("http://127.0.0.1:{closed_port}/push/1"),
        ..subscription.clone()
    };
    let counted = |outcome: &str| {
        crate::telemetry::test_support::counter_value(
            "harold.web_app.pushes",
            &[("outcome", outcome)],
        )
        .unwrap_or(0)
    };

    // Other tests push too and the counter is process-wide, so each outcome is checked
    // for a rise of at least 1 by its own push.
    for (status, outcome) in [(201, "accepted"), (410, "gone"), (500, "refused")] {
        service.set_status(status);
        let before = counted(outcome);
        push(&pusher, &subscription, "hello").await;
        assert!(counted(outcome) > before, "{outcome}");
    }
    let before = counted("unreachable");
    push(&pusher, &closed, "hello").await;
    assert!(counted("unreachable") > before, "unreachable");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_push_service_that_does_not_answer_counts_as_unreachable() {
    let service = FakePushService::start_silent().await;
    let (key, _directory) = fresh_key();
    let pusher = Arc::new(Pusher::new(key, LOGIN).with_timeout(Duration::from_millis(200)));
    let subscription = service.subscription(1);
    let started = Instant::now();

    // `CapturedLogs` sees only its own thread, so it starts on the push's thread.
    let (outcome, logged) = tokio::task::spawn_blocking(move || {
        let logs = CapturedLogs::start();
        let outcome = pusher.push(&subscription, "hello");
        (outcome, logs.text())
    })
    .await
    .expect("push thread");

    assert_eq!(outcome, PushOutcome::Unreachable);
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(service.requests().len(), 1);
    assert!(
        logged.contains(r#"outcome="unreachable" reason="timeout""#),
        "{logged}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_slow_push_response_body_is_cut_off_at_the_deadline() {
    let service = FakePushService::start(201).await;
    service.set_slow_body(Duration::from_millis(50), 200);
    let (key, _directory) = fresh_key();
    let pusher = Arc::new(Pusher::new(key, LOGIN).with_timeout(Duration::from_millis(200)));
    let subscription = service.subscription(1);
    let started = Instant::now();

    let outcome = push(&pusher, &subscription, "hello").await;

    assert_eq!(outcome, PushOutcome::Accepted);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "took {:?}",
        started.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_large_push_response_is_read_no_further_than_64_kib() {
    let service = FakePushService::start(201).await;
    service.set_response_body_len(1024 * 1024);
    let (key, _directory) = fresh_key();
    let pusher = Arc::new(Pusher::new(key, LOGIN).with_timeout(Duration::from_secs(5)));
    let subscription = service.subscription(1);
    let started = Instant::now();

    let outcome = push(&pusher, &subscription, "hello").await;

    assert_eq!(outcome, PushOutcome::Accepted);
    assert!(started.elapsed() < Duration::from_secs(2));
    let read = pusher.response_bytes_read();
    assert!(read > 0, "the answer's body was read");
    assert!(read <= 64 * 1024, "read {read} bytes");
}

#[test]
fn the_key_is_created_once_and_kept_private() {
    let directory = TestDirectory::new();
    let path = vapid_key_path(&directory.path().join("data").join("harold.db"));
    assert_eq!(
        path,
        directory
            .path()
            .join("data")
            .join("web-push")
            .join("vapid.pem")
    );

    let first = VapidKey::load_or_create(&path).expect("create the key");

    let file_mode = std::fs::metadata(&path)
        .expect("key file")
        .permissions()
        .mode();
    assert_eq!(file_mode & 0o777, 0o600);
    let folder_mode = std::fs::metadata(path.parent().expect("key folder"))
        .expect("key folder")
        .permissions()
        .mode();
    assert_eq!(folder_mode & 0o777, 0o700);
    let public_key = first.public_key_base64url();
    assert_eq!(base64url_decode(&public_key).len(), 65);
    assert_eq!(base64url_decode(&public_key)[0], 0x04);

    let second = VapidKey::load_or_create(&path).expect("load the key");
    assert_eq!(second.public_key_base64url(), public_key);
}

#[test]
fn an_existing_key_is_made_private_on_load() {
    let directory = TestDirectory::new();
    let path = vapid_key_path(&directory.path().join("data").join("harold.db"));
    let folder = path.parent().expect("key folder").to_path_buf();
    let created = VapidKey::load_or_create(&path).expect("create the key");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod key");
    std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o755))
        .expect("chmod folder");

    let loaded = VapidKey::load_or_create(&path).expect("load the key");

    assert_eq!(
        loaded.public_key_base64url(),
        created.public_key_base64url()
    );
    let file_mode = std::fs::metadata(&path)
        .expect("key file")
        .permissions()
        .mode();
    assert_eq!(file_mode & 0o777, 0o600);
    let folder_mode = std::fs::metadata(&folder)
        .expect("key folder")
        .permissions()
        .mode();
    assert_eq!(folder_mode & 0o777, 0o700);
}

#[test]
fn the_alert_body_is_the_first_120_characters() {
    let long = "é".repeat(200);
    assert_eq!(alert_body(&long), "é".repeat(120));
    assert_eq!(alert_body("short text"), "short text");
}

#[test]
fn base64url_has_no_padding_and_url_safe_characters() {
    assert_eq!(base64url(b""), "");
    assert_eq!(base64url(b"f"), "Zg");
    assert_eq!(base64url(b"fo"), "Zm8");
    assert_eq!(base64url(b"foo"), "Zm9v");
    assert_eq!(base64url(&[0xfb, 0xff, 0xbf]), "-_-_");
    let bytes: Vec<u8> = (0..=255).collect();
    assert_eq!(base64url_decode(&base64url(&bytes)), bytes);
}

#[test]
fn push_lines_carry_no_address_or_key() {
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let service = runtime.block_on(FakePushService::start(201));
    let (key, _directory) = fresh_key();
    let public_key = key.public_key_base64url();
    let pusher = Pusher::new(key, LOGIN);
    let subscription = service.subscription(1);
    let logs = CapturedLogs::start();

    assert_eq!(
        pusher.push(&subscription, "APP-CANARY-7731"),
        PushOutcome::Accepted
    );
    service.set_status(500);
    assert_eq!(
        pusher.push(&subscription, "APP-CANARY-7731"),
        PushOutcome::Refused
    );
    let closed_port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("address")
        .port();
    let closed = AppPushSubscribed {
        endpoint: format!("http://127.0.0.1:{closed_port}/push/1"),
        ..subscription.clone()
    };
    assert_eq!(
        pusher.push(&closed, "APP-CANARY-7731"),
        PushOutcome::Unreachable
    );

    let text = logs.text();
    assert!(text.contains("app push sent"), "{text}");
    assert!(text.contains("app push refused"), "{text}");
    assert!(text.contains(r#"reason="status_500""#), "{text}");
    assert!(text.contains(r#"reason="connect""#), "{text}");
    for secret in [
        subscription.endpoint.as_str(),
        "127.0.0.1",
        subscription.p256dh.as_str(),
        subscription.auth.as_str(),
        public_key.as_str(),
        "APP-CANARY-7731",
    ] {
        assert!(!text.contains(secret), "{secret} in {text}");
    }
}
