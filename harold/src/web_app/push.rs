//! Sends the chief's alerts to the phone through its browser's push service.
//!
//! The `web-push` crate encrypts the alert (aes128gcm) and signs a VAPID token with
//! Harold's key, and Harold posts the request itself with a blocking `reqwest` client.
//! The push address and keys come from the browser, so they are treated as untrusted
//! and never logged; nor is the alert's text, nor a `reqwest` error, whose text holds
//! the address.

use std::fs::{DirBuilder, OpenOptions, Permissions};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use jwt_simple::prelude::ES256KeyPair;
use opentelemetry::KeyValue;
use reqwest::blocking::Client;
use reqwest::header::{HeaderName, HeaderValue};
use tracing::{info, warn};
use web_push::{
    ContentEncoding, SubscriptionInfo, VapidSignatureBuilder, WebPushMessageBuilder,
    request_builder,
};

use crate::store::AppPushSubscribed;
use crate::telemetry::instruments;

/// How long a push may take to get its answer's status. Reading the body stops once
/// this much time has passed since the push started; a read already waiting may take
/// one more timeout, so a push takes at most about twice this.
const PUSH_TIMEOUT: Duration = Duration::from_secs(10);

/// The most of a push service's answer that is read. The address comes from the browser,
/// so whatever answers it is not trusted to send a small body.
const RESPONSE_READ_LIMIT: u64 = 64 * 1024;

/// The longest alert, in characters. The whole message waits in the app.
const ALERT_CHARACTERS: usize = 120;

/// Harold's VAPID key: a P-256 private key, as PKCS#8 PEM.
pub(crate) struct VapidKey {
    pem: Vec<u8>,
    /// The public key, an uncompressed P-256 point (65 bytes).
    public_point: Vec<u8>,
}

impl VapidKey {
    /// Reads the key at `path`, or creates it (folder 0700, file 0600) when absent. An
    /// existing key and its folder are set to those modes too, so only the user can
    /// read the key.
    pub(crate) fn load_or_create(path: &Path) -> Result<Self, String> {
        match std::fs::read(path) {
            Ok(pem) => Self::from_pem(pem)
                .and_then(|key| make_private(path).map(|()| key))
                .map_err(|error| format!("read the VAPID key at {}: {error}", path.display())),
            Err(error) if error.kind() == ErrorKind::NotFound => Self::create(path)
                .map_err(|error| format!("create the VAPID key at {}: {error}", path.display())),
            Err(error) => Err(format!(
                "read the VAPID key at {}: {}",
                path.display(),
                error.kind()
            )),
        }
    }

    fn from_pem(pem: Vec<u8>) -> Result<Self, String> {
        let public_point = VapidSignatureBuilder::from_pem_no_sub(pem.as_slice())
            .map_err(|_| "not a P-256 private key in PEM".to_owned())?
            .get_public_key();
        Ok(Self { pem, public_point })
    }

    /// Writes a new key beside `path` and moves it into place, so a crash leaves no
    /// half-written key.
    fn create(path: &Path) -> Result<Self, String> {
        let folder = path
            .parent()
            .filter(|folder| !folder.as_os_str().is_empty())
            .ok_or_else(|| "the key path has no folder".to_owned())?;
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(folder)
            .map_err(|error| format!("make the folder: {}", error.kind()))?;
        std::fs::set_permissions(folder, Permissions::from_mode(0o700))
            .map_err(|error| format!("limit the folder: {}", error.kind()))?;

        let pem = ES256KeyPair::generate()
            .to_pem()
            .map_err(|_| "encode the new key".to_owned())?;
        let key = Self::from_pem(pem.into_bytes())?;

        let partial = folder.join(format!(".vapid-{}.pem", uuid::Uuid::new_v4()));
        let written = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&partial)
            .and_then(|mut file| {
                file.set_permissions(Permissions::from_mode(0o600))?;
                file.write_all(&key.pem)?;
                file.sync_all()
            })
            .and_then(|()| std::fs::rename(&partial, path));
        if let Err(error) = written {
            let _ = std::fs::remove_file(&partial);
            return Err(format!("write the key: {}", error.kind()));
        }
        Ok(key)
    }

    /// The public key, uncompressed P-256 point, base64url without padding: the page's
    /// applicationServerKey.
    pub(crate) fn public_key_base64url(&self) -> String {
        base64url(&self.public_point)
    }
}

