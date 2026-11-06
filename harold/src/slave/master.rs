//! The master side of the slave link. A slave opens one stream. The master checks the secret
//! in stream metadata before it reads a message, answers `Hello` with `Welcome`, applies each
//! `Turn` and `Lock` above the last applied number once, and sends an `Ack`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use harold_api::harold::AgentStateSnapshot;
use harold_api::harold::slave::v1::slave_link_server::{SlaveLink, SlaveLinkServer};
use harold_api::harold::slave::v1::{
    Ack, MasterMessage, SlaveMessage, Turn, Welcome, master_message, slave_message,
};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_stream::Stream;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
use tracing::{info, warn};

use super::pairing::{master_secret_path, read_secret, secrets_equal};
use crate::settings::SlavesSettings;
use crate::telemetry;

/// The metadata key that holds the slave's secret. The secret never goes in a message.
pub(crate) const SECRET_HEADER: &str = "x-harold-slave-secret";
/// The metadata key that holds the slave's name.
pub(crate) const NAME_HEADER: &str = "x-harold-slave-name";
/// The link protocol version this master knows.
pub(crate) const LINK_PROTOCOL: u32 = 1;
/// How often the listener pings a quiet link. A slave that sleeps or loses its network
/// answers no ping, so the stream ends and `slave-link-down` can fire.
pub(crate) const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
/// How long the listener waits for the answer to a ping before it ends the stream.
pub(crate) const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(20);
/// How long a new stream may take to send `Hello`.
const HELLO_WAIT: Duration = Duration::from_secs(10);

/// Whether the sink took a report. After `Failed` the master keeps the report number, sends
/// no `Ack` and ends the stream, so the slave sends the report again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Applied {
    Applied,
    Failed,
}

/// Where the master puts what a slave reports.
pub(crate) trait MasterSink: Send + Sync + 'static {
    fn apply_snapshot(&self, slave: &str, snapshot: AgentStateSnapshot);
    /// Applies a turn. The master repeats nothing: it dedupes by report number before this call.
    fn apply_turn(&self, slave: &str, turn: Turn) -> Applied;
    fn apply_lock(&self, slave: &str, locked: bool) -> Applied;
    fn link_changed(&self, slave: &str, up: bool);
}

/// The name of the slave that passed the secret check. The interceptor sets it.
#[derive(Clone)]
struct AuthenticatedSlave(String);

type Replies = Pin<Box<dyn Stream<Item = Result<MasterMessage, Status>> + Send>>;

pub(crate) struct SlaveLinkService {
    data: PathBuf,
    sink: Arc<dyn MasterSink>,
    /// What all streams of a slave share: the newest stream, so a stream that a newer one
    /// replaced stays quiet when it ends, and the last applied report number, so two streams
    /// cannot apply one report twice.
    slaves: Arc<Mutex<HashMap<String, SlaveState>>>,
}

#[derive(Default)]
struct SlaveState {
    generation: u64,
    last_report: u64,
}

