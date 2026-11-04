//! The slave side of the slave link. The slave opens one stream to the master and keeps it
//! open. It reads `Welcome` before it sends any report, sends the reports the master has not
//! applied, then the newest `Snapshot` and `Lock`, and keeps each report until an `Ack`.
//!
//! The way a stream is opened is a `Connector`, so a test gives the master service in the same
//! process and opens no port. No line written here holds the secret, a typed line or a turn's
//! text.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use harold_api::harold::slave::v1::slave_link_client::SlaveLinkClient;
use harold_api::harold::slave::v1::{
    Hello, Lock, MasterMessage, SlaveMessage, Snapshot, Turn, master_message, slave_message,
};
use harold_api::harold::{AgentStateSnapshot, TurnCompleteRequest};
use prost::Message;
use tokio::sync::{Notify, mpsc, watch};
use tokio::task::JoinHandle;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
use tracing::{debug, info, warn};

use super::backlog::{ReportBacklog, ReportKind};
use super::master::{LINK_PROTOCOL, NAME_HEADER, SECRET_HEADER};
use super::pairing::{read_secret, slave_secret_path};
use crate::settings::MasterSettings;
use crate::store::StoreError;

/// Where the slave reads what it reports.
pub(crate) trait SlaveSource: Send + Sync + 'static {
    /// The newest agent snapshot of this machine.
    fn snapshots(&self) -> watch::Receiver<AgentStateSnapshot>;
    /// Whether the screen of this machine is locked.
    fn locks(&self) -> watch::Receiver<bool>;
}

/// The file the backlog lives in, in the slave's own data folder.
const BACKLOG_FILE: &str = "report-backlog.db";

/// How long the slave waits between connects, and when a connect counts as stable.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Timing {
    pub(crate) initial: Duration,
    pub(crate) max: Duration,
    /// A link that stays up this long resets the wait to `initial`.
    pub(crate) stable: Duration,
    /// How often the backlog is trimmed while the link is up.
    pub(crate) trim_every: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            initial: Duration::from_secs(1),
            max: Duration::from_secs(30),
            stable: Duration::from_secs(10),
            trim_every: Duration::from_secs(60 * 60),
        }
    }
}

pub(crate) type OutboundStream = ReceiverStream<SlaveMessage>;
pub(crate) type OpenFuture =
    Pin<Box<dyn Future<Output = Result<Response<Streaming<MasterMessage>>, Status>> + Send>>;

/// The connect step. A production connector opens a tonic channel to `[master] address`; a
/// test connector calls the master service in the same process.
pub(crate) trait Connector: Send + Sync + 'static {
    fn open(&self, request: Request<OutboundStream>) -> OpenFuture;
}

/// Opens a tonic channel to `[master] address` for each connect.
pub(crate) struct TcpConnector {
    address: String,
}

impl TcpConnector {
    pub(crate) fn new(master: &MasterSettings) -> Self {
        Self {
            address: master.address.clone(),
        }
    }
}

impl Connector for TcpConnector {
    fn open(&self, request: Request<OutboundStream>) -> OpenFuture {
        let address = self.address.clone();
        Box::pin(async move {
            let endpoint = tonic::transport::Endpoint::from_shared(format!("http://{address}"))
                .map_err(|_| Status::invalid_argument("master address"))?
                .connect_timeout(Duration::from_secs(10))
                .http2_keep_alive_interval(Duration::from_secs(30))
                .keep_alive_while_idle(true);
            let channel = endpoint
                .connect()
                .await
                .map_err(|_| Status::unavailable("master not reachable"))?;
            SlaveLinkClient::new(channel).open(request).await
        })
    }
}

/// Why a stream ended, as the `slave link closed` line names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Closed {
    MasterGone,
    Network,
}

impl Closed {
    fn label(self) -> &'static str {
        match self {
            Closed::MasterGone => "master_gone",
            Closed::Network => "network",
        }
    }
}

