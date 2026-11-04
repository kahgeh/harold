//! Tests of the slave side. The master runs in the same process, as a service that tests
//! call with no port and no socket.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use harold_api::harold::AgentStateSnapshot;
use harold_api::harold::slave::v1::slave_link_client::SlaveLinkClient;
use harold_api::harold::slave::v1::{Turn, slave_message};
use tokio::sync::watch;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;

use super::*;
use crate::slave::master::{Applied, LinkService, MasterSink, link_service};
use crate::slave::pairing::{master_secret_path, new_secret, write_secret};

const NAME: &str = "omarchy";

/// Short waits, so a test with a reconnect takes milliseconds.
fn quick() -> Timing {
    Timing {
        initial: Duration::from_millis(20),
        max: Duration::from_millis(80),
        stable: Duration::from_millis(500),
        trim_every: Duration::from_secs(3600),
    }
}

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("harold-slave-{label}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let temp = std::env::temp_dir().canonicalize().unwrap();
    assert!(
        dir.canonicalize().unwrap().starts_with(&temp),
        "{} is outside the temp folder",
        dir.display()
    );
    dir
}

/// What the master sink saw.
#[derive(Default)]
struct Sink {
    events: Mutex<Vec<String>>,
    turns: Mutex<Vec<Turn>>,
    locks: Mutex<Vec<bool>>,
    fail: AtomicBool,
}

impl Sink {
    fn events(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }

    fn turn_numbers(&self) -> Vec<u64> {
        self.turns
            .lock()
            .unwrap()
            .iter()
            .map(|t| t.report_no)
            .collect()
    }

    fn sessions(&self) -> Vec<String> {
        self.turns
            .lock()
            .unwrap()
            .iter()
            .map(|t| t.session_id.clone())
            .collect()
    }

    fn outcome(&self) -> Applied {
        if self.fail.load(Ordering::SeqCst) {
            Applied::Failed
        } else {
            Applied::Applied
        }
    }
}

impl MasterSink for Sink {
    fn apply_snapshot(&self, _slave: &str, _snapshot: AgentStateSnapshot) {
        self.events.lock().unwrap().push("snapshot".into());
    }
    fn apply_turn(&self, _slave: &str, turn: Turn) -> Applied {
        let outcome = self.outcome();
        if outcome == Applied::Applied {
            self.turns.lock().unwrap().push(turn);
        }
        outcome
    }
    fn apply_lock(&self, _slave: &str, locked: bool) -> Applied {
        let outcome = self.outcome();
        if outcome == Applied::Applied {
            self.locks.lock().unwrap().push(locked);
        }
        outcome
    }
    fn link_changed(&self, _slave: &str, up: bool) {
        self.events.lock().unwrap().push(format!("link {up}"));
    }
}

/// One master in the same process: a data folder, a sink and the service.
struct Master {
    data: PathBuf,
    sink: Arc<Sink>,
    service: LinkService,
}

fn master(secret: &str) -> Master {
    let data = temp_dir("master");
    write_secret(&master_secret_path(&data, NAME), secret).unwrap();
    let sink = Arc::new(Sink::default());
    let service = link_service(&data, &[NAME.to_owned()], sink.clone());
    Master {
        data,
        sink,
        service,
    }
}

/// What the test connector saw leave the slave.
#[derive(Default)]
struct Wire {
    sent: Mutex<Vec<SlaveMessage>>,
    headers: Mutex<Vec<(String, String)>>,
}

/// A connector to an in-process master. The test can switch the master, make the master
/// unreachable and cut the stream that is open.
struct TestConnector {
    service: Mutex<LinkService>,
    down: AtomicBool,
    opens: Mutex<Vec<Instant>>,
    failed_opens: AtomicUsize,
    cut: watch::Sender<u64>,
    wire: Arc<Wire>,
}

impl TestConnector {
    fn new(master: &Master) -> Arc<Self> {
        Arc::new(Self {
            service: Mutex::new(master.service.clone()),
            down: AtomicBool::new(false),
            opens: Mutex::new(Vec::new()),
            failed_opens: AtomicUsize::new(0),
            cut: watch::channel(0).0,
            wire: Arc::new(Wire::default()),
        })
    }

    fn use_master(&self, master: &Master) {
        *self.service.lock().unwrap() = master.service.clone();
    }

