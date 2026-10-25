//! The TypeDB server as a child process of Harold. Harold starts it on the first graph call,
//! gives it a config that listens on 127.0.0.1 only and reports nothing, and stops it after the
//! idle time. Harold never downloads the binary: the owner installs it and sets its path.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tracing::{info, warn};

use super::backend::BackendError;

/// How long a start waits for the server to answer its health check.
const START_LIMIT: Duration = Duration::from_secs(60);
/// How long a stop waits for the server to exit before it kills it.
const STOP_LIMIT: Duration = Duration::from_secs(15);
/// How often the reaper looks at the idle time.
const REAP_EVERY: Duration = Duration::from_secs(30);
const REQUEST_LIMIT: Duration = Duration::from_secs(120);
const CONNECT_LIMIT: Duration = Duration::from_secs(5);
/// The only TypeDB version Harold starts: the one audited in `docs/references/dependency-audits.md`.
pub(crate) const PINNED_VERSION: &str = "3.14.1";
/// The account a new TypeDB data folder starts with. Harold replaces its password at once.
const USER: &str = "admin";
const DEFAULT_PASSWORD: &str = "password";

/// A server Harold may start. `binary` is the path the owner set in `[knowledge.graph]`.
pub(crate) struct ServerConfig {
    pub binary: PathBuf,
    /// `<store>/tools/knowledge/graph/typedb/`: the config, the data and the logs live here.
    pub folder: PathBuf,
    pub idle: Duration,
}

struct Live {
    child: Child,
    base: String,
    token: Option<String>,
    last_used: Instant,
}

/// What a request needs: where the server is and who Harold is to it.
#[derive(Clone)]
pub(crate) struct Lease {
    pub base: String,
    pub token: String,
}

pub(crate) struct TypeDbServer {
    config: ServerConfig,
    client: reqwest::Client,
    live: Mutex<Option<Live>>,
}

/// Refuses any address that is not on this Mac's loopback.
pub(crate) fn require_loopback(address: SocketAddr) -> Result<(), BackendError> {
    match address.ip() {
        IpAddr::V4(ip) if ip.is_loopback() => Ok(()),
        _ => Err(BackendError::Refused("not_loopback".into())),
    }
}

/// The config file TypeDB reads. Both listeners are on 127.0.0.1, nothing is reported to
/// TypeDB's makers, and the monitoring port is off.
pub(crate) fn config_text(
    grpc: SocketAddr,
    http: SocketAddr,
    folder: &Path,
) -> Result<String, BackendError> {
    require_loopback(grpc)?;
    require_loopback(http)?;
    let quote = |path: PathBuf| {
        let text = path
            .to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"");
        format!("\"{text}\"")
    };
    let lines = [
        "# Written by Harold each time it starts TypeDB. Edits are lost.".to_string(),
        "server:".into(),
        format!("    listen-address: {grpc}"),
        "    advertise-address:".into(),
        "    http:".into(),
        "        enabled: true".into(),
        format!("        listen-address: {http}"),
        "        advertise-address:".into(),
        "    admin:".into(),
        "        enabled: false".into(),
        "        socket-path:".into(),
        "    authentication:".into(),
        "        token-expiration-seconds: 5000".into(),
        "    encryption:".into(),
        "        enabled: false".into(),
        "        certificate:".into(),
        "        certificate-key:".into(),
        "        ca-certificate:".into(),
        "storage:".into(),
        format!("    data-directory: {}", quote(folder.join("data"))),
        "    rocksdb:".into(),
        "        cache-size: 256mb".into(),
        "        write-buffers-limit: 128mb".into(),
        "    mvcc:".into(),
        "        cleanup:".into(),
        "            enabled: false".into(),
        "            strategy: eager".into(),
        "logging:".into(),
        format!("    directory: {}", quote(folder.join("log"))),
        "diagnostics:".into(),
        "    monitoring:".into(),
        "        enabled: false".into(),
        "        port: 4104".into(),
        "    reporting:".into(),
        "        metrics: false".into(),
        "        errors: false".into(),
    ];
    Ok(lines.join("\n") + "\n")
}

fn free_port() -> Result<u16, BackendError> {
    let listener =
        TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(|_| BackendError::Unreachable)?;
    listener
        .local_addr()
        .map(|address| address.port())
        .map_err(|_| BackendError::Unreachable)
}