struct Shared {
    name: String,
    version: String,
    secret_path: PathBuf,
    backlog: ReportBacklog,
    /// The newest report number the master is known to have: its last `Welcome` or `Ack`.
    /// `Hello.last_known_report` carries it.
    known: AtomicU64,
    /// Set after the first warning about a connect that failed, so the warning comes once.
    warned: AtomicBool,
    /// The backlog's last number when this process opened it. Rows above it are new rows:
    /// reports this process made before its first `Welcome`.
    start_last: u64,
    /// Set when the first `Welcome` of this process has been handled.
    welcomed_once: AtomicBool,
    /// Wakes the stream when a report is appended.
    wake: Notify,
    timing: Timing,
}

/// The running slave link.
pub(crate) struct SlaveLink {
    shared: Arc<Shared>,
    task: JoinHandle<()>,
}

impl SlaveLink {
    /// Opens the backlog in `data` and starts the link task.
    pub(crate) async fn start(
        connector: Arc<dyn Connector>,
        name: &str,
        version: &str,
        data: &Path,
        source: impl SlaveSource,
        timing: Timing,
    ) -> Result<Self, StoreError> {
        let backlog = ReportBacklog::open(&data.join(BACKLOG_FILE)).await?;
        let start_last = backlog.last_number();
        let shared = Arc::new(Shared {
            name: name.to_owned(),
            version: version.to_owned(),
            secret_path: slave_secret_path(data),
            backlog,
            known: AtomicU64::new(0),
            warned: AtomicBool::new(false),
            start_last,
            welcomed_once: AtomicBool::new(false),
            wake: Notify::new(),
            timing,
        });
        let task = tokio::spawn(run(
            Arc::clone(&shared),
            connector,
            source.snapshots(),
            source.locks(),
        ));
        Ok(Self { shared, task })
    }

    /// Keeps a turn end and sends it when the link is up. `transcript_path` goes empty: it is
    /// a path on this machine and the master never reads it.
    pub(crate) async fn report_turn(&self, request: TurnCompleteRequest) {
        let turn = Turn {
            report_no: 0,
            pane_id: request.pane_id,
            pane_label: request.pane_label,
            session_id: request.session_id,
            turn_started_at: request.turn_started_at,
            last_user_prompt: request.last_user_prompt,
            assistant_message: request.assistant_message,
            main_context: request.main_context,
            input_tokens: request.input_tokens,
            output_tokens: request.output_tokens,
            cache_read_tokens: request.cache_read_tokens,
            cache_write_tokens: request.cache_write_tokens,
            model: request.model,
            transcript_path: String::new(),
        };
        let message = SlaveMessage {
            body: Some(slave_message::Body::Turn(turn)),
        };
        self.shared.keep(ReportKind::Turn, message).await;
    }

    /// Stops the link task.
    pub(crate) fn abort(&self) {
        self.task.abort();
    }
}

impl Drop for SlaveLink {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The running link of this process, set once when `[master]` exists.
static LINK: std::sync::OnceLock<SlaveLink> = std::sync::OnceLock::new();

/// A source that holds the newest values for the link. The agent monitor and the lock check
/// publish into it.
pub(crate) struct LocalSource {
    snapshot: watch::Sender<AgentStateSnapshot>,
    lock: watch::Sender<bool>,
}

impl LocalSource {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            snapshot: watch::channel(AgentStateSnapshot::default()).0,
            lock: watch::channel(false).0,
        })
    }

    /// Sets the newest agent snapshot. The agent monitor calls it (slice 2c.1 task 7).
    #[allow(dead_code)]
    pub(crate) fn publish_snapshot(&self, snapshot: AgentStateSnapshot) {
        self.snapshot.send_replace(snapshot);
    }

    /// Sets the lock state. The lock check calls it (slice 2c.1 task 7).
    #[allow(dead_code)]
    pub(crate) fn publish_lock(&self, locked: bool) {
        self.lock.send_replace(locked);
    }
}

