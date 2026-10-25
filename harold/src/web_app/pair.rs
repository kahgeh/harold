//! The device check: only a paired device may use the app. A paired device holds a random
//! secret as the cookie `harold_device`; Harold keeps only the secret's SHA-256 in
//! `web-app/devices.json`, so reading Harold's files does not give a secret away.
//!
//! The check runs right after the login check. A request without a known cookie gets `401`,
//! and a page request gets the pair screen instead. A few files that hold no private data
//! are served without it (`OPEN_PATHS`).
//!
//! A device is paired by typing the pairing code into the pair screen. The code is a line in
//! a file outside the repository, read at each attempt (`read_code`). Five wrong tries in
//! all lock pairing for good, until the code in the file changes or `--unlock`. The code, and
//! every line of the file, stay out of logs, pages and the command line's output.

use std::collections::HashMap;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use askama::Template;
use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Response};
use datastar::prelude::{PatchElements, PatchSignals};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use super::route::now_ms;
use super::{AppState, Clock};

/// The cookie a paired device keeps its secret in.
pub(crate) const COOKIE: &str = "harold_device";

/// 400 days, the longest a browser keeps a cookie.
const COOKIE_MAX_AGE: u64 = 34_560_000;

/// How many random bytes a device secret holds.
const SECRET_BYTES: usize = 32;

/// How often at most "last seen" is written to the file.
const WRITE_EVERY: Duration = Duration::from_secs(60 * 60);

/// How often at most a refusal of one reason is logged.
const LOG_EVERY: Duration = Duration::from_secs(60 * 60);

/// Served without the cookie, by exact path: the app's styles and scripts, the service
/// worker, the manifest (the phone fetches it without cookies) and the icon. They hold no
/// private data. Everything else needs the cookie.
const OPEN_PATHS: [&str; 14] = [
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
    "/static/icon.png",
    // The pair post carries no cookie yet; it has its own checks.
    "/pair",
];

/// One paired device, as `devices.json` holds it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Device {
    pub name: String,
    /// The hex SHA-256 of the cookie's value.
    pub fingerprint: String,
    pub paired_at_ms: i64,
    pub last_seen_ms: i64,
}

#[derive(Default, Serialize, Deserialize)]
struct File {
    devices: Vec<Device>,
}

struct Inner {
    devices: Vec<Device>,
    /// Whether `last_seen_ms` changed since the file was written.
    dirty: bool,
    /// When the file was last written.
    written: Instant,
    /// When each refusal reason was last logged.
    logged: HashMap<&'static str, Instant>,
}

/// The paired devices, and the switch for the check.
pub(crate) struct Pairing {
    /// Whether the check is on: `[web_app] require_pairing`.
    require: bool,
    path: PathBuf,
    /// The pairing file: `HAROLD_PAIRING_CODE=<code>`.
    code_file: PathBuf,
    /// The wrong tries and the lock: `web-app/pairing-lock.json`.
    lock_path: PathBuf,
    clock: Clock,
    state: Mutex<Inner>,
    /// Held across a whole attempt, so tries are counted one at a time.
    attempts: Mutex<Attempts>,
}

#[derive(Default)]
struct Attempts {
    /// The reason last logged as `app pairing not set up`, so one WARN covers a lasting fault.
    warned: Option<&'static str>,
}

/// Why pairing is refused with `503`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NotSetUp {
    /// The file is missing or cannot be read.
    Missing,
    /// The file has no `HAROLD_PAIRING_CODE` line, or it is empty.
    NoCode,
    /// The code has fewer than 6 characters.
    TooShort,
    /// Anyone but the owner can read the file: its mode is not 0600.
    Mode,
}

impl NotSetUp {
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::NoCode => "no_code",
            Self::TooShort => "too_short",
            Self::Mode => "mode",
        }
    }
}

