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
    /// A stream of the same page that was open before this one, held until the test ends.
    _held: Option<crate::web_app::OpenPage>,
    client: DuplexStream,
    shutdown: watch::Sender<()>,
    directory: std::path::PathBuf,
}

/// Opens `GET /updates` as the page does and reads the head of the response.
async fn open_stream() -> Stream {
    open_stream_with("", None).await
}

/// Opens `GET /updates` with `query` after the address (for example the page's signals), with
/// one stream of the page `held_page_id` already open.
async fn open_stream_with(query: &str, held_page_id: Option<&str>) -> Stream {
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
    let held = held_page_id.map(|id| state.pages.open_page(Some(id.to_owned())));
    let app = router(state);
    let (mut client, server) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let _ = axum::serve(OneConnection(Some(server)), app).await;
    });
    let request = format!(
        "GET /updates{query} HTTP/1.1\r\nhost: localhost\r\ntailscale-user-login: {LOGIN}\r\n\
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
        _held: held,
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
    // The open and the close are read in the same paused time.
    assert_eq!(open_seconds(&line), 90, "{line}");
    assert!(line.contains("short=false"), "{line}");
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
    assert_eq!(open_seconds(&line), 10, "{line}");
    assert!(line.contains("short=true"), "{line}");
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

#[test]
fn close_reason_maps_each_end_reason() {
    for (ended, reason) in [
        ("shutdown", "shutdown"),
        ("page_gone", "client_gone"),
        ("store_stopped", "server_end"),
        ("anything_else", "client_gone"),
    ] {
        assert_eq!(super::close_reason(ended), reason, "{ended}");
    }
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

/// The query Datastar adds for the signals `{"page_id":"<id>"}`.
const PAGE_QUERY: &str = "?datastar=%7B%22page_id%22%3A%22p-one%22%7D";

#[tokio::test(start_paused = true)]
async fn the_connect_and_close_lines_hold_the_page_id_and_the_streams_of_that_page() {
    test_support::install();
    let logs = CapturedLogs::start();
    let first = open_stream_with(PAGE_QUERY, None).await;
    let connected = line_with_text(&logs, "app page connected").await;
    assert!(connected.contains(r#"page_id="p-one""#), "{connected}");
    assert!(connected.contains("streams_for_page=1"), "{connected}");
    drop(first.client);
    let closed = line_with_text(&logs, "app page disconnected").await;
    assert!(closed.contains(r#"page_id="p-one""#), "{closed}");
    assert!(closed.contains("streams_for_page=1"), "{closed}");
}

#[tokio::test(start_paused = true)]
async fn a_second_stream_of_one_page_id_logs_two_streams_for_that_page() {
    test_support::install();
    let logs = CapturedLogs::start();
    let _second = open_stream_with(PAGE_QUERY, Some("p-one")).await;
    let connected = line_with_text(&logs, "app page connected").await;
    assert!(connected.contains("streams_for_page=2"), "{connected}");
}

#[tokio::test(start_paused = true)]
async fn a_stream_without_a_page_id_logs_an_empty_page_id_and_one_stream() {
    test_support::install();
    let logs = CapturedLogs::start();
    let _stream = open_stream().await;
    let connected = line_with_text(&logs, "app page connected").await;
    assert!(connected.contains(r#"page_id="""#), "{connected}");
    assert!(connected.contains("streams_for_page=1"), "{connected}");
}

async fn line_with_text(logs: &CapturedLogs, wanted: &str) -> String {
    for _ in 0..500 {
        if let Some(line) = logs.text().lines().find(|line| line.contains(wanted)) {
            return line.to_owned();
        }
        tokio::task::yield_now().await;
    }
    panic!("no line with {wanted} in:\n{}", logs.text());
}