    fn set_down(&self, down: bool) {
        self.down.store(down, Ordering::SeqCst);
    }

    /// Ends the stream that is open, as a cut network does.
    fn cut(&self) {
        self.cut.send_modify(|n| *n += 1);
    }

    fn opens(&self) -> usize {
        self.opens.lock().unwrap().len()
    }
}

impl Connector for TestConnector {
    fn open(&self, request: Request<OutboundStream>) -> OpenFuture {
        self.opens.lock().unwrap().push(Instant::now());
        if self.down.load(Ordering::SeqCst) {
            self.failed_opens.fetch_add(1, Ordering::SeqCst);
            return Box::pin(async { Err(Status::unavailable("down")) });
        }
        let service = self.service.lock().unwrap().clone();
        let wire = Arc::clone(&self.wire);
        let mut cut = self.cut.subscribe();
        cut.borrow_and_update();
        Box::pin(async move {
            let mut headers = Vec::new();
            for key in [NAME_HEADER, SECRET_HEADER] {
                if let Some(value) = request.metadata().get(key) {
                    headers.push((key.to_owned(), value.to_str().unwrap().to_owned()));
                }
            }
            *wire.headers.lock().unwrap() = headers.clone();
            let metadata = request.metadata().clone();
            let mut inner = request.into_inner();
            let (tx, rx) = tokio::sync::mpsc::channel::<SlaveMessage>(64);
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        message = inner.next() => match message {
                            Some(message) => {
                                wire.sent.lock().unwrap().push(message.clone());
                                if tx.send(message).await.is_err() {
                                    break;
                                }
                            }
                            None => break,
                        },
                        _ = cut.changed() => break,
                    }
                }
            });
            let mut forwarded = Request::new(ReceiverStream::new(rx));
            *forwarded.metadata_mut() = metadata;
            SlaveLinkClient::new(service).open(forwarded).await
        })
    }
}

struct TestSource {
    snapshot: watch::Sender<AgentStateSnapshot>,
    lock: watch::Sender<bool>,
}

impl TestSource {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            snapshot: watch::channel(AgentStateSnapshot::default()).0,
            lock: watch::channel(false).0,
        })
    }
}

impl SlaveSource for Arc<TestSource> {
    fn snapshots(&self) -> watch::Receiver<AgentStateSnapshot> {
        self.snapshot.subscribe()
    }
    fn locks(&self) -> watch::Receiver<bool> {
        self.lock.subscribe()
    }
}

/// A slave data folder that holds the secret of the master.
fn slave_data(secret: &str) -> PathBuf {
    let data = temp_dir("slave");
    write_secret(&slave_secret_path(&data), secret).unwrap();
    data
}

async fn start(connector: &Arc<TestConnector>, data: &Path, source: &Arc<TestSource>) -> SlaveLink {
    SlaveLink::start(
        connector.clone(),
        NAME,
        "0.9.0",
        data,
        source.clone(),
        quick(),
    )
    .await
    .unwrap()
}

fn request(pane: &str, session: &str) -> TurnCompleteRequest {
    TurnCompleteRequest {
        pane_id: pane.into(),
        pane_label: "label".into(),
        last_user_prompt: "prompt".into(),
        assistant_message: "answer".into(),
        main_context: "ctx".into(),
        session_id: session.into(),
        transcript_path: "/home/someone/transcript.jsonl".into(),
        input_tokens: 11,
        output_tokens: 22,
        cache_read_tokens: 33,
        cache_write_tokens: 44,
        model: "model-x".into(),
        ..TurnCompleteRequest::default()
    }
}

async fn until(what: &str, mut done: impl FnMut() -> bool) {
    for _ in 0..600 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for: {what}");
}

#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Logs {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Logs {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
    fn has_line(&self, parts: &[&str]) -> bool {
        self.text()
            .lines()
            .any(|line| parts.iter().all(|part| line.contains(part)))
    }
}

/// Captures this thread's log lines; use it from a current-thread runtime.
fn capture_logs() -> (Logs, tracing::subscriber::DefaultGuard) {
    crate::test_logs::keep_second_dispatcher();
    let logs = Logs::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::INFO)
        .finish();
    (logs, tracing::subscriber::set_default(subscriber))
}

