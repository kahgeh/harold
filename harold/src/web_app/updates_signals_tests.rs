//! The updates stream counts how long it stays open and why it closes. The tests run in
//! paused time and serve the real router over an in-memory pipe: no test opens a port.
//! The metrics are global, so a count is checked as a rise of at least one.

use std::cell::Cell;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::sync::watch;

use super::StreamSignals;
use crate::outbound::fake::CapturedLogs;
use crate::store::HaroldStore;
use crate::telemetry::test_support;
use crate::web_app::{AppState, router};

const LOGIN: &str = "me@example.com";

/// A listener that hands out one in-memory connection and then waits for ever.
struct OneConnection(Option<DuplexStream>);

impl axum::serve::Listener for OneConnection {
    type Io = DuplexStream;
    type Addr = ();

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.0.take() {
            Some(io) => (io, ()),
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        Ok(())
    }
}

/// An open updates stream: the client end of the pipe, and the sender of the shutdown signal.
struct Stream {
    client: DuplexStream,
    shutdown: watch::Sender<()>,
    directory: std::path::PathBuf,
}

/// Opens `GET /updates` as the page does and reads the head of the response.
async fn open_stream() -> Stream {
    let directory =
        std::env::temp_dir().join(format!("harold-updates-signals-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let store = Arc::new(HaroldStore::open(directory.join("store")).await.unwrap());
    let (shutdown, shutdown_rx) = watch::channel(());
    let state = AppState::for_tests(
        store,
        LOGIN,
        "BPublicKeyForTheTestPage",
        Arc::from(vec!["localhost".to_owned()]),
        shutdown_rx,
        None,
        Default::default(),
    );
    let app = router(state);
    let (mut client, server) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let _ = axum::serve(OneConnection(Some(server)), app).await;
    });
    let request = format!(
        "GET /updates HTTP/1.1\r\nhost: localhost\r\ntailscale-user-login: {LOGIN}\r\n\
         sec-fetch-site: same-origin\r\ndatastar-request: true\r\n\r\n"
    );
    client.write_all(request.as_bytes()).await.unwrap();
    let mut head = Vec::new();
    let mut buffer = [0u8; 4096];
    while !String::from_utf8_lossy(&head).contains("\r\n\r\n") {
        let read = client.read(&mut buffer).await.unwrap();
        assert!(read > 0, "the stream closed before its head");
        head.extend_from_slice(&buffer[..read]);
    }
    assert!(
        head.starts_with(b"HTTP/1.1 200"),
        "{}",
        String::from_utf8_lossy(&head)
    );
    Stream {
        client,
        shutdown,
        directory,
    }
}

fn closed(reason: &str, short: bool) -> u64 {
    test_support::exact_counter_value(
        "harold.web_app.updates.closed",
        &[
            ("reason", reason),
            ("short", if short { "true" } else { "false" }),
        ],
    )
    .unwrap_or(0)
}

/// Lets the tasks run, without moving the paused clock, until the log holds the close line.
async fn closed_line(logs: &CapturedLogs) -> String {
    for _ in 0..500 {
        if let Some(line) = logs
            .text()
            .lines()
            .find(|line| line.contains("app updates closed"))
        {
            return line.to_owned();
        }
        tokio::task::yield_now().await;
    }
    panic!("no close line in:\n{}", logs.text());
}

/// The whole seconds that the close line holds for `open_s`.
fn open_seconds(line: &str) -> u64 {
    let (_, after) = line.split_once("open_s=").expect("open_s in the line");
    after
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .expect("open_s is a number")
}

#[tokio::test(start_paused = true)]
async fn updates_stream_closed_by_client_records_client_gone_and_open_time() {
    test_support::install();
    let logs = CapturedLogs::start();
    let before = closed("client_gone", false);
    let stream = open_stream().await;
    tokio::time::advance(Duration::from_secs(90)).await;
    drop(stream.client);
    let line = closed_line(&logs).await;
    assert!(line.contains(r#"reason="client_gone""#), "{line}");
    // The open and the close are read in the same paused time: 90 s, and no more than
    // the 1 s that the tasks need to notice the close.
    let open = open_seconds(&line);
    assert!((90..=91).contains(&open), "{line}");
    // Other tests share the counter: a rise of at least one is the exact check possible.
    assert!(closed("client_gone", false) > before);
    let _ = std::fs::remove_dir_all(stream.directory);
}

#[tokio::test(start_paused = true)]
async fn short_stream_counts_short_true() {
    test_support::install();
    let logs = CapturedLogs::start();
    let before = closed("client_gone", true);
    let stream = open_stream().await;
    tokio::time::advance(Duration::from_secs(10)).await;
    drop(stream.client);
    let line = closed_line(&logs).await;
    assert!(line.contains(r#"reason="client_gone""#), "{line}");
    assert!((10..=11).contains(&open_seconds(&line)), "{line}");
    assert!(closed("client_gone", true) > before);
    let _ = std::fs::remove_dir_all(stream.directory);
}

#[tokio::test(start_paused = true)]
async fn shutdown_records_shutdown() {
    test_support::install();
    let logs = CapturedLogs::start();
    let before = closed("shutdown", true);
    let stream = open_stream().await;
    tokio::time::advance(Duration::from_secs(5)).await;
    stream.shutdown.send(()).unwrap();
    let line = closed_line(&logs).await;
    assert!(line.contains(r#"reason="shutdown""#), "{line}");
    assert!(closed("shutdown", true) > before);
    drop(stream.client);
    let _ = std::fs::remove_dir_all(stream.directory);
}

thread_local! {
    static OPEN: Cell<i64> = const { Cell::new(0) };
}

fn track(delta: i64) {
    OPEN.with(|open| open.set(open.get() + delta));
}

// The global open count is shared with every test that opens a stream, so it cannot be
// read exactly. This test gives the guard a counter of its own on this thread.
#[tokio::test(start_paused = true)]
async fn open_gauge_rises_and_falls() {
    test_support::install();
    let _logs = CapturedLogs::start();
    let first = StreamSignals::with_open_add(track);
    let mut second = StreamSignals::with_open_add(track);
    assert_eq!(OPEN.with(Cell::get), 2);
    second.ended("store_stopped");
    drop(second);
    assert_eq!(OPEN.with(Cell::get), 1);
    // A guard that drops with no reason, as a cancelled task does, still lowers the count.
    drop(first);
    assert_eq!(OPEN.with(Cell::get), 0);
}