/// How a pairing attempt went.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Attempt {
    /// Paired; the cookie's value.
    Paired(String),
    Wrong,
    Locked,
    NotSetUp(NotSetUp),
    /// The lock or the list could not be read or written; nothing is paired.
    Failed,
}

/// What `harold app pair` reports. Never the code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PairingStatus {
    pub not_set_up: Option<NotSetUp>,
    pub locked: bool,
    pub tries: u32,
}

/// Wrong tries in all, then the lock.
pub(crate) const MAX_TRIES: u32 = 5;
/// The fewest characters a code has.
const MIN_CODE: usize = 6;
const CODE_KEY: &str = "HAROLD_PAIRING_CODE";

/// The lock file.
#[derive(Serialize, Deserialize)]
struct Lock {
    /// Random hex, hashed with the code.
    salt: String,
    /// The hex SHA-256 of the salt and the normalised code: which code the count is for.
    fingerprint: String,
    tries: u32,
    locked: bool,
}

/// Where the list lives: `web-app/devices.json` beside the store's folder.
pub(crate) fn devices_path(store_path: &Path) -> PathBuf {
    store_path
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .join("web-app")
        .join("devices.json")
}

/// The hex SHA-256 of a cookie value.
fn fingerprint(secret: &str) -> String {
    hex::encode(Sha256::digest(secret.as_bytes()))
}