impl SlaveSource for Arc<LocalSource> {
    fn snapshots(&self) -> watch::Receiver<AgentStateSnapshot> {
        self.snapshot.subscribe()
    }
    fn locks(&self) -> watch::Receiver<bool> {
        self.lock.subscribe()
    }
}

/// Starts the link and keeps it for the process. A second call changes nothing.
pub(crate) async fn install(
    master: &MasterSettings,
    name: &str,
    data: &Path,
    source: impl SlaveSource,
) -> Result<(), StoreError> {
    let link = run_slave_link(master, name, data, source).await?;
    if LINK.set(link).is_err() {
        warn!("slave link already running");
    }
    Ok(())
}

/// Whether this process runs a slave link.
pub(crate) fn linked() -> bool {
    LINK.get().is_some()
}

/// Keeps a turn end for the master. It does nothing when no link runs.
pub(crate) async fn report_turn(request: TurnCompleteRequest) {
    if let Some(link) = LINK.get() {
        link.report_turn(request).await;
    }
}

/// Stops the link of this process.
pub(crate) fn stop() {
    if let Some(link) = LINK.get() {
        link.abort();
    }
}

/// Starts the slave link to `[master] address` for the slave `name`.
pub(crate) async fn run_slave_link(
    master: &MasterSettings,
    name: &str,
    data: &Path,
    source: impl SlaveSource,
) -> Result<SlaveLink, StoreError> {
    SlaveLink::start(
        Arc::new(TcpConnector::new(master)),
        name,
        env!("CARGO_PKG_VERSION"),
        data,
        source,
        Timing::default(),
    )
    .await
}

impl Shared {
    /// Keeps a report on disk at once, with a number above the backlog's last. A report made
    /// before the first `Welcome` is renumbered then, if the master's number is above it.
    async fn keep(&self, kind: ReportKind, message: SlaveMessage) {
        self.append(kind, &message).await;
    }

    async fn append(&self, kind: ReportKind, message: &SlaveMessage) {
        match self
            .backlog
            .append(kind, message.encode_to_vec(), SystemTime::now())
            .await
        {
            Ok(_) => self.wake.notify_one(),
            Err(error) => warn!(slave = %self.name, error = %error, "slave report not kept"),
        }
    }

    /// Handles a `Welcome`: the numbers rise above it, and the reports the master has go.
    ///
    /// At the first `Welcome` of the process, the old rows (up to `start_last`) go when the
    /// master has them. The new rows (above `start_last`) never go: when `welcome` is at or
    /// above `start_last` they are renumbered above `welcome`, so the master does not take
    /// them as repeats. When `welcome` is below `start_last`, they are already above it.
    async fn welcomed(&self, welcome: u64) {
        let own = self.backlog.last_number();
        if welcome > own {
            warn!(own, welcome, "report backlog behind");
        }
        self.known.fetch_max(welcome, Ordering::SeqCst);
        let first = !self.welcomed_once.swap(true, Ordering::SeqCst);
        let result = if first {
            self.first_welcome(welcome).await
        } else {
            self.backlog.rebase(welcome);
            self.backlog.ack(welcome).await
        };
        if let Err(error) = result {
            warn!(slave = %self.name, error = %error, "slave report not kept");
        }
    }

    async fn first_welcome(&self, welcome: u64) -> crate::store::Result<()> {
        self.backlog.ack(welcome.min(self.start_last)).await?;
        if welcome >= self.start_last {
            self.backlog
                .renumber_new_above(self.start_last, welcome)
                .await?;
        }
        self.backlog.rebase(welcome);
        Ok(())
    }

    /// Warns once per process start that a connect failed, and why. Never holds the secret.
    fn warn_once(&self, reason: &'static str) {
        if !self.warned.swap(true, Ordering::SeqCst) {
            warn!(slave = %self.name, reason, "slave link not opened");
        }
    }