/// Sets the key file to 0600 and its folder to 0700.
fn make_private(path: &Path) -> Result<(), String> {
    std::fs::set_permissions(path, Permissions::from_mode(0o600))
        .map_err(|error| format!("limit the key file: {}", error.kind()))?;
    if let Some(folder) = path
        .parent()
        .filter(|folder| !folder.as_os_str().is_empty())
    {
        std::fs::set_permissions(folder, Permissions::from_mode(0o700))
            .map_err(|error| format!("limit the folder: {}", error.kind()))?;
    }
    Ok(())
}

/// Where the VAPID key lives: `web-push/vapid.pem` beside the store's folder, in its
/// parent, which is the installed service's `data` folder.
pub(crate) fn vapid_key_path(store_path: &Path) -> PathBuf {
    store_path
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .join("web-push")
        .join("vapid.pem")
}

/// What a push service did with a push.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PushOutcome {
    /// It took the push (2xx).
    Accepted,
    /// The subscription no longer exists (404 or 410): forget it.
    Gone,
    /// It answered with any other status, or the push could not be built: the
    /// subscription's address or keys are unusable, or the body is over the crate's limit.
    Refused,
    /// It could not be reached, or did not answer in time.
    Unreachable,
}

/// Sends pushes signed with Harold's VAPID key.
pub(crate) struct Pusher {
    key: VapidKey,
    /// The VAPID `sub` claim: `mailto:<allowed_login>`.
    subject: String,
    timeout: Duration,
    /// Made on the first push, which runs on a blocking thread: a blocking client cannot
    /// be made inside the async runtime.
    client: OnceLock<Option<Client>>,
    /// How many bytes of the last answer's body were read.
    #[cfg(test)]
    response_bytes_read: AtomicUsize,
}