impl Pairing {
    /// Reads the list at `path`; no file is an empty list. Err names the path, never its
    /// contents: a list that cannot be read must not become an empty one.
    pub(crate) fn load(
        path: PathBuf,
        code_file: PathBuf,
        require: bool,
        clock: Clock,
    ) -> Result<Self, String> {
        let devices = match std::fs::read(&path) {
            Ok(bytes) => {
                serde_json::from_slice::<File>(&bytes)
                    .map_err(|_| {
                        format!("web-app/devices.json is not readable: {}", path.display())
                    })?
                    .devices
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(_) => {
                return Err(format!(
                    "web-app/devices.json cannot be read: {}",
                    path.display()
                ));
            }
        };
        let written = clock();
        let lock_path = path.with_file_name("pairing-lock.json");
        Ok(Self {
            require,
            path,
            code_file,
            lock_path,
            attempts: Mutex::default(),
            clock,
            state: Mutex::new(Inner {
                devices,
                dirty: false,
                written,
                logged: HashMap::new(),
            }),
        })
    }

    /// A pairing with the check off, for tests that are not about it.
    #[cfg(test)]
    pub(crate) fn off() -> Self {
        Self {
            require: false,
            path: std::env::temp_dir().join("harold-devices-unused.json"),
            code_file: PathBuf::from("/nonexistent/pairing.env"),
            lock_path: std::env::temp_dir().join("harold-lock-unused.json"),
            attempts: Mutex::default(),
            clock: std::sync::Arc::new(Instant::now),
            state: Mutex::new(Inner {
                devices: Vec::new(),
                dirty: false,
                written: Instant::now(),
                logged: HashMap::new(),
            }),
        }
    }

    pub(crate) fn required(&self) -> bool {
        self.require
    }

    fn state(&self) -> MutexGuard<'_, Inner> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The paired devices, oldest first.
    pub(crate) fn devices(&self) -> Vec<Device> {
        self.state().devices.clone()
    }

    /// Pairs a device called `name` (made unique with a number when taken) and returns its
    /// secret, the cookie's value. Err when the list cannot be written; nothing is paired.
    pub(crate) fn add_device(&self, name: &str) -> Result<String, String> {
        let mut bytes = [0u8; SECRET_BYTES];
        openssl::rand::rand_bytes(&mut bytes).map_err(|_| "no random bytes".to_owned())?;
        let secret = hex::encode(bytes);
        let now = now_ms();
        let mut state = self.state();
        let mut unique = name.to_owned();
        let mut number = 1;
        while state.devices.iter().any(|device| device.name == unique) {
            number += 1;
            unique = format!("{name} {number}");
        }
        let mut devices = state.devices.clone();
        devices.push(Device {
            name: unique.clone(),
            fingerprint: fingerprint(&secret),
            paired_at_ms: now,
            last_seen_ms: now,
        });
        write_list(&self.path, &devices)?;
        state.devices = devices;
        state.dirty = false;
        state.written = (self.clock)();
        info!(name = %unique, "app device paired");
        Ok(secret)
    }

    /// Removes the device called `name`. False when there is none. Err when the list
    /// cannot be written; the device then stays.
    pub(crate) fn unpair(&self, name: &str) -> Result<bool, String> {
        let mut state = self.state();
        let Some(at) = state.devices.iter().position(|device| device.name == name) else {
            return Ok(false);
        };
        let mut devices = state.devices.clone();
        devices.remove(at);
        write_list(&self.path, &devices)?;
        state.devices = devices;
        state.dirty = false;
        state.written = (self.clock)();
        info!(name, "app device unpaired");
        Ok(true)
    }

    /// Tries `typed` against the pairing file's code, one attempt at a time. A right code
    /// pairs a device called `name` and sets the count back to 0; a wrong one counts, and
    /// the fifth in all locks pairing. A code changed in the file starts the count again.
    pub(crate) fn attempt(&self, typed: &str, name: &str) -> Attempt {
        let mut attempts = self.attempts.lock().unwrap_or_else(PoisonError::into_inner);
        let code = match read_code(&self.code_file) {
            Ok(code) => {
                attempts.warned = None;
                code
            }
            Err(reason) => {
                if attempts.warned != Some(reason.reason()) {
                    attempts.warned = Some(reason.reason());
                    warn!(reason = reason.reason(), "app pairing not set up");
                }
                return Attempt::NotSetUp(reason);
            }
        };
        let Ok(mut lock) = self.lock_for(&code) else {
            return Attempt::Failed;
        };
        if lock.locked {
            // A code changed in the file was already taken into account by `lock_for`.
            return Attempt::Locked;
        }
        if codes_match(&normalise(typed), &code) {
            lock.tries = 0;
            if write_json(&self.lock_path, &lock).is_err() {
                return Attempt::Failed;
            }
            return match self.add_device(name) {
                Ok(secret) => Attempt::Paired(secret),
                Err(_) => Attempt::Failed,
            };
        }
        lock.tries += 1;
        lock.locked = lock.tries >= MAX_TRIES;
        if write_json(&self.lock_path, &lock).is_err() {
            return Attempt::Failed;
        }
        warn!(tries = lock.tries, "app pairing code failed");
        if lock.locked {
            warn!("app pairing locked");
        }
        Attempt::Wrong
    }

    /// The lock for `code`: the file's, or a fresh one when there is none or the file's is
    /// for another code. Err when the file cannot be read.
    fn lock_for(&self, code: &str) -> Result<Lock, ()> {
        let stored = match std::fs::read(&self.lock_path) {
            Ok(bytes) => Some(serde_json::from_slice::<Lock>(&bytes).map_err(|_| ())?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => return Err(()),
        };
        let salt = match &stored {
            Some(lock) => lock.salt.clone(),
            None => {
                let mut bytes = [0u8; 16];
                openssl::rand::rand_bytes(&mut bytes).map_err(|_| ())?;
                hex::encode(bytes)
            }
        };
        let fingerprint = hex::encode(Sha256::digest(format!("{salt}:{code}").as_bytes()));
        match stored {
            Some(lock) if lock.fingerprint == fingerprint => Ok(lock),
            _ => Ok(Lock {
                salt,
                fingerprint,
                tries: 0,
                locked: false,
            }),
        }
    }

    /// What `harold app pair` reports.
    pub(crate) fn status(&self) -> PairingStatus {
        let _attempts = self.attempts.lock().unwrap_or_else(PoisonError::into_inner);
        let code = match read_code(&self.code_file) {
            Ok(code) => code,
            Err(reason) => {
                return PairingStatus {
                    not_set_up: Some(reason),
                    locked: false,
                    tries: 0,
                };
            }
        };
        let lock = self.lock_for(&code);
        PairingStatus {
            not_set_up: None,
            locked: lock.as_ref().is_ok_and(|lock| lock.locked),
            tries: lock.map_or(0, |lock| lock.tries),
        }
    }

    /// Whether `secret` is a paired device's. Notes the time it was seen, and writes the
    /// list at most once an hour for it.
    fn admit(&self, secret: &str) -> bool {
        let wanted = fingerprint(secret);
        let mut state = self.state();
        let Some(device) = state
            .devices
            .iter_mut()
            .find(|device| device.fingerprint == wanted)
        else {
            return false;
        };
        device.last_seen_ms = now_ms();
        state.dirty = true;
        let now = (self.clock)();
        if now.saturating_duration_since(state.written) >= WRITE_EVERY {
            // A failed write is tried again with the next request.
            if write_list(&self.path, &state.devices).is_ok() {
                state.dirty = false;
                state.written = now;
            }
        }
        true
    }

    /// Whether a refusal for `reason` is logged now: once per reason per hour.
    fn should_log(&self, reason: &'static str) -> bool {
        let now = (self.clock)();
        let mut state = self.state();
        match state.logged.get(reason) {
            Some(last) if now.saturating_duration_since(*last) < LOG_EVERY => false,
            _ => {
                state.logged.insert(reason, now);
                true
            }
        }
    }
}

fn write_list(path: &Path, devices: &[Device]) -> Result<(), String> {
    write_json(
        path,
        &File {
            devices: devices.to_vec(),
        },
    )
}

/// Writes `value` atomically: a temporary file at mode 0600 in a folder at mode 0700, then
/// a rename over `path`. The error names the path, never the contents.
fn write_json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    let failed = |_| format!("{} cannot be written", path.display());
    let folder = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(folder).map_err(failed)?;
    std::fs::set_permissions(folder, std::fs::Permissions::from_mode(0o700)).map_err(failed)?;
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|_| format!("{} cannot be made", path.display()))?;
    let temporary = path.with_extension("json.tmp");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temporary)
        .map_err(failed)?;
    file.write_all(&bytes).map_err(failed)?;
    file.sync_all().map_err(failed)?;
    std::fs::rename(&temporary, path).map_err(failed)
}