fn private_folder(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(text.as_bytes())
}

/// Ends a server taken out of `live`: a polite end first, a kill after `STOP_LIMIT`.
async fn end(mut live: Live) {
    if let Some(pid) = live.child.id() {
        let _ = Command::new("/bin/kill")
            .args(["-TERM", &pid.to_string()])
            .status()
            .await;
    }
    if tokio::time::timeout(STOP_LIMIT, live.child.wait())
        .await
        .is_err()
    {
        let _ = live.child.kill().await;
    }
    info!("knowledge graph server stopped");
}

impl TypeDbServer {
    pub(crate) fn new(config: ServerConfig) -> Arc<Self> {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_LIMIT)
            .connect_timeout(CONNECT_LIMIT)
            .no_proxy()
            .build()
            .unwrap_or_default();
        Arc::new(Self {
            config,
            client,
            live: Mutex::new(None),
        })
    }

    pub(crate) fn client(&self) -> &reqwest::Client {
        &self.client
    }

    fn password_file(&self) -> PathBuf {
        self.config.folder.join("password")
    }

    /// The server's address and token, starting the server first when it is not running.
    pub(crate) async fn lease(&self) -> Result<Lease, BackendError> {
        let mut guard = self.live.lock().await;
        if let Some(live) = guard.as_mut() {
            if matches!(live.child.try_wait(), Ok(None)) {
                live.last_used = Instant::now();
                if let Some(token) = &live.token {
                    return Ok(Lease {
                        base: live.base.clone(),
                        token: token.clone(),
                    });
                }
            } else {
                *guard = None;
            }
        }
        if guard.is_none() {
            *guard = Some(self.start().await?);
        }
        let live = guard.as_mut().ok_or(BackendError::Unreachable)?;
        let token = self.sign_in(&live.base).await?;
        live.token = Some(token.clone());
        live.last_used = Instant::now();
        Ok(Lease {
            base: live.base.clone(),
            token,
        })
    }

    /// The server's address and token when it runs and is signed in. It never starts the
    /// server and never marks it used, so a periodic look keeps neither it nor its idle time.
    pub(crate) async fn peek_lease(&self) -> Option<Lease> {
        let mut guard = self.live.lock().await;
        let live = guard.as_mut()?;
        if !matches!(live.child.try_wait(), Ok(None)) {
            return None;
        }
        Some(Lease {
            base: live.base.clone(),
            token: live.token.clone()?,
        })
    }

    /// Gives the server a running process and a token, as a start would.
    #[cfg(test)]
    pub(crate) async fn adopt_for_test(&self, child: Child) {
        *self.live.lock().await = Some(Live {
            child,
            base: "http://127.0.0.1:1".into(),
            token: Some("token".into()),
            last_used: Instant::now(),
        });
    }

    /// Forgets the token after the server refused it, so the next lease signs in again.
    pub(crate) async fn forget_token(&self) {
        if let Some(live) = self.live.lock().await.as_mut() {
            live.token = None;
        }
    }

    /// Refuses a binary that is not the audited version.
    async fn check_version(&self) -> Result<(), BackendError> {
        let output = Command::new(&self.config.binary)
            .arg("--version")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|error| {
                warn!(error = %error, "knowledge graph server did not start");
                BackendError::Unreachable
            })?;
        let text = String::from_utf8_lossy(&output.stdout);
        if text.trim() == format!("server {PINNED_VERSION}") {
            return Ok(());
        }
        warn!(
            wanted = PINNED_VERSION,
            "knowledge graph server is not the audited version"
        );
        Err(BackendError::Unreachable)
    }

    async fn start(&self) -> Result<Live, BackendError> {
        self.check_version().await?;
        let started = Instant::now();
        let folder = &self.config.folder;
        for sub in ["", "data", "log"] {
            private_folder(&folder.join(sub)).map_err(|_| BackendError::Unreachable)?;
        }
        let grpc = SocketAddr::from((Ipv4Addr::LOCALHOST, free_port()?));
        let http = SocketAddr::from((Ipv4Addr::LOCALHOST, free_port()?));
        let config_file = folder.join("config.yml");
        write_private(&config_file, &config_text(grpc, http, folder)?)
            .map_err(|_| BackendError::Unreachable)?;
        let mut child = Command::new(&self.config.binary)
            .arg("--config")
            .arg(&config_file)
            .current_dir(folder)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| {
                warn!(error = %error, "knowledge graph server did not start");
                BackendError::Unreachable
            })?;
        let base = format!("http://{http}");
        let deadline = Instant::now() + START_LIMIT;
        loop {
            if matches!(child.try_wait(), Ok(Some(_)) | Err(_)) {
                warn!("knowledge graph server exited while it started");
                return Err(BackendError::Unreachable);
            }
            let healthy = self
                .client
                .get(format!("{base}/health"))
                .send()
                .await
                .is_ok_and(|reply| reply.status().is_success());
            if healthy {
                break;
            }
            if Instant::now() >= deadline {
                let _ = child.kill().await;
                warn!("knowledge graph server did not answer in time");
                return Err(BackendError::Unreachable);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        info!(
            start_ms = started.elapsed().as_millis() as u64,
            "knowledge graph server started"
        );
        Ok(Live {
            child,
            base,
            token: None,
            last_used: Instant::now(),
        })
    }

    async fn sign_in_with(
        &self,
        base: &str,
        password: &str,
    ) -> Result<Option<String>, BackendError> {
        let reply = self
            .client
            .post(format!("{base}/v1/signin"))
            .json(&json!({"username": USER, "password": password}))
            .send()
            .await
            .map_err(|_| BackendError::Unreachable)?;
        match reply.status().as_u16() {
            200 => {
                let body: Value = reply.json().await.map_err(|_| BackendError::Unreachable)?;
                Ok(body["token"].as_str().map(str::to_string))
            }
            401 => Ok(None),
            _ => Err(BackendError::Unreachable),
        }
    }

    /// Signs in with Harold's own password. A new data folder still has the default password,
    /// so Harold replaces it with a random one it keeps in a private file.
    async fn sign_in(&self, base: &str) -> Result<String, BackendError> {
        let file = self.password_file();
        if let Ok(saved) = std::fs::read_to_string(&file)
            && let Some(token) = self.sign_in_with(base, saved.trim()).await?
        {
            return Ok(token);
        }
        let Some(token) = self.sign_in_with(base, DEFAULT_PASSWORD).await? else {
            warn!("knowledge graph server refused the sign-in");
            return Err(BackendError::Unreachable);
        };
        let password = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        write_private(&file, &password).map_err(|_| BackendError::Unreachable)?;
        let changed = self
            .client
            .put(format!("{base}/v1/users/{USER}"))
            .bearer_auth(&token)
            .json(&json!({"password": password}))
            .send()
            .await
            .map_err(|_| BackendError::Unreachable)?;
        if !changed.status().is_success() {
            let _ = std::fs::remove_file(&file);
            return Err(BackendError::Unreachable);
        }
        self.sign_in_with(base, &password)
            .await?
            .ok_or(BackendError::Unreachable)
    }

    /// Stops the server now, if it runs: a polite end first, a kill after `STOP_LIMIT`.
    pub(crate) async fn stop(&self) {
        let mut guard = self.live.lock().await;
        if let Some(live) = guard.take() {
            end(live).await;
        }
    }

    /// Checks the idle time and takes the server in one lock hold, and ends it before the lock
    /// goes. A call that comes meanwhile waits, then finds no server and starts it again.
    async fn stop_if_idle(&self) {
        let mut guard = self.live.lock().await;
        let idle = guard
            .as_ref()
            .is_some_and(|live| live.last_used.elapsed() >= self.config.idle);
        if !idle {
            return;
        }
        if let Some(live) = guard.take() {
            end(live).await;
        }
    }

    /// Stops the server after the idle time with no call, and at shutdown.
    pub(crate) fn spawn_reaper(
        self: &Arc<Self>,
        mut shutdown: tokio::sync::watch::Receiver<()>,
    ) -> tokio::task::JoinHandle<()> {
        let server = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = tokio::time::sleep(REAP_EVERY) => server.stop_if_idle().await,
                    _ = shutdown.changed() => {
                        server.stop().await;
                        return;
                    }
                }
            }
        })
    }
}

#[cfg(test)]
#[path = "server_tests.rs"]
mod tests;