impl Pusher {
    pub(crate) fn new(key: VapidKey, allowed_login: &str) -> Self {
        Self {
            key,
            subject: format!("mailto:{allowed_login}"),
            timeout: PUSH_TIMEOUT,
            client: OnceLock::new(),
            #[cfg(test)]
            response_bytes_read: AtomicUsize::new(0),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_timeout(self, timeout: Duration) -> Self {
        Self {
            timeout,
            client: OnceLock::new(),
            ..self
        }
    }

    #[cfg(test)]
    pub(crate) fn response_bytes_read(&self) -> usize {
        self.response_bytes_read.load(Ordering::SeqCst)
    }

    /// Blocking: call from spawn_blocking. Encrypts `body` (aes128gcm), signs with VAPID,
    /// posts it.
    pub(crate) fn push(&self, subscription: &AppPushSubscribed, body: &str) -> PushOutcome {
        let (outcome, reason) = self.send(subscription, body);
        match reason {
            None => info!(outcome = outcome.as_str(), "app push sent"),
            Some(reason) => warn!(
                outcome = outcome.as_str(),
                reason = reason.as_str(),
                "app push refused"
            ),
        }
        instruments()
            .web_app_pushes
            .add(1, &[KeyValue::new("outcome", outcome.as_str())]);
        outcome
    }

    /// The outcome, and for any outcome but `Accepted` the fixed reason to log.
    fn send(&self, subscription: &AppPushSubscribed, body: &str) -> (PushOutcome, Option<String>) {
        let deadline = Instant::now() + self.timeout;
        let Some(request) = self.request(subscription, body) else {
            return (PushOutcome::Refused, Some("build".to_owned()));
        };
        let Some(client) = self.client() else {
            return (PushOutcome::Unreachable, Some("client".to_owned()));
        };
        let mut response = match client.execute(request) {
            Ok(response) => response,
            Err(error) => return (PushOutcome::Unreachable, Some(send_error_reason(&error))),
        };
        let status = response.status().as_u16();
        let read = read_capped(&mut response, deadline);
        self.note_response_read(read);
        match status {
            200..=299 => (PushOutcome::Accepted, None),
            404 | 410 => (PushOutcome::Gone, Some(format!("status_{status}"))),
            _ => (PushOutcome::Refused, Some(format!("status_{status}"))),
        }
    }

    /// The encrypted, signed request for `subscription`, or `None` when its address or
    /// keys cannot be used.
    fn request(
        &self,
        subscription: &AppPushSubscribed,
        body: &str,
    ) -> Option<reqwest::blocking::Request> {
        let info = SubscriptionInfo::new(
            subscription.endpoint.as_str(),
            subscription.p256dh.as_str(),
            subscription.auth.as_str(),
        );
        // The token's audience is the push service's origin, port included; the crate
        // would leave the port out.
        let origin = reqwest::Url::parse(&subscription.endpoint)
            .ok()?
            .origin()
            .ascii_serialization();
        let mut signature = VapidSignatureBuilder::from_pem(self.key.pem.as_slice(), &info).ok()?;
        signature.add_claim("sub", self.subject.as_str());
        signature.add_claim("aud", origin);
        let signature = signature.build().ok()?;

        let mut message = WebPushMessageBuilder::new(&info);
        message.set_payload(ContentEncoding::Aes128Gcm, body.as_bytes());
        message.set_vapid_signature(signature);
        let message = message.build().ok()?;

        // `web-push` builds an `http` 0.2 request and `reqwest` takes `http` 1, so the
        // parts are copied across one by one.
        let built = request_builder::build_request::<Vec<u8>>(message);
        let method = reqwest::Method::from_bytes(built.method().as_str().as_bytes()).ok()?;
        let url = reqwest::Url::parse(&built.uri().to_string()).ok()?;
        let mut request = reqwest::blocking::Request::new(method, url);
        for (name, value) in built.headers() {
            let name = HeaderName::from_bytes(name.as_str().as_bytes()).ok()?;
            let value = HeaderValue::from_bytes(value.as_bytes()).ok()?;
            request.headers_mut().append(name, value);
        }
        *request.body_mut() = Some(built.into_body().into());
        Some(request)
    }

    #[cfg(test)]
    fn note_response_read(&self, read: usize) {
        self.response_bytes_read.store(read, Ordering::SeqCst);
    }

    #[cfg(not(test))]
    fn note_response_read(&self, _read: usize) {}

    fn client(&self) -> Option<&Client> {
        self.client
            .get_or_init(|| {
                Client::builder()
                    .timeout(self.timeout)
                    .redirect(reqwest::redirect::Policy::none())
                    .build()
                    .ok()
            })
            .as_ref()
    }
}

impl PushOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Gone => "gone",
            Self::Refused => "refused",
            Self::Unreachable => "unreachable",
        }
    }
}

/// A fixed word for why a push could not be sent. The error's own text holds the
/// address, so it is never logged.
fn send_error_reason(error: &reqwest::Error) -> String {
    let reason = if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connect"
    } else {
        "send"
    };
    reason.to_owned()
}

/// Reads and drops at most `RESPONSE_READ_LIMIT` bytes of the answer's body, starting
/// no read after `deadline`, and returns how many were read. Nothing is sized from the
/// answer's `Content-Length`.
///
/// The deadline matters because the blocking client gives each read its own fresh
/// timeout: a service that sends a byte now and then would otherwise hold the thread
/// for as long as it liked.
fn read_capped(body: &mut impl Read, deadline: Instant) -> usize {
    let mut limited = body.take(RESPONSE_READ_LIMIT);
    let mut buffer = [0_u8; 8 * 1024];
    let mut read = 0;
    while Instant::now() < deadline {
        match limited.read(&mut buffer) {
            Ok(0) | Err(_) => return read,
            Ok(count) => read += count,
        }
    }
    read
}

/// The first 120 characters of `text`, the most an alert shows.
pub(crate) fn alert_body(text: &str) -> String {
    text.chars().take(ALERT_CHARACTERS).collect()
}

/// `bytes` as base64url without padding (RFC 4648 section 5).
pub(crate) fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut text = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let group = chunk
            .iter()
            .enumerate()
            .fold(0_u32, |group, (index, byte)| {
                group | u32::from(*byte) << (16 - 8 * index)
            });
        for index in 0..=chunk.len() {
            let sextet = (group >> (18 - 6 * index)) & 0x3f;
            text.push(char::from(ALPHABET[sextet as usize]));
        }
    }
    text
}

#[cfg(test)]
#[path = "push_tests.rs"]
mod tests;