impl SlaveLinkService {
    pub(crate) fn new(data: &Path, sink: Arc<dyn MasterSink>) -> Self {
        Self {
            data: data.to_path_buf(),
            sink,
            slaves: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Registers a new stream of `slave`. Returns its generation and the last applied number.
    fn open_stream(&self, slave: &str) -> (u64, u64) {
        let mut slaves = self.slaves.lock().unwrap_or_else(PoisonError::into_inner);
        let state = slaves
            .entry(slave.to_owned())
            .or_insert_with(|| SlaveState {
                generation: 0,
                last_report: read_last_report(&self.data, slave),
            });
        state.generation += 1;
        (state.generation, state.last_report)
    }
}

fn refuse(reason: &'static str, slave: Option<&str>) {
    match slave {
        Some(slave) => warn!(slave, reason, "slave link refused"),
        None => warn!(reason, "slave link refused"),
    }
}

/// The interceptor: checks the name and the secret before the service reads a message.
#[derive(Clone)]
pub(crate) struct Guard {
    data: PathBuf,
    names: Vec<String>,
}

impl tonic::service::Interceptor for Guard {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        let metadata = request.metadata();
        let name = metadata
            .get(NAME_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let supplied = metadata
            .get(SECRET_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let denied = || Status::unauthenticated("refused");
        let Some(name) = name.filter(|name| self.names.contains(name)) else {
            refuse("unknown_slave", None);
            return Err(denied());
        };
        let Some(expected) = read_secret(&master_secret_path(&self.data, &name)) else {
            refuse("unknown_slave", Some(&name));
            return Err(denied());
        };
        let Some(supplied) = supplied else {
            refuse("bad_secret", Some(&name));
            return Err(denied());
        };
        if !secrets_equal(&supplied, &expected) {
            refuse("bad_secret", Some(&name));
            return Err(denied());
        }
        request.extensions_mut().insert(AuthenticatedSlave(name));
        Ok(request)
    }
}

fn state_path(data: &Path, slave: &str) -> PathBuf {
    data.join("slaves").join(format!("{slave}.json"))
}

/// The last report number the master applied for `slave`. 0 when there is none.
fn read_last_report(data: &Path, slave: &str) -> u64 {
    let Ok(text) = std::fs::read_to_string(state_path(data, slave)) else {
        return 0;
    };
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|value| value["last_report"].as_u64())
        .unwrap_or(0)
}

/// Writes the number whole or not at all.
fn write_last_report(data: &Path, slave: &str, number: u64) -> std::io::Result<()> {
    let path = state_path(data, slave);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("json.tmp");
    std::fs::write(
        &temporary,
        serde_json::json!({ "last_report": number }).to_string(),
    )?;
    std::fs::rename(&temporary, path)
}

#[tonic::async_trait]
impl SlaveLink for SlaveLinkService {
    type OpenStream = Replies;

    async fn open(
        &self,
        request: Request<Streaming<SlaveMessage>>,
    ) -> Result<Response<Self::OpenStream>, Status> {
        let slave = request
            .extensions()
            .get::<AuthenticatedSlave>()
            .map(|slave| slave.0.clone())
            .ok_or_else(|| Status::unauthenticated("refused"))?;
        let mut inbound = request.into_inner();
        let first = tokio::time::timeout(HELLO_WAIT, inbound.message())
            .await
            .map_err(|_| Status::deadline_exceeded("no hello"))??;
        let Some(slave_message::Body::Hello(hello)) = first.and_then(|message| message.body) else {
            return Err(Status::invalid_argument("hello first"));
        };
        if hello.name != slave {
            refuse("unknown_slave", Some(&slave));
            return Err(Status::unauthenticated("refused"));
        }
        if hello.protocol != LINK_PROTOCOL {
            refuse("version", Some(&slave));
            return Err(Status::failed_precondition("version"));
        }
        let (generation, last_report) = self.open_stream(&slave);
        let (replies, outbound) = mpsc::channel(16);
        let welcome = MasterMessage {
            body: Some(master_message::Body::Welcome(Welcome { last_report })),
        };
        replies
            .send(Ok(welcome))
            .await
            .map_err(|_| Status::cancelled("closed"))?;
        info!(slave = %slave, welcome = last_report, version = %hello.version, "slave link opened");
        self.sink.link_changed(&slave, true);
        telemetry::note_slave_connected(&slave, true);

        let sink = Arc::clone(&self.sink);
        let data = self.data.clone();
        let stream_slave = slave;
        let slaves = Arc::clone(&self.slaves);
        tokio::spawn(async move {
            let reason = loop {
                let message = match inbound.message().await {
                    Ok(Some(message)) => message,
                    Ok(None) => break "slave_gone",
                    Err(_) => break "network",
                };
                let Some(body) = message.body else { continue };
                let handled = handle(&*sink, &data, &slaves, &stream_slave, body);
                if handled == Handled::Failed {
                    // No `Ack` and no new number: the slave sends the report again.
                    let _ = replies
                        .send(Err(Status::internal("report not applied")))
                        .await;
                    break "master_gone";
                }
                if let Handled::Ack(number) = handled {
                    let ack = MasterMessage {
                        body: Some(master_message::Body::Ack(Ack { report_no: number })),
                    };
                    if replies.send(Ok(ack)).await.is_err() {
                        break "slave_gone";
                    }
                }
            };
            let current = slaves
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(&stream_slave)
                .is_some_and(|state| state.generation == generation);
            if current {
                sink.link_changed(&stream_slave, false);
                telemetry::note_slave_connected(&stream_slave, false);
            }
            info!(slave = %stream_slave, reason, "slave link closed");
        });
        Ok(Response::new(Box::pin(ReceiverStream::new(outbound))))
    }
}

/// What a message needs next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Handled {
    /// Nothing to send.
    Nothing,
    /// Send an `Ack` with this number.
    Ack(u64),
    /// The sink failed: send no `Ack` and end the stream.
    Failed,
}

/// Handles one message.
fn handle(
    sink: &dyn MasterSink,
    data: &Path,
    slaves: &Mutex<HashMap<String, SlaveState>>,
    slave: &str,
    body: slave_message::Body,
) -> Handled {
    match body {
        slave_message::Body::Snapshot(snapshot) => {
            telemetry::note_slave_backlog(slave, snapshot.backlog);
            if let Some(state) = snapshot.snapshot {
                sink.apply_snapshot(slave, state);
            }
            telemetry::count_slave_report(slave, "snapshot", "applied");
            Handled::Nothing
        }
        slave_message::Body::Turn(turn) => {
            let number = turn.report_no;
            report(data, slaves, slave, number, "turn", || {
                sink.apply_turn(slave, turn)
            })
        }
        slave_message::Body::Lock(lock) => {
            let number = lock.report_no;
            report(data, slaves, slave, number, "lock", || {
                sink.apply_lock(slave, lock.locked)
            })
        }
        // A second `Hello` changes nothing.
        slave_message::Body::Hello(_) => Handled::Nothing,
    }
}

/// Applies a numbered report when it is above the last applied number. The check, the apply
/// and the new number happen under one lock, so two streams of one slave cannot apply a
/// report twice. A failed apply keeps the number.
fn report(
    data: &Path,
    slaves: &Mutex<HashMap<String, SlaveState>>,
    slave: &str,
    number: u64,
    kind: &'static str,
    apply: impl FnOnce() -> Applied,
) -> Handled {
    let mut slaves = slaves.lock().unwrap_or_else(PoisonError::into_inner);
    let state = slaves.entry(slave.to_owned()).or_default();
    if number <= state.last_report {
        telemetry::count_slave_report(slave, kind, "duplicate");
        return Handled::Ack(state.last_report);
    }
    if apply() == Applied::Failed {
        warn!(slave, kind, report_no = number, "slave report not applied");
        return Handled::Failed;
    }
    state.last_report = number;
    if let Err(error) = write_last_report(data, slave, number) {
        warn!(slave, error = %error, "slave state not saved");
    }
    telemetry::count_slave_report(slave, kind, "applied");
    Handled::Ack(number)
}

/// The slave link service with the secret check in front of it.
pub(crate) type LinkService =
    tonic::service::interceptor::InterceptedService<SlaveLinkServer<SlaveLinkService>, Guard>;

/// The slave link service with the secret check in front of it. Tests call it in-process,
/// with no socket.
pub(crate) fn link_service(
    data: &Path,
    names: &[String],
    sink: Arc<dyn MasterSink>,
) -> LinkService {
    SlaveLinkServer::with_interceptor(
        SlaveLinkService::new(data, sink),
        Guard {
            data: data.to_path_buf(),
            names: names.to_vec(),
        },
    )
}

/// Serves the slave link on `listener` until the task ends. It serves `SlaveLink` only.
pub(crate) fn serve_on(
    listener: tokio::net::TcpListener,
    settings: &SlavesSettings,
    data: &Path,
    sink: Arc<dyn MasterSink>,
) -> JoinHandle<()> {
    let service = link_service(data, &settings.names, sink);
    tokio::spawn(async move {
        let result = tonic::transport::Server::builder()
            .http2_keepalive_interval(Some(KEEPALIVE_INTERVAL))
            .http2_keepalive_timeout(Some(KEEPALIVE_TIMEOUT))
            .add_service(service)
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await;
        if let Err(error) = result {
            warn!(error = %error, "slave link listener stopped");
        }
    })
}

/// Starts the slave link listener on `[slaves] listen`. Abort the handle to stop it.
pub(crate) fn serve_slave_link(
    settings: &SlavesSettings,
    data: &Path,
    sink: Arc<dyn MasterSink>,
) -> JoinHandle<()> {
    let settings = settings.clone();
    let data = data.to_path_buf();
    tokio::spawn(async move {
        let listener = match tokio::net::TcpListener::bind(&settings.listen).await {
            Ok(listener) => listener,
            Err(error) => {
                let port = settings
                    .listen
                    .parse::<std::net::SocketAddr>()
                    .map(|addr| addr.port())
                    .unwrap_or(0);
                crate::telemetry::log_listener_failed("slave_link", port, &error);
                return;
            }
        };
        info!(address = %settings.listen, "slave link listening");
        let _ = serve_on(listener, &settings, &data, sink).await;
    })
}

#[cfg(test)]
#[path = "master_tests.rs"]
mod master_tests;