    async fn trim(&self) {
        match self.backlog.trim(SystemTime::now()).await {
            Ok(dropped) => {
                if dropped.too_old > 0 {
                    warn!(
                        count = dropped.too_old,
                        reason = "too_old",
                        "slave report dropped"
                    );
                }
                if dropped.too_many > 0 {
                    warn!(
                        count = dropped.too_many,
                        reason = "too_many",
                        "slave report dropped"
                    );
                }
            }
            Err(error) => warn!(slave = %self.name, error = %error, "slave report not kept"),
        }
    }
}

/// Connects again and again, with a wait that doubles up to the maximum.
async fn run(
    shared: Arc<Shared>,
    connector: Arc<dyn Connector>,
    mut snapshots: watch::Receiver<AgentStateSnapshot>,
    mut locks: watch::Receiver<bool>,
) {
    let timing = shared.timing;
    let mut wait = timing.initial;
    loop {
        let started = Instant::now();
        shared.trim().await;
        let (closed, opened) = session(&shared, &*connector, &mut snapshots, &mut locks).await;
        if opened && started.elapsed() >= timing.stable {
            wait = timing.initial;
        }
        if !opened {
            debug!(slave = %shared.name, reason = closed.label(), "slave link not opened");
        }
        info!(slave = %shared.name, reason = closed.label(), "slave link closed");
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(timing.max);
    }
}

fn hello(shared: &Shared) -> SlaveMessage {
    SlaveMessage {
        body: Some(slave_message::Body::Hello(Hello {
            name: shared.name.clone(),
            version: shared.version.clone(),
            last_known_report: shared.known.load(Ordering::SeqCst),
            protocol: LINK_PROTOCOL,
        })),
    }
}

/// One stream. Returns why it ended and whether it got as far as `Welcome`.
async fn session(
    shared: &Shared,
    connector: &dyn Connector,
    snapshots: &mut watch::Receiver<AgentStateSnapshot>,
    locks: &mut watch::Receiver<bool>,
) -> (Closed, bool) {
    let Some(secret) = read_secret(&shared.secret_path) else {
        shared.warn_once("no_secret");
        return (Closed::Network, false);
    };
    let (send, receive) = mpsc::channel::<SlaveMessage>(64);
    if send.send(hello(shared)).await.is_err() {
        return (Closed::Network, false);
    }
    let mut request = Request::new(ReceiverStream::new(receive));
    let (Ok(name), Ok(secret)) = (shared.name.parse(), secret.parse()) else {
        shared.warn_once("refused");
        return (Closed::Network, false);
    };
    request.metadata_mut().insert(NAME_HEADER, name);
    request.metadata_mut().insert(SECRET_HEADER, secret);
    let response = match connector.open(request).await {
        Ok(response) => response,
        Err(status) => {
            match status.code() {
                tonic::Code::Unauthenticated => shared.warn_once("refused"),
                tonic::Code::FailedPrecondition => shared.warn_once("version"),
                _ => {}
            }
            return (Closed::Network, false);
        }
    };
    let mut inbound = response.into_inner();
    let welcome = match inbound.message().await {
        Ok(Some(MasterMessage {
            body: Some(master_message::Body::Welcome(welcome)),
        })) => welcome.last_report,
        Ok(_) => return (Closed::MasterGone, false),
        Err(_) => return (Closed::Network, false),
    };
    info!(
        slave = %shared.name,
        welcome,
        version = %shared.version,
        "slave link opened"
    );
    let reason = stream_reports(shared, welcome, send, &mut inbound, snapshots, locks).await;
    (reason, true)
}

/// The decoded newest report of a kind, to know what the master has not seen.
fn lock_value(message: &SlaveMessage) -> Option<bool> {
    match &message.body {
        Some(slave_message::Body::Lock(lock)) => Some(lock.locked),
        _ => None,
    }
}

fn numbered(payload: &[u8], number: u64) -> Option<SlaveMessage> {
    let mut message = SlaveMessage::decode(payload).ok()?;
    match &mut message.body {
        Some(slave_message::Body::Turn(turn)) => turn.report_no = number,
        Some(slave_message::Body::Lock(lock)) => lock.report_no = number,
        _ => return None,
    }
    Some(message)
}