fn write_master_state(data: &Path, last_report: u64) {
    let path = data.join("slaves").join(format!("{NAME}.json"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, format!("{{\"last_report\":{last_report}}}")).unwrap();
}

#[tokio::test]
async fn turn_arrives_with_empty_transcript_path() {
    let secret = new_secret();
    let master = master(&secret);
    let connector = TestConnector::new(&master);
    let slave = start(&connector, &slave_data(&secret), &TestSource::new()).await;
    until("link up", || {
        master.sink.events().contains(&"link true".into())
    })
    .await;

    slave.report_turn(request("block:7", "s-1")).await;
    until("turn applied", || master.sink.sessions() == ["s-1"]).await;

    let turns = master.sink.turns.lock().unwrap().clone();
    assert_eq!(turns[0].transcript_path, "");
    assert_eq!(turns[0].pane_id, "block:7");
    assert_eq!(turns[0].session_id, "s-1");
    assert_eq!(turns[0].assistant_message, "answer");
    assert_eq!(turns[0].input_tokens, 11);
    assert_eq!(turns[0].cache_write_tokens, 44);
    assert_eq!(turns[0].model, "model-x");
}

#[tokio::test]
async fn secret_and_name_go_in_metadata_only() {
    let (logs, _guard) = capture_logs();
    let secret = new_secret();
    let master = master(&secret);
    let connector = TestConnector::new(&master);
    let slave = start(&connector, &slave_data(&secret), &TestSource::new()).await;
    slave.report_turn(request("block:1", "s")).await;
    until("turn applied", || master.sink.sessions() == ["s"]).await;

    let headers = connector.wire.headers.lock().unwrap().clone();
    assert!(headers.contains(&(NAME_HEADER.to_owned(), NAME.to_owned())));
    assert!(headers.contains(&(SECRET_HEADER.to_owned(), secret.clone())));
    for message in connector.wire.sent.lock().unwrap().iter() {
        assert!(!format!("{message:?}").contains(&secret));
    }
    assert!(!logs.text().contains(&secret));
    assert!(logs.has_line(&["slave link opened", "welcome=0", "version=0.9.0"]));
}

#[tokio::test]
async fn cut_link_keeps_reports_and_sends_only_those_after_welcome() {
    let secret = new_secret();
    let master = master(&secret);
    let connector = TestConnector::new(&master);
    let slave = start(&connector, &slave_data(&secret), &TestSource::new()).await;
    until("link up", || {
        master.sink.events().contains(&"link true".into())
    })
    .await;

    slave.report_turn(request("block:1", "a")).await;
    slave.report_turn(request("block:1", "b")).await;
    until("two turns", || master.sink.turn_numbers().len() == 2).await;
    let first = master.sink.turn_numbers();

    connector.set_down(true);
    connector.cut();
    until("link down", || {
        master.sink.events().contains(&"link false".into())
    })
    .await;
    slave.report_turn(request("block:1", "c")).await;
    slave.report_turn(request("block:1", "d")).await;
    assert_eq!(master.sink.turn_numbers(), first);

    connector.set_down(false);
    until("four turns", || master.sink.turn_numbers().len() == 4).await;
    let numbers = master.sink.turn_numbers();
    // Each number once, and the two kept reports went after the reconnect.
    let mut sorted = numbers.clone();
    sorted.dedup();
    assert_eq!(sorted, numbers);
    assert_eq!(&numbers[..2], &first[..]);
    assert!(numbers[2] > first[1] && numbers[3] > numbers[2]);
    let sessions: Vec<String> = master
        .sink
        .turns
        .lock()
        .unwrap()
        .iter()
        .map(|t| t.session_id.clone())
        .collect();
    assert_eq!(sessions, ["a", "b", "c", "d"]);
}

#[tokio::test]
async fn restart_with_unacked_reports_does_not_apply_a_report_twice() {
    // The master applied reports 1 and 2, and the slave died before it kept the Ack. The new
    // slave still holds both reports. The master's Welcome says it has them.
    let secret = new_secret();
    let master = master(&secret);
    let data = slave_data(&secret);
    let seed = || async {
        let backlog = ReportBacklog::open(&data.join(BACKLOG_FILE)).await.unwrap();
        for session in ["a", "b"] {
            let message = SlaveMessage {
                body: Some(slave_message::Body::Turn(Turn {
                    session_id: session.into(),
                    ..Turn::default()
                })),
            };
            backlog
                .append(ReportKind::Turn, message.encode_to_vec(), SystemTime::now())
                .await
                .unwrap();
        }
        // The rows came from an earlier process: they are old rows, not provisional.
        backlog.welcome_first(0).await.unwrap();
    };
    seed().await;
    let connector = TestConnector::new(&master);
    let slave = start(&connector, &data, &TestSource::new()).await;
    until("two turns", || master.sink.turn_numbers() == [1, 2]).await;
    until("acked", || slave.shared.backlog.len() == 0).await;
    slave.abort();
    drop(slave);

    // The same two reports are back in a new backlog, as when the Ack was lost.
    std::fs::remove_dir_all(&data).unwrap();
    std::fs::create_dir_all(&data).unwrap();
    write_secret(&slave_secret_path(&data), &secret).unwrap();
    seed().await;
    let slave = start(&connector, &data, &TestSource::new()).await;
    until("second link", || connector.opens() >= 2).await;
    slave.report_turn(request("block:1", "c")).await;
    until("third turn", || master.sink.turn_numbers().len() == 3).await;
    // The new slave sent nothing of the two kept reports. The master had 3 (the first
    // slave's Lock), so the new slave's Lock took 4 and the turn took 5.
    assert_eq!(master.sink.turn_numbers(), [1, 2, 5]);
    assert_eq!(master.sink.sessions(), ["a", "b", "c"]);
}

#[tokio::test]
async fn wiped_backlog_then_reconnect_numbers_above_welcome_and_next_turn_applies() {
    let (logs, _guard) = capture_logs();
    let secret = new_secret();
    let master = master(&secret);
    write_master_state(&master.data, 5);
    // The master is not reachable at first, so the slave starts with a wiped backlog and
    // takes a turn before any Welcome.
    let connector = TestConnector::new(&master);
    connector.set_down(true);
    let slave = start(&connector, &slave_data(&secret), &TestSource::new()).await;
    slave.report_turn(request("block:1", "early")).await;
    assert_eq!(slave.shared.backlog.len(), 1, "kept on disk at once");

    connector.set_down(false);
    until("early turn applied", || master.sink.sessions() == ["early"]).await;
    assert_eq!(master.sink.turn_numbers(), [6]);
    slave.report_turn(request("block:1", "next")).await;
    until("next turn applied", || {
        master.sink.sessions() == ["early", "next"]
    })
    .await;
    // The slave's Lock took number 7.
    assert_eq!(master.sink.turn_numbers(), [6, 8]);
    assert!(logs.has_line(&["report backlog behind", "own=1", "welcome=5"]));
}

#[tokio::test]
async fn failed_apply_keeps_the_report_and_reconnects_with_backoff() {
    let secret = new_secret();
    let master = master(&secret);
    master.sink.fail.store(true, Ordering::SeqCst);
    let connector = TestConnector::new(&master);
    let slave = start(&connector, &slave_data(&secret), &TestSource::new()).await;
    slave.report_turn(request("block:1", "x")).await;

    tokio::time::sleep(Duration::from_millis(600)).await;
    // Waits of 20, 40, 80, 80 ms: a few opens, not a tight loop.
    let opens = connector.opens();
    assert!((3..=12).contains(&opens), "opens: {opens}");
    let times = connector.opens.lock().unwrap().clone();
    let gaps: Vec<_> = times.windows(2).map(|w| w[1] - w[0]).collect();
    assert!(gaps[1] >= gaps[0], "{gaps:?}");
    assert!(
        gaps.iter().all(|gap| *gap >= Duration::from_millis(18)),
        "{gaps:?}"
    );
    assert!(master.sink.turn_numbers().is_empty());
    assert!(slave.shared.backlog.len() >= 1, "the report stays");

    master.sink.fail.store(false, Ordering::SeqCst);
    until("applied", || master.sink.sessions() == ["x"]).await;
    until("acked", || slave.shared.backlog.len() == 0).await;
}

#[tokio::test]
async fn backoff_doubles_to_the_maximum_and_resets_after_a_stable_link() {
    let secret = new_secret();
    let master = master(&secret);
    let connector = TestConnector::new(&master);
    connector.set_down(true);
    let _slave = start(&connector, &slave_data(&secret), &TestSource::new()).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let times = connector.opens.lock().unwrap().clone();
    let gaps: Vec<_> = times.windows(2).map(|w| w[1] - w[0]).collect();
    assert!(gaps.len() >= 4, "{gaps:?}");
    // 20, 40, 80, 80: the wait stops at the maximum.
    assert!(gaps[2] >= Duration::from_millis(75));
    assert!(
        gaps.iter().all(|gap| *gap < Duration::from_millis(250)),
        "{gaps:?}"
    );

    connector.set_down(false);
    until("link up", || {
        master.sink.events().contains(&"link true".into())
    })
    .await;
    // A stable link, then a cut: the next connect waits the first wait again.
    tokio::time::sleep(Duration::from_millis(600)).await;
    let before = connector.opens();
    let cut_at = Instant::now();
    connector.cut();
    until("reconnect", || connector.opens() > before).await;
    let reopened = connector.opens.lock().unwrap()[before];
    let gap = reopened - cut_at;
    // The first wait is 20 ms. A wait that had not reset would be 80 ms.
    assert!(gap < Duration::from_millis(70), "{gap:?}");
}

#[tokio::test]
async fn master_restart_without_state_file_applies_each_report_once() {
    let secret = new_secret();
    let first = master(&secret);
    let connector = TestConnector::new(&first);
    let slave = start(&connector, &slave_data(&secret), &TestSource::new()).await;
    slave.report_turn(request("block:1", "a")).await;
    until("first applied", || first.sink.sessions() == ["a"]).await;
    until("acked", || slave.shared.backlog.len() == 0).await;

    // The master comes back with a new data folder: no state file, Welcome 0.
    let second = master(&secret);
    connector.set_down(true);
    connector.cut();
    until("link down", || {
        first.sink.events().contains(&"link false".into())
    })
    .await;
    connector.use_master(&second);
    slave.report_turn(request("block:1", "b")).await;
    slave.report_turn(request("block:1", "c")).await;
    connector.set_down(false);
    until("two turns", || second.sink.sessions().len() == 2).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(first.sink.sessions(), ["a"]);
    assert_eq!(second.sink.sessions(), ["b", "c"]);
}

#[tokio::test]
async fn limit_drops_log_slave_report_dropped() {
    let (logs, _guard) = capture_logs();
    let secret = new_secret();
    let master = master(&secret);
    let data = slave_data(&secret);
    {
        let backlog = ReportBacklog::open(&data.join(BACKLOG_FILE)).await.unwrap();
        let day_and_more = SystemTime::now() - Duration::from_secs(25 * 3600);
        let message = SlaveMessage {
            body: Some(slave_message::Body::Turn(Turn::default())),
        }
        .encode_to_vec();
        backlog
            .append(ReportKind::Turn, message.clone(), day_and_more)
            .await
            .unwrap();
        for _ in 0..1002 {
            backlog
                .append(ReportKind::Turn, message.clone(), SystemTime::now())
                .await
                .unwrap();
        }
    }
    let connector = TestConnector::new(&master);
    connector.set_down(true);
    let _slave = start(&connector, &data, &TestSource::new()).await;
    until("too_old line", || {
        logs.has_line(&["slave report dropped", "count=1", "too_old"])
    })
    .await;
    assert!(logs.has_line(&["slave report dropped", "count=2", "too_many"]));
}

#[tokio::test]
async fn backlog_count_is_sent_in_snapshot() {
    let secret = new_secret();
    let master = master(&secret);
    let connector = TestConnector::new(&master);
    let source = TestSource::new();
    let slave = start(&connector, &slave_data(&secret), &source).await;
    until("link up", || {
        master.sink.events().contains(&"link true".into())
    })
    .await;
    until("first acks", || slave.shared.backlog.len() == 0).await;
    connector.set_down(true);
    connector.cut();
    until("link down", || {
        master.sink.events().contains(&"link false".into())
    })
    .await;
    slave.report_turn(request("block:1", "a")).await;
    slave.report_turn(request("block:1", "b")).await;
    until("two kept", || slave.shared.backlog.len() == 2).await;
    connector.set_down(false);
    // The newest Snapshot carries the count, and the count falls to 0 after the Acks.
    until("snapshot with 0", || {
        connector.wire.sent.lock().unwrap().iter().any(|message| {
            matches!(&message.body, Some(slave_message::Body::Snapshot(s)) if s.backlog == 0)
        }) && master.sink.sessions().len() == 2
    })
    .await;
    let counts: Vec<u64> = connector
        .wire
        .sent
        .lock()
        .unwrap()
        .iter()
        .filter_map(|message| match &message.body {
            Some(slave_message::Body::Snapshot(s)) => Some(s.backlog),
            _ => None,
        })
        .collect();
    assert!(counts.iter().any(|count| *count >= 2), "{counts:?}");
}

#[tokio::test]
async fn first_message_is_hello_and_snapshot_and_lock_follow_reports() {
    let secret = new_secret();
    let master = master(&secret);
    let connector = TestConnector::new(&master);
    let source = TestSource::new();
    source.lock.send_replace(true);
    let _slave = start(&connector, &slave_data(&secret), &source).await;
    until("lock applied", || {
        master.sink.locks.lock().unwrap().as_slice() == [true]
    })
    .await;
    until("snapshot", || {
        master.sink.events().contains(&"snapshot".into())
    })
    .await;
    {
        let sent = connector.wire.sent.lock().unwrap();
        assert!(
            matches!(&sent[0].body, Some(slave_message::Body::Hello(h)) if h.name == NAME && h.protocol == 1)
        );
    }
    source.lock.send_replace(false);
    until("unlock applied", || {
        master.sink.locks.lock().unwrap().as_slice() == [true, false]
    })
    .await;
}

#[tokio::test]
async fn slave_writes_only_its_own_data_folder() {
    let secret = new_secret();
    let master = master(&secret);
    let data = slave_data(&secret);
    let connector = TestConnector::new(&master);
    let slave = start(&connector, &data, &TestSource::new()).await;
    slave.report_turn(request("block:1", "a")).await;
    until("applied", || master.sink.sessions() == ["a"]).await;

    let mut names: Vec<String> = std::fs::read_dir(&data)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    // The secret and the backlog, and nothing of a master's store: no event store, no state
    // database, no device list, no questions tool, no chief session.
    for name in &names {
        assert!(
            name == "master.secret" || name.starts_with(BACKLOG_FILE),
            "unexpected file in the slave folder: {name}"
        );
    }
    assert!(names.iter().any(|name| name.starts_with(BACKLOG_FILE)));
    assert!(!master.data.join(BACKLOG_FILE).exists());
}

#[tokio::test]
async fn paired_slave_still_records_the_turn_and_sends_turn() {
    // After a first Welcome, a turn made while the master is away stays in the backlog and
    // goes up after the reconnect.
    let secret = new_secret();
    let master = master(&secret);
    let connector = TestConnector::new(&master);
    let slave = start(&connector, &slave_data(&secret), &TestSource::new()).await;
    until("link up", || {
        master.sink.events().contains(&"link true".into())
    })
    .await;
    connector.set_down(true);
    connector.cut();
    until("link down", || {
        master.sink.events().contains(&"link false".into())
    })
    .await;
    slave.report_turn(request("block:1", "a")).await;
    assert!(slave.shared.backlog.len() >= 1);
    connector.set_down(false);
    until("turn sent", || master.sink.sessions() == ["a"]).await;
}

/// A backlog folder whose last number is `rows`: that many kept `Lock` reports.
async fn seed_locks(data: &Path, rows: u64) {
    let backlog = ReportBacklog::open(&data.join(BACKLOG_FILE)).await.unwrap();
    let message = SlaveMessage {
        body: Some(slave_message::Body::Lock(Lock::default())),
    };
    for _ in 0..rows {
        backlog
            .append(ReportKind::Lock, message.encode_to_vec(), SystemTime::now())
            .await
            .unwrap();
    }
    // The rows came from an earlier process: they are old rows, not provisional.
    backlog.welcome_first(0).await.unwrap();
}

#[tokio::test]
async fn new_rows_are_renumbered_above_welcome_when_backlog_was_older() {
    let secret = new_secret();
    let master = master(&secret);
    write_master_state(&master.data, 5);
    let data = slave_data(&secret);
    // A restored older backlog: its last number is 2, below the master's 5.
    seed_locks(&data, 2).await;
    let connector = TestConnector::new(&master);
    connector.set_down(true);
    let slave = start(&connector, &data, &TestSource::new()).await;
    slave.report_turn(request("block:1", "early")).await;
    assert_eq!(slave.shared.backlog.last_number(), 3, "number 3 on disk");
    connector.set_down(false);
    until("early turn applied", || master.sink.sessions() == ["early"]).await;
    assert_eq!(master.sink.turn_numbers(), [6]);
}

#[tokio::test]
async fn new_rows_keep_numbers_when_welcome_is_below_start_last() {
    let secret = new_secret();
    let master = master(&secret);
    write_master_state(&master.data, 2);
    let data = slave_data(&secret);
    // Rows 1 to 4 are kept. The master has 1 and 2. Rows 3 and 4 are old rows it lacks.
    seed_locks(&data, 4).await;
    let connector = TestConnector::new(&master);
    connector.set_down(true);
    let slave = start(&connector, &data, &TestSource::new()).await;
    slave.report_turn(request("block:1", "early")).await;
    connector.set_down(false);
    until("early turn applied", || master.sink.sessions() == ["early"]).await;
    assert_eq!(master.sink.turn_numbers(), [5], "no renumbering");
}

#[tokio::test]
async fn a_crash_after_a_pre_welcome_write_with_the_master_ahead_keeps_the_row() {
    let secret = new_secret();
    let master = master(&secret);
    write_master_state(&master.data, 5);
    let data = slave_data(&secret);
    seed_locks(&data, 2).await;
    let connector = TestConnector::new(&master);
    connector.set_down(true);
    let slave = start(&connector, &data, &TestSource::new()).await;
    slave.report_turn(request("block:1", "early")).await;
    slave.abort();
    drop(slave);
    tokio::time::sleep(Duration::from_millis(100)).await;

    // The restarted process opens a backlog at 3. The row is still provisional.
    let slave = start(&connector, &data, &TestSource::new()).await;
    connector.set_down(false);
    until("early turn applied", || master.sink.sessions() == ["early"]).await;
    assert_eq!(master.sink.turn_numbers(), [6]);
    drop(slave);
}

#[tokio::test]
async fn a_wiped_backlog_with_a_crash_and_welcome_7_sends_the_row_as_8() {
    let secret = new_secret();
    let master = master(&secret);
    write_master_state(&master.data, 7);
    let data = slave_data(&secret);
    let connector = TestConnector::new(&master);
    connector.set_down(true);
    let slave = start(&connector, &data, &TestSource::new()).await;
    slave.report_turn(request("block:1", "early")).await;
    slave.abort();
    drop(slave);
    tokio::time::sleep(Duration::from_millis(100)).await;

    let _slave = start(&connector, &data, &TestSource::new()).await;
    connector.set_down(false);
    until("early turn applied", || master.sink.sessions() == ["early"]).await;
    assert_eq!(master.sink.turn_numbers(), [8]);
}

#[tokio::test]
async fn a_failed_welcome_first_sends_nothing_and_is_retried_on_the_next_welcome() {
    let secret = new_secret();
    let master = master(&secret);
    write_master_state(&master.data, 5);
    let data = slave_data(&secret);
    let connector = TestConnector::new(&master);
    connector.set_down(true);
    let slave = start(&connector, &data, &TestSource::new()).await;
    slave.report_turn(request("block:1", "early")).await;
    slave.shared.backlog.fail_welcome_first_once();
    connector.set_down(false);
    until("early turn applied", || master.sink.sessions() == ["early"]).await;
    // The first Welcome could not renumber, so nothing went out under the old number. The
    // retry on the next Welcome numbered the row above 5, and the master applied it once.
    assert_eq!(master.sink.turn_numbers(), [6]);
    assert!(connector.opens() >= 2);
}

#[tokio::test]
async fn a_second_welcome_in_the_same_process_only_acks() {
    let secret = new_secret();
    let master = master(&secret);
    let connector = TestConnector::new(&master);
    let slave = start(&connector, &slave_data(&secret), &TestSource::new()).await;
    slave.report_turn(request("block:1", "a")).await;
    until("a applied", || master.sink.sessions() == ["a"]).await;
    until("acked", || slave.shared.backlog.len() == 0).await;
    connector.cut();
    slave.report_turn(request("block:1", "b")).await;
    until("b applied", || master.sink.sessions() == ["a", "b"]).await;
    until("acked again", || slave.shared.backlog.len() == 0).await;
    let numbers = master.sink.turn_numbers();
    assert!(numbers[0] < numbers[1]);
}

#[tokio::test]
async fn reports_before_first_welcome_are_kept_on_disk_and_trimmed() {
    let (logs, _guard) = capture_logs();
    let secret = new_secret();
    let master = master(&secret);
    let connector = TestConnector::new(&master);
    connector.set_down(true);
    let slave = start(&connector, &slave_data(&secret), &TestSource::new()).await;
    for _ in 0..1003 {
        slave.report_turn(request("block:1", "x")).await;
    }
    // On disk and counted, and the limit of 1,000 holds before any Welcome.
    until("trimmed", || {
        logs.has_line(&["slave report dropped", "too_many"])
    })
    .await;
    assert!(slave.shared.backlog.len() <= 1000 + 5);
    assert!(slave.shared.backlog.len() >= 1000);
}

#[tokio::test]
async fn a_crash_before_welcome_keeps_the_reports() {
    let secret = new_secret();
    let master = master(&secret);
    let data = slave_data(&secret);
    let connector = TestConnector::new(&master);
    connector.set_down(true);
    let slave = start(&connector, &data, &TestSource::new()).await;
    slave.report_turn(request("block:1", "a")).await;
    slave.report_turn(request("block:1", "b")).await;
    slave.abort();
    drop(slave);
    tokio::time::sleep(Duration::from_millis(100)).await;

    // A new process opens the same folder and still has both reports, then sends them.
    let reopened = ReportBacklog::open(&data.join(BACKLOG_FILE)).await.unwrap();
    assert_eq!(reopened.after(0).await.unwrap().len(), 2);
    drop(reopened);
    connector.set_down(false);
    let _slave = start(&connector, &data, &TestSource::new()).await;
    until("both applied", || master.sink.sessions() == ["a", "b"]).await;
}

#[tokio::test]
async fn hello_carries_the_last_number_the_master_is_known_to_have() {
    let secret = new_secret();
    let master = master(&secret);
    write_master_state(&master.data, 5);
    let connector = TestConnector::new(&master);
    let slave = start(&connector, &slave_data(&secret), &TestSource::new()).await;
    slave.report_turn(request("block:1", "a")).await;
    until("applied", || master.sink.sessions() == ["a"]).await;
    until("acked", || slave.shared.backlog.len() == 0).await;
    let acked = slave.shared.known.load(Ordering::SeqCst);
    assert!(acked > 5);
    connector.cut();
    until("second hello", || hellos(&connector).len() >= 2).await;
    let hellos = hellos(&connector);
    assert_eq!(hellos[0], 0);
    assert_eq!(hellos[1], acked);
}

fn hellos(connector: &TestConnector) -> Vec<u64> {
    connector
        .wire
        .sent
        .lock()
        .unwrap()
        .iter()
        .filter_map(|message| match &message.body {
            Some(slave_message::Body::Hello(hello)) => Some(hello.last_known_report),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_failed_connect_warns_once_with_the_reason_and_never_the_secret() {
    let (logs, _guard) = capture_logs();
    // No secret file in the slave folder.
    let master_a = master(&new_secret());
    let connector = TestConnector::new(&master_a);
    let bare = temp_dir("slave-bare");
    let _slave = start(&connector, &bare, &TestSource::new()).await;
    until("no_secret", || {
        logs.has_line(&["slave link not opened", "no_secret"])
    })
    .await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    let count = |reason: &str| {
        logs.text()
            .lines()
            .filter(|line| line.contains("slave link not opened") && line.contains(reason))
            .count()
    };
    assert_eq!(count("no_secret"), 1, "once per start");

    // A wrong secret: the master refuses it.
    let wrong = new_secret();
    let _slave = start(&connector, &slave_data(&wrong), &TestSource::new()).await;
    until("refused", || {
        logs.has_line(&["slave link not opened", "refused"])
    })
    .await;
    assert!(!logs.text().contains(&wrong));
}
