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
/// How long a new stream may take to send `Hello`.
const HELLO_WAIT: Duration = Duration::from_secs(10);

/// Where the master puts what a slave reports.
pub(crate) trait MasterSink: Send + Sync + 'static {
    fn apply_snapshot(&self, slave: &str, snapshot: AgentStateSnapshot);
    /// Applies a turn. Returns false when the sink did not take it.
    fn apply_turn(&self, slave: &str, turn: Turn) -> bool;
    fn apply_lock(&self, slave: &str, locked: bool);
    fn link_changed(&self, slave: &str, up: bool);
}

/// A sink that takes every report and keeps none. The master registry replaces it.
pub(crate) struct DiscardingSink;

impl MasterSink for DiscardingSink {
    fn apply_snapshot(&self, _slave: &str, _snapshot: AgentStateSnapshot) {}
    fn apply_turn(&self, _slave: &str, _turn: Turn) -> bool {
        false
    }
    fn apply_lock(&self, _slave: &str, _locked: bool) {}
    fn link_changed(&self, _slave: &str, _up: bool) {}
}

/// The name of the slave that passed the secret check. The interceptor sets it.
#[derive(Clone)]
struct AuthenticatedSlave(String);

type Replies = Pin<Box<dyn Stream<Item = Result<MasterMessage, Status>> + Send>>;

pub(crate) struct SlaveLinkService {
    data: PathBuf,
    sink: Arc<dyn MasterSink>,
    /// The newest stream of each slave, so a stream that a newer one replaced stays quiet
    /// when it ends.
    generations: Arc<Mutex<HashMap<String, u64>>>,
}

impl SlaveLinkService {
    pub(crate) fn new(data: &Path, sink: Arc<dyn MasterSink>) -> Self {
        Self {
            data: data.to_path_buf(),
            sink,
            generations: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn next_generation(&self, slave: &str) -> u64 {
        let mut generations = self
            .generations
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let generation = generations.entry(slave.to_owned()).or_insert(0);
        *generation += 1;
        *generation
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
        let mut last_report = read_last_report(&self.data, &slave);
        let (replies, outbound) = mpsc::channel(16);
        let welcome = MasterMessage {
            body: Some(master_message::Body::Welcome(Welcome { last_report })),
        };
        replies
            .send(Ok(welcome))
            .await
            .map_err(|_| Status::cancelled("closed"))?;
        info!(slave = %slave, welcome = last_report, version = %hello.version, "slave link opened");
        let generation = self.next_generation(&slave);
        self.sink.link_changed(&slave, true);
        telemetry::note_slave_connected(&slave, true);

        let sink = Arc::clone(&self.sink);
        let data = self.data.clone();
        let stream_slave = slave;
        let generations = Arc::clone(&self.generations);
        tokio::spawn(async move {
            let reason = loop {
                let message = match inbound.message().await {
                    Ok(Some(message)) => message,
                    Ok(None) => break "slave_gone",
                    Err(_) => break "network",
                };
                let Some(body) = message.body else { continue };
                let acked = handle(&*sink, &data, &stream_slave, &mut last_report, body);
                if let Some(number) = acked {
                    let ack = MasterMessage {
                        body: Some(master_message::Body::Ack(Ack { report_no: number })),
                    };
                    if replies.send(Ok(ack)).await.is_err() {
                        break "slave_gone";
                    }
                }
            };
            let current = generations
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(&stream_slave)
                == Some(&generation);
            if current {
                sink.link_changed(&stream_slave, false);
                telemetry::note_slave_connected(&stream_slave, false);
            }
            info!(slave = %stream_slave, reason, "slave link closed");
        });
        Ok(Response::new(Box::pin(ReceiverStream::new(outbound))))
    }
}

/// Handles one message. Returns the number to acknowledge, if the message is a report that
/// takes an `Ack`.
fn handle(
    sink: &dyn MasterSink,
    data: &Path,
    slave: &str,
    last_report: &mut u64,
    body: slave_message::Body,
) -> Option<u64> {
    match body {
        slave_message::Body::Snapshot(snapshot) => {
            telemetry::note_slave_backlog(slave, snapshot.backlog);
            if let Some(state) = snapshot.snapshot {
                sink.apply_snapshot(slave, state);
            }
            telemetry::count_slave_report(slave, "snapshot", "applied");
            None
        }
        slave_message::Body::Turn(turn) => {
            let number = turn.report_no;
            report(sink, data, slave, last_report, number, "turn", |sink| {
                sink.apply_turn(slave, turn)
            })
        }
        slave_message::Body::Lock(lock) => {
            let number = lock.report_no;
            report(sink, data, slave, last_report, number, "lock", |sink| {
                sink.apply_lock(slave, lock.locked);
                true
            })
        }
        // A second `Hello` changes nothing.
        slave_message::Body::Hello(_) => None,
    }
}

/// Applies a numbered report when it is above the last applied number, and returns the
/// number the master acknowledges: the last applied number.
fn report(
    sink: &dyn MasterSink,
    data: &Path,
    slave: &str,
    last_report: &mut u64,
    number: u64,
    kind: &'static str,
    apply: impl FnOnce(&dyn MasterSink) -> bool,
) -> Option<u64> {
    if number <= *last_report {
        telemetry::count_slave_report(slave, kind, "duplicate");
        return Some(*last_report);
    }
    let taken = apply(sink);
    *last_report = number;
    if let Err(error) = write_last_report(data, slave, number) {
        warn!(slave, error = %error, "slave state not saved");
    }
    telemetry::count_slave_report(slave, kind, if taken { "applied" } else { "duplicate" });
    Some(number)
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
                warn!(listen = %settings.listen, error = %error, "slave link listener not started");
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
