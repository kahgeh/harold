//! A push service for tests. It listens on `127.0.0.1`, keeps every request it gets and
//! answers each with the status it is set to, or never answers at all.

use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use jwt_simple::prelude::{ECDSAP256PublicKeyLike, ES256KeyPair};
use tokio::task::JoinHandle;

use super::push::base64url;
use crate::store::AppPushSubscribed;

/// One request the fake push service got.
#[derive(Debug, Clone)]
pub(crate) struct FakePushRequest {
    /// The request's path, such as `/push/1`.
    pub path: String,
    /// The headers, with lowercased names, in the order they came.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl FakePushRequest {
    /// The first value of the header `name` (lowercase).
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header, _)| header == name)
            .map(|(_, value)| value.as_str())
    }
}

struct Shared {
    /// The status every request gets.
    status: AtomicU16,
    /// The length of the body sent with each answer, in bytes.
    response_body_len: AtomicUsize,
    /// When set, a request is read and never answered.
    silent: bool,
    requests: Mutex<Vec<FakePushRequest>>,
}

pub(crate) struct FakePushService {
    /// Where the service listens: `http://127.0.0.1:<port>/push`. Subscription `n` is
    /// `<endpoint>/<n>`.
    pub endpoint: String,
    shared: Arc<Shared>,
    server: JoinHandle<()>,
}

impl FakePushService {
    /// Answers every request with `status`.
    pub(crate) async fn start(status: u16) -> Self {
        Self::start_with(status, false).await
    }

    /// Accepts every request and never answers it.
    pub(crate) async fn start_silent() -> Self {
        Self::start_with(200, true).await
    }

    async fn start_with(status: u16, silent: bool) -> Self {
        let shared = Arc::new(Shared {
            status: AtomicU16::new(status),
            response_body_len: AtomicUsize::new(0),
            silent,
            requests: Mutex::new(Vec::new()),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the fake push service");
        let port = listener.local_addr().expect("fake push address").port();
        let router = Router::new()
            .route("/push/{n}", post(receive))
            .with_state(shared.clone());
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Self {
            endpoint: format!("http://127.0.0.1:{port}/push"),
            shared,
            server,
        }
    }

    /// The status the next requests get.
    pub(crate) fn set_status(&self, status: u16) {
        self.shared.status.store(status, Ordering::SeqCst);
    }

    /// Sends `len` bytes of body with each answer from now on.
    pub(crate) fn set_response_body_len(&self, len: usize) {
        self.shared.response_body_len.store(len, Ordering::SeqCst);
    }

    /// Every request received so far, oldest first.
    pub(crate) fn requests(&self) -> Vec<FakePushRequest> {
        self.shared
            .requests
            .lock()
            .expect("fake push requests")
            .clone()
    }

    /// A subscription to this service at `<endpoint>/<n>`, with keys shaped as a
    /// browser's: an uncompressed P-256 point and a 16-byte auth secret, both base64url.
    pub(crate) fn subscription(&self, n: u32) -> AppPushSubscribed {
        let browser_key = ES256KeyPair::generate();
        let point = browser_key
            .public_key()
            .public_key()
            .to_bytes_uncompressed();
        AppPushSubscribed {
            endpoint: format!("{}/{n}", self.endpoint),
            p256dh: base64url(&point),
            auth: base64url(uuid::Uuid::new_v4().as_bytes()),
        }
    }
}

impl Drop for FakePushService {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn receive(
    State(shared): State<Arc<Shared>>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let headers = headers
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_ascii_lowercase(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect();
    shared
        .requests
        .lock()
        .expect("fake push requests")
        .push(FakePushRequest {
            path: uri.path().to_owned(),
            headers,
            body: body.to_vec(),
        });
    if shared.silent {
        std::future::pending::<()>().await;
    }
    let status = StatusCode::from_u16(shared.status.load(Ordering::SeqCst))
        .expect("a valid fake push status");
    let body = vec![b'x'; shared.response_body_len.load(Ordering::SeqCst)];
    (status, body).into_response()
}
