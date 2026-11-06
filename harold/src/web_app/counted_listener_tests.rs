//! The counted listener counts each connection once, when it closes. No test opens a port:
//! every connection is an in-memory pipe.

use std::time::Duration;

use axum::Router;
use axum::routing::get;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::sync::Mutex;

use super::counted_listener::{CountedListener, CountedStream};
use crate::telemetry::test_support;

/// The counters are global. Each test holds this lock, so no other test adds to them
/// while it compares the values before and after.
static ONE_AT_A_TIME: Mutex<()> = Mutex::const_new(());

fn connections(outcome: &str) -> u64 {
    test_support::exact_counter_value("harold.web_app.connections", &[("outcome", outcome)])
        .unwrap_or(0)
}

/// Waits until the counter for `outcome` is `expected`, or fails after two seconds.
async fn wait_for(outcome: &str, expected: u64) {
    for _ in 0..200 {
        if connections(outcome) == expected {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(connections(outcome), expected, "outcome {outcome}");
}

/// A listener that hands out in-memory connections one by one and then waits for ever.
struct PipeListener(Vec<DuplexStream>);

impl axum::serve::Listener for PipeListener {
    type Io = DuplexStream;
    type Addr = ();

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.0.pop() {
            Some(io) => (io, ()),
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        Ok(())
    }
}

#[tokio::test]
async fn connection_closed_with_no_bytes_counts_no_request() {
    let _lock = ONE_AT_A_TIME.lock().await;
    let (client, server) = tokio::io::duplex(1024);
    let (requests_before, none_before) = (connections("requests"), connections("no_request"));
    let mut stream = CountedStream::new(server);
    drop(client);
    let mut buffer = [0u8; 8];
    assert_eq!(stream.read(&mut buffer).await.unwrap(), 0);
    drop(stream);
    assert_eq!(connections("no_request"), none_before + 1);
    assert_eq!(connections("requests"), requests_before);
}

#[tokio::test]
async fn connection_with_one_request_counts_requests() {
    let _lock = ONE_AT_A_TIME.lock().await;
    let (mut client, server) = tokio::io::duplex(1024);
    let (requests_before, none_before) = (connections("requests"), connections("no_request"));
    let mut stream = CountedStream::new(server);
    client.write_all(b"GET / HTTP/1.1\r\n\r\n").await.unwrap();
    let mut buffer = [0u8; 64];
    assert!(stream.read(&mut buffer).await.unwrap() > 0);
    drop(stream);
    assert_eq!(connections("requests"), requests_before + 1);
    assert_eq!(connections("no_request"), none_before);
}

#[tokio::test]
async fn connection_with_several_reads_counts_requests_once() {
    let _lock = ONE_AT_A_TIME.lock().await;
    let (mut client, server) = tokio::io::duplex(1024);
    let (requests_before, none_before) = (connections("requests"), connections("no_request"));
    let mut stream = CountedStream::new(server);
    let mut buffer = [0u8; 4];
    for _ in 0..3 {
        client.write_all(b"abcd").await.unwrap();
        assert_eq!(stream.read(&mut buffer).await.unwrap(), 4);
    }
    drop(client);
    assert_eq!(stream.read(&mut buffer).await.unwrap(), 0);
    drop(stream);
    assert_eq!(connections("requests"), requests_before + 1);
    assert_eq!(connections("no_request"), none_before);
}

#[tokio::test]
async fn writes_pass_through_and_do_not_count_as_a_request() {
    let _lock = ONE_AT_A_TIME.lock().await;
    let (mut client, server) = tokio::io::duplex(1024);
    let (requests_before, none_before) = (connections("requests"), connections("no_request"));
    let mut stream = CountedStream::new(server);
    stream.write_all(b"hello").await.unwrap();
    let mut buffer = [0u8; 5];
    client.read_exact(&mut buffer).await.unwrap();
    assert_eq!(&buffer, b"hello");
    drop(stream);
    assert_eq!(connections("no_request"), none_before + 1);
    assert_eq!(connections("requests"), requests_before);
}

#[tokio::test]
async fn served_connections_count_by_whether_they_sent_a_request() {
    let _lock = ONE_AT_A_TIME.lock().await;
    let (requests_before, none_before) = (connections("requests"), connections("no_request"));
    // `pop` hands out the last connection first: the one with no request comes second.
    let (mut idle_client, idle_server) = tokio::io::duplex(64 * 1024);
    let (mut client, server) = tokio::io::duplex(64 * 1024);
    let app = Router::new().route("/", get(|| async { "ok" }));
    let serving = tokio::spawn(async move {
        let listener = CountedListener::new(PipeListener(vec![idle_server, server]));
        let _ = axum::serve(listener, app).await;
    });

    client
        .write_all(b"GET / HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut reply = Vec::new();
    client.read_to_end(&mut reply).await.unwrap();
    assert!(String::from_utf8_lossy(&reply).starts_with("HTTP/1.1 200"));
    wait_for("requests", requests_before + 1).await;
    assert_eq!(connections("no_request"), none_before);

    idle_client.shutdown().await.unwrap();
    let mut rest = Vec::new();
    idle_client.read_to_end(&mut rest).await.unwrap();
    wait_for("no_request", none_before + 1).await;
    assert_eq!(connections("requests"), requests_before + 1);
    serving.abort();
}
