use std::path::Path;
use std::sync::{Arc, Mutex};

use harold_api::harold::AgentStateSnapshot;
use harold_api::harold::slave::v1::slave_link_client::SlaveLinkClient;
use harold_api::harold::slave::v1::{
    Hello, Lock, MasterMessage, SlaveMessage, Snapshot, Turn, master_message, slave_message,
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::Code;
use tonic::Streaming;

use std::time::Duration;

use super::*;
use crate::slave::pairing::{master_secret_path, new_secret, write_secret};

const NAME: &str = "omarchy";

/// What the fake sink saw, in order.
#[derive(Default)]
struct FakeSink {
    events: Mutex<Vec<String>>,
}

impl FakeSink {
    fn events(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }
}

impl MasterSink for FakeSink {
    fn apply_snapshot(&self, slave: &str, _snapshot: AgentStateSnapshot) {
        self.events
            .lock()
            .unwrap()
            .push(format!("snapshot {slave}"));
    }
    fn apply_turn(&self, slave: &str, turn: Turn) -> bool {
        self.events
            .lock()
            .unwrap()
            .push(format!("turn {slave} {}", turn.report_no));
        true
    }
    fn apply_lock(&self, slave: &str, locked: bool) {
        self.events
            .lock()
            .unwrap()
            .push(format!("lock {slave} {locked}"));
    }
    fn link_changed(&self, slave: &str, up: bool) {
        self.events
            .lock()
            .unwrap()
            .push(format!("link {slave} {up}"));
    }
}

/// The master service, called in-process: no port and no socket.
struct Running {
    service: LinkService,
}

fn settings(names: &[&str]) -> SlavesSettings {
    SlavesSettings {
        listen: "127.0.0.1:0".into(),
        names: names.iter().map(|name| (*name).to_owned()).collect(),
    }
}

fn start(data: &Path, names: &[&str], sink: Arc<FakeSink>) -> Running {
    let names: Vec<String> = names.iter().map(|name| (*name).to_owned()).collect();
    Running {
        service: link_service(data, &names, sink),
    }
}

struct Data(std::path::PathBuf);

impl Data {
    fn path(&self) -> &Path {
        &self.0
    }
}

/// Fails when `path` resolves outside the temp folder, for example into an installed Harold
/// (`~/bin/harold`) or `~/Library`.
fn assert_under_temp(path: &Path) {
    let temp = std::env::temp_dir().canonicalize().unwrap();
    let resolved = path.canonicalize().unwrap();
    assert!(
        resolved.starts_with(&temp),
        "{} is outside {}",
        resolved.display(),
        temp.display()
    );
}

fn data() -> Data {
    let dir = std::env::temp_dir().join(format!("harold-master-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    assert_under_temp(&dir);
    Data(dir)
}

fn paired(data: &Path) -> String {
    let secret = new_secret();
    write_secret(&master_secret_path(data, NAME), &secret).unwrap();
    secret
}

fn hello(name: &str, protocol: u32) -> SlaveMessage {
    SlaveMessage {
        body: Some(slave_message::Body::Hello(Hello {
            name: name.into(),
            version: "0.9.0".into(),
            last_known_report: 0,
            protocol,
        })),
    }
}

fn turn(report_no: u64) -> SlaveMessage {
    SlaveMessage {
        body: Some(slave_message::Body::Turn(Turn {
            report_no,
            pane_id: "block:1".into(),
            ..Turn::default()
        })),
    }
}

struct Link {
    send: mpsc::Sender<SlaveMessage>,
    replies: Streaming<MasterMessage>,
}

impl Link {
    async fn next(&mut self) -> master_message::Body {
        self.replies
            .message()
            .await
            .unwrap()
            .expect("a reply")
            .body
            .expect("a body")
    }
}

/// Opens a link. `first` goes into the stream before the call, so the master can read it.
async fn open(
    running: &Running,
    secret: Option<&str>,
    name: Option<&str>,
    first: SlaveMessage,
) -> Result<Link, tonic::Status> {
    let (send, receive) = mpsc::channel(8);
    send.send(first).await.unwrap();
    let mut request = tonic::Request::new(ReceiverStream::new(receive));
    if let Some(secret) = secret {
        request
            .metadata_mut()
            .insert(SECRET_HEADER, secret.parse().unwrap());
    }
    if let Some(name) = name {
        request
            .metadata_mut()
            .insert(NAME_HEADER, name.parse().unwrap());
    }
    let mut client = SlaveLinkClient::new(running.service.clone());
    let replies = client.open(request).await?.into_inner();
    Ok(Link { send, replies })
}

#[tokio::test]
async fn wrong_secret_is_refused_before_a_message() {
    let data = data();
    paired(data.path());
    let sink = Arc::new(FakeSink::default());
    let running = start(data.path(), &[NAME], sink.clone());
    let refused = open(&running, Some("not-the-secret"), Some(NAME), hello(NAME, 1)).await;
    assert_eq!(refused.err().unwrap().code(), Code::Unauthenticated);
    assert!(sink.events().is_empty());
}

#[tokio::test]
async fn unknown_name_is_refused() {
    let data = data();
    let secret = paired(data.path());
    let sink = Arc::new(FakeSink::default());
    let running = start(data.path(), &[NAME], sink.clone());
    for name in ["stranger", "../omarchy", ""] {
        let refused = open(&running, Some(&secret), Some(name), hello(name, 1)).await;
        assert_eq!(
            refused.err().unwrap().code(),
            Code::Unauthenticated,
            "{name}"
        );
    }
    assert!(sink.events().is_empty());
}

#[tokio::test]
async fn no_secret_is_refused() {
    let data = data();
    paired(data.path());
    let sink = Arc::new(FakeSink::default());
    let running = start(data.path(), &[NAME], sink.clone());
    let refused = open(&running, None, Some(NAME), hello(NAME, 1)).await;
    assert_eq!(refused.err().unwrap().code(), Code::Unauthenticated);
    assert!(sink.events().is_empty());
}

#[tokio::test]
async fn turn_is_applied_once_and_acked() {
    let data = data();
    let secret = paired(data.path());
    let sink = Arc::new(FakeSink::default());
    let running = start(data.path(), &[NAME], sink.clone());
    let mut link = open(&running, Some(&secret), Some(NAME), hello(NAME, 1))
        .await
        .unwrap();
    assert!(matches!(
        link.next().await,
        master_message::Body::Welcome(welcome) if welcome.last_report == 0
    ));
    link.send.send(turn(1)).await.unwrap();
    assert!(matches!(
        link.next().await,
        master_message::Body::Ack(ack) if ack.report_no == 1
    ));
    assert_eq!(
        sink.events(),
        [format!("link {NAME} true"), format!("turn {NAME} 1")]
    );
    let state =
        std::fs::read_to_string(data.path().join("slaves").join(format!("{NAME}.json"))).unwrap();
    assert!(state.contains('1'), "{state}");
}

#[tokio::test]
async fn repeat_turn_is_duplicate_and_acked() {
    let data = data();
    let secret = paired(data.path());
    let sink = Arc::new(FakeSink::default());
    let running = start(data.path(), &[NAME], sink.clone());
    let mut link = open(&running, Some(&secret), Some(NAME), hello(NAME, 1))
        .await
        .unwrap();
    link.next().await;
    for _ in 0..2 {
        link.send.send(turn(1)).await.unwrap();
        assert!(matches!(
            link.next().await,
            master_message::Body::Ack(ack) if ack.report_no == 1
        ));
    }
    link.send
        .send(SlaveMessage {
            body: Some(slave_message::Body::Lock(Lock {
                report_no: 2,
                locked: true,
            })),
        })
        .await
        .unwrap();
    assert!(matches!(
        link.next().await,
        master_message::Body::Ack(ack) if ack.report_no == 2
    ));
    let turns = sink
        .events()
        .iter()
        .filter(|event| event.starts_with("turn"))
        .count();
    assert_eq!(turns, 1);
    assert!(sink.events().contains(&format!("lock {NAME} true")));
}

#[tokio::test]
async fn welcome_holds_the_last_applied_number_after_restart() {
    let data = data();
    let secret = paired(data.path());
    {
        let running = start(data.path(), &[NAME], Arc::new(FakeSink::default()));
        let mut link = open(&running, Some(&secret), Some(NAME), hello(NAME, 1))
            .await
            .unwrap();
        link.next().await;
        link.send.send(turn(5)).await.unwrap();
        link.next().await;
    }
    let running = start(data.path(), &[NAME], Arc::new(FakeSink::default()));
    let mut link = open(&running, Some(&secret), Some(NAME), hello(NAME, 1))
        .await
        .unwrap();
    assert!(matches!(
        link.next().await,
        master_message::Body::Welcome(welcome) if welcome.last_report == 5
    ));
}

#[tokio::test]
async fn unknown_version_is_refused() {
    let data = data();
    let secret = paired(data.path());
    let sink = Arc::new(FakeSink::default());
    let running = start(data.path(), &[NAME], sink.clone());
    let refused = open(&running, Some(&secret), Some(NAME), hello(NAME, 99)).await;
    assert_eq!(refused.err().unwrap().code(), Code::FailedPrecondition);
    assert!(sink.events().is_empty());
}

#[tokio::test]
async fn snapshot_reaches_the_sink_and_sets_the_backlog_gauge() {
    let data = data();
    let secret = paired(data.path());
    let sink = Arc::new(FakeSink::default());
    let running = start(data.path(), &[NAME], sink.clone());
    let mut link = open(&running, Some(&secret), Some(NAME), hello(NAME, 1))
        .await
        .unwrap();
    link.next().await;
    link.send
        .send(SlaveMessage {
            body: Some(slave_message::Body::Snapshot(Snapshot {
                snapshot: Some(AgentStateSnapshot::default()),
                backlog: 3,
            })),
        })
        .await
        .unwrap();
    // A turn after the snapshot is acked only when the snapshot was read first.
    link.send.send(turn(1)).await.unwrap();
    link.next().await;
    assert!(sink.events().contains(&format!("snapshot {NAME}")));
    assert_eq!(
        crate::telemetry::test_support::gauge_value(
            "harold.slave_link.backlog",
            &[("slave", NAME)]
        ),
        Some(3)
    );
}

#[test]
fn the_data_folder_of_a_test_is_under_the_temp_folder() {
    let data = data();
    assert_under_temp(data.path());
    assert!(!data.path().to_string_lossy().contains("/bin/harold"));
    assert!(!data.path().to_string_lossy().contains("/Library/"));
    let outside = std::panic::catch_unwind(|| assert_under_temp(Path::new("/")));
    assert!(outside.is_err());
}

#[ignore = "opens a port: runs in CI only"]
#[tokio::test]
async fn serve_slave_link_listens_on_the_configured_address_and_only_for_the_link() {
    let data = data();
    paired(data.path());
    let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = free.local_addr().unwrap();
    drop(free);
    let mut slaves = settings(&[NAME]);
    slaves.listen = address.to_string();
    let handle = serve_slave_link(&slaves, data.path(), Arc::new(FakeSink::default()));
    let mut client = loop {
        match SlaveLinkClient::connect(format!("http://{address}")).await {
            Ok(client) => break client,
            Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    };
    let (_send, receive) = mpsc::channel::<SlaveMessage>(1);
    let refused = client
        .open(tonic::Request::new(ReceiverStream::new(receive)))
        .await;
    assert_eq!(refused.err().unwrap().code(), Code::Unauthenticated);
    // Another service on the same address is not there.
    let mut harold =
        harold_api::harold::harold_client::HaroldClient::connect(format!("http://{address}"))
            .await
            .unwrap();
    let missing = harold
        .set_tts_muted(harold_api::harold::SetTtsMutedRequest::default())
        .await
        .unwrap_err();
    assert_eq!(missing.code(), Code::Unimplemented);
    handle.abort();
}