async fn snapshot_message(
    shared: &Shared,
    snapshots: &mut watch::Receiver<AgentStateSnapshot>,
) -> SlaveMessage {
    let snapshot = snapshots.borrow_and_update().clone();
    SlaveMessage {
        body: Some(slave_message::Body::Snapshot(Snapshot {
            snapshot: Some(snapshot),
            backlog: shared.backlog.len(),
        })),
    }
}

async fn stream_reports(
    shared: &Shared,
    welcome: u64,
    send: mpsc::Sender<SlaveMessage>,
    inbound: &mut Streaming<MasterMessage>,
    snapshots: &mut watch::Receiver<AgentStateSnapshot>,
    locks: &mut watch::Receiver<bool>,
) -> Closed {
    // The numbers rise above `Welcome` before any report goes out.
    shared.welcomed(welcome).await;
    shared.trim().await;
    // The master needs the newest lock. A kept `Lock` above `Welcome` may say it.
    let current_lock = *locks.borrow_and_update();
    let kept = shared.backlog.after(welcome).await.unwrap_or_default();
    let last_kept_lock = kept
        .iter()
        .rev()
        .filter_map(|(_, payload)| SlaveMessage::decode(payload.as_slice()).ok())
        .find_map(|message| lock_value(&message));
    if last_kept_lock != Some(current_lock) {
        let message = SlaveMessage {
            body: Some(slave_message::Body::Lock(Lock {
                report_no: 0,
                locked: current_lock,
            })),
        };
        shared.append(ReportKind::Lock, &message).await;
    }

    let mut sent = welcome;
    let mut snapshot_due = true;
    let mut snapshots_open = true;
    let mut locks_open = true;
    let mut trim = tokio::time::interval(shared.timing.trim_every);
    trim.tick().await;
    loop {
        match shared.backlog.after(sent).await {
            Ok(pending) => {
                for (number, payload) in pending {
                    sent = number;
                    let Some(message) = numbered(&payload, number) else {
                        continue;
                    };
                    if send.send(message).await.is_err() {
                        return Closed::Network;
                    }
                    snapshot_due = true;
                }
            }
            Err(error) => {
                warn!(slave = %shared.name, error = %error, "slave report not kept");
                return Closed::Network;
            }
        }
        if snapshot_due {
            snapshot_due = false;
            if send
                .send(snapshot_message(shared, snapshots).await)
                .await
                .is_err()
            {
                return Closed::Network;
            }
        }
        tokio::select! {
            _ = shared.wake.notified() => {}
            changed = snapshots.changed(), if snapshots_open => {
                // A gone source sends no more snapshots, and the stream goes on.
                snapshots_open = changed.is_ok();
                snapshot_due = true;
            }
            changed = locks.changed(), if locks_open => {
                if changed.is_err() {
                    locks_open = false;
                    continue;
                }
                let locked = *locks.borrow_and_update();
                let message = SlaveMessage {
                    body: Some(slave_message::Body::Lock(Lock { report_no: 0, locked })),
                };
                shared.append(ReportKind::Lock, &message).await;
            }
            _ = trim.tick() => shared.trim().await,
            message = inbound.message() => match message {
                Ok(Some(MasterMessage { body: Some(master_message::Body::Ack(ack)) })) => {
                    if let Err(error) = shared.backlog.ack(ack.report_no).await {
                        warn!(slave = %shared.name, error = %error, "slave report not kept");
                    }
                    shared.known.fetch_max(ack.report_no, Ordering::SeqCst);
                    sent = sent.max(ack.report_no);
                    snapshot_due = true;
                }
                Ok(Some(_)) => {}
                Ok(None) => return Closed::MasterGone,
                Err(status) if status.code() == tonic::Code::Internal => {
                    return Closed::MasterGone;
                }
                Err(_) => return Closed::Network,
            },
        }
    }
}

#[cfg(test)]
#[path = "slave_tests.rs"]
mod slave_tests;