/// The cookie `Set-Cookie` sends for a device secret.
pub(crate) fn set_cookie_value(secret: &str) -> String {
    format!("{COOKIE}={secret}; HttpOnly; Secure; SameSite=Lax; Path=/; Max-Age={COOKIE_MAX_AGE}")
}

/// The value of the `harold_device` cookie among the request's `Cookie` headers.
fn device_cookie(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|line| line.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find_map(|(name, value)| (name == COOKIE).then_some(value))
}

/// The pair screen: one field and a Pair button.
#[derive(Template)]
#[template(path = "web_app/pair.html")]
pub(crate) struct PairPage {
    /// The app's name: the title and the heading.
    pub app_name: String,
    /// Whether the page links the app's icon.
    pub has_icon: bool,
}

fn pair_screen(state: &AppState) -> Response {
    let page = PairPage {
        app_name: state.identity.name.clone(),
        has_icon: state.identity.icon.is_some(),
    };
    match page.render() {
        Ok(html) => Html(html).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// Passes the request on when the check is off, the path is an open one, or the cookie is
/// a paired device's. Otherwise `GET /` gets the pair screen and anything else `401`.
/// Logs `app device refused` with its reason, once per reason per hour; never a cookie.
pub(crate) async fn require_device(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let pairing = &state.pairing;
    if !pairing.required() {
        return next.run(request).await;
    }
    let path = request.uri().path();
    let page = path == "/" && matches!(*request.method(), Method::GET | Method::HEAD);
    if OPEN_PATHS.contains(&path) {
        return next.run(request).await;
    }
    let reason = match device_cookie(request.headers()) {
        Some(secret) if pairing.admit(secret) => return next.run(request).await,
        Some(_) => "unknown_device",
        None => "no_cookie",
    };
    if pairing.should_log(reason) {
        info!(reason, "app device refused");
    }
    if page {
        return pair_screen(&state);
    }
    StatusCode::UNAUTHORIZED.into_response()
}

/// The code as compared: without spaces and hyphens, in lowercase.
fn normalise(code: &str) -> String {
    code.chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .flat_map(char::to_lowercase)
        .collect()
}

/// Whether two normalised codes are equal, in time that does not depend on where they
/// differ or how long they are: their digests are compared.
fn codes_match(typed: &str, code: &str) -> bool {
    let typed = Sha256::digest(typed.as_bytes());
    let code = Sha256::digest(code.as_bytes());
    openssl::memcmp::eq(&typed, &code)
}

/// The normalised code in the pairing file at `path`, read now. Err says why pairing is not
/// set up; it never holds the file's text.
fn read_code(path: &Path) -> Result<String, NotSetUp> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::metadata(path).map_err(|_| NotSetUp::Missing)?;
    if !metadata.is_file() {
        return Err(NotSetUp::Missing);
    }
    if metadata.mode() & 0o777 != 0o600 {
        return Err(NotSetUp::Mode);
    }
    let text = std::fs::read_to_string(path).map_err(|_| NotSetUp::Missing)?;
    let value = text.lines().find_map(|line| {
        let line = line.trim();
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        line.strip_prefix(CODE_KEY)?.trim_start().strip_prefix('=')
    });
    let code = normalise(value.unwrap_or_default().trim().trim_matches(['"', '\'']));
    if code.is_empty() {
        return Err(NotSetUp::NoCode);
    }
    if code.chars().count() < MIN_CODE {
        return Err(NotSetUp::TooShort);
    }
    Ok(code)
}

/// A name for the device that sent the request, such as `iPhone, paired 23 Oct`.
fn device_name(headers: &HeaderMap) -> String {
    let agent = headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let kind = ["iPhone", "iPad", "Android", "Macintosh", "Windows", "Linux"]
        .into_iter()
        .find(|kind| agent.contains(kind))
        .map_or(
            "Browser",
            |kind| if kind == "Macintosh" { "Mac" } else { kind },
        );
    let today = time::OffsetDateTime::now_utc().to_offset(crate::telemetry::local_offset());
    let month = today.month().to_string();
    format!(
        "{kind}, paired {} {}",
        today.day(),
        month.get(..3).unwrap_or(&month)
    )
}

/// The body of `POST /pair`.
#[derive(Deserialize)]
struct PairBody {
    code: String,
}

/// The longest body `POST /pair` reads.
const PAIR_BODY_MAX: usize = 1024;

/// `POST /pair` with the page's `code` signal, `{"code": "..."}`. Answers `200` as Datastar
/// events: the cookie, with `{"paired": true}` for a code that paired; otherwise the page's
/// `#pair-notice` patched to "That code didn't work", "Pairing is locked on the Mac" or
/// "Pairing isn't set up on the Mac". A body that is not that JSON gets `400` and is not
/// counted as a try.
pub(crate) async fn post_pair(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !state.pairing.required() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Ok(body) = serde_json::from_slice::<PairBody>(&body[..body.len().min(PAIR_BODY_MAX + 1)])
    else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if body.code.len() > PAIR_BODY_MAX {
        return StatusCode::BAD_REQUEST.into_response();
    }
    match state.pairing.attempt(&body.code, &device_name(&headers)) {
        Attempt::Paired(secret) => (
            [(header::SET_COOKIE, set_cookie_value(&secret))],
            super::datastar_events([PatchSignals::new(r#"{"paired":true}"#)]),
        )
            .into_response(),
        Attempt::Wrong => notice(WRONG),
        Attempt::Locked => notice(LOCKED),
        Attempt::NotSetUp(_) => notice(NOT_SET_UP),
        Attempt::Failed => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// The pair screen's notice line, patched to show `text`.
fn notice(text: &str) -> Response {
    let line = format!(
        r#"<p id="pair-notice" class="pair-notice" role="alert">{}</p>"#,
        super::escape_html(text)
    );
    super::datastar_events([PatchElements::new(line)])
}

/// What the pair screen says for each refusal.
pub(crate) const WRONG: &str = "That code didn't work";
pub(crate) const LOCKED: &str = "Pairing is locked on the Mac";
pub(crate) const NOT_SET_UP: &str = "Pairing isn't set up on the Mac";

/// Clears the pairing lock and the count of the Harold whose store is at `store_path`, by
/// removing `web-app/pairing-lock.json`; the next attempt starts a fresh count. The command
/// does this itself, as the user, in the data folder: no call to the daemon can reset it.
pub(crate) fn unlock(store_path: &Path) -> Result<(), String> {
    let path = devices_path(store_path).with_file_name("pairing-lock.json");
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(format!("{} cannot be removed", path.display())),
    }
}

/// The WARN at start-up when the check is off.
pub(crate) fn warn_when_off(pairing: &Pairing) {
    if !pairing.required() {
        warn!("web app allows any device");
    }
}

/// The gRPC service behind `harold app`: the paired devices and the pairing lock.
pub(crate) struct AppService {
    /// None when Harold runs without the web app.
    pub pairing: Option<std::sync::Arc<Pairing>>,
}

impl AppService {
    fn pairing(&self) -> Result<&Pairing, tonic::Status> {
        self.pairing
            .as_deref()
            .ok_or_else(|| tonic::Status::failed_precondition(NO_WEB_APP))
    }
}

/// The status message when Harold runs without the web app.
pub(crate) const NO_WEB_APP: &str = "the web app is not running";
/// The status message for a name no device has.
pub(crate) const NO_SUCH_DEVICE: &str = "no such device";

#[tonic::async_trait]
impl harold_api::harold::harold_app_server::HaroldApp for AppService {
    async fn pairing_status(
        &self,
        request: tonic::Request<harold_api::harold::PairingStatusRequest>,
    ) -> Result<tonic::Response<harold_api::harold::PairingStatusResponse>, tonic::Status> {
        let _ = request;
        let status = self.pairing()?.status();
        Ok(tonic::Response::new(
            harold_api::harold::PairingStatusResponse {
                set_up: status.not_set_up.is_none(),
                reason: status
                    .not_set_up
                    .map(|reason| reason.reason().to_owned())
                    .unwrap_or_default(),
                locked: status.locked,
                tries: status.tries,
            },
        ))
    }

    async fn list_devices(
        &self,
        _request: tonic::Request<harold_api::harold::ListDevicesRequest>,
    ) -> Result<tonic::Response<harold_api::harold::ListDevicesResponse>, tonic::Status> {
        let devices = self
            .pairing()?
            .devices()
            .into_iter()
            .map(|device| harold_api::harold::PairedDevice {
                name: device.name,
                paired_at_ms: device.paired_at_ms,
                last_seen_ms: device.last_seen_ms,
            })
            .collect();
        Ok(tonic::Response::new(
            harold_api::harold::ListDevicesResponse { devices },
        ))
    }

    async fn unpair_device(
        &self,
        request: tonic::Request<harold_api::harold::UnpairDeviceRequest>,
    ) -> Result<tonic::Response<harold_api::harold::UnpairDeviceResponse>, tonic::Status> {
        let removed = self
            .pairing()?
            .unpair(&request.into_inner().name)
            .map_err(|_| tonic::Status::internal("device list not written"))?;
        if !removed {
            return Err(tonic::Status::not_found(NO_SUCH_DEVICE));
        }
        Ok(tonic::Response::new(
            harold_api::harold::UnpairDeviceResponse {},
        ))
    }
}
