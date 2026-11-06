//! The counted listener counts each connection once, when it closes. Almost every connection
//! is an in-memory pipe. One ignored test opens a loopback port: it runs only in the container
//! test set and in CI. Most tests use a local recorder, so no other test can change the values
//! they check. One test checks the link to the global counter.

use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use axum::Router;
use axum::routing::get;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

use super::counted_listener::{CountedListener, CountedStream, Recorder};
use crate::telemetry::test_support;

/// The outcomes that one test records.
type Seen = Arc<StdMutex<Vec<&'static str>>>;

fn recorder() -> (Seen, Recorder) {
    let seen: Seen = Default::default();
    let sink = Arc::clone(&seen);
    let record: Recorder = Arc::new(move |outcome| sink.lock().unwrap().push(outcome));
    (seen, record)
}

fn seen_now(seen: &Seen) -> Vec<&'static str> {
    seen.lock().unwrap().clone()
}

/// Waits until `seen` holds `expected`, or fails after two seconds.
async fn wait_for(seen: &Seen, expected: &[&str]) {
    for _ in 0..200 {
        if seen_now(seen) == expected {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(seen_now(seen), expected);
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
    let (seen, record) = recorder();
    let (client, server) = tokio::io::duplex(1024);
    let mut stream = CountedStream::with_recorder(server, record);
    drop(client);
    let mut buffer = [0u8; 8];
    assert_eq!(stream.read(&mut buffer).await.unwrap(), 0);
    drop(stream);
    assert_eq!(seen_now(&seen), ["no_request"]);
}

#[tokio::test]
async fn connection_with_one_request_counts_requests() {
    let (seen, record) = recorder();
    let (mut client, server) = tokio::io::duplex(1024);
    let mut stream = CountedStream::with_recorder(server, record);
    client.write_all(b"GET / HTTP/1.1\r\n\r\n").await.unwrap();
    let mut buffer = [0u8; 64];
    assert!(stream.read(&mut buffer).await.unwrap() > 0);
    drop(stream);
    assert_eq!(seen_now(&seen), ["requests"]);
}

#[tokio::test]
async fn connection_with_several_reads_counts_requests_once() {
    let (seen, record) = recorder();
    let (mut client, server) = tokio::io::duplex(1024);
    let mut stream = CountedStream::with_recorder(server, record);
    let mut buffer = [0u8; 4];
    for _ in 0..3 {
        client.write_all(b"abcd").await.unwrap();
        assert_eq!(stream.read(&mut buffer).await.unwrap(), 4);
    }
    drop(client);
    assert_eq!(stream.read(&mut buffer).await.unwrap(), 0);
    drop(stream);
    assert_eq!(seen_now(&seen), ["requests"]);
}

#[tokio::test]
async fn writes_pass_through_and_do_not_count_as_a_request() {
    let (seen, record) = recorder();
    let (mut client, server) = tokio::io::duplex(1024);
    let mut stream = CountedStream::with_recorder(server, record);
    stream.write_all(b"hello").await.unwrap();
    let mut buffer = [0u8; 5];
    client.read_exact(&mut buffer).await.unwrap();
    assert_eq!(&buffer, b"hello");
    drop(stream);
    assert_eq!(seen_now(&seen), ["no_request"]);
}

#[tokio::test]
async fn served_connections_count_by_whether_they_sent_a_request() {
    let (seen, record) = recorder();
    // `pop` hands out the last connection first: the one with a request comes first.
    let (mut idle_client, idle_server) = tokio::io::duplex(64 * 1024);
    let (mut client, server) = tokio::io::duplex(64 * 1024);
    let app = Router::new().route("/", get(|| async { "ok" }));
    let serving = tokio::spawn(async move {
        let listener =
            CountedListener::with_recorder(PipeListener(vec![idle_server, server]), record);
        let _ = axum::serve(listener, app).await;
    });

    client
        .write_all(b"GET / HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut reply = Vec::new();
    client.read_to_end(&mut reply).await.unwrap();
    assert!(String::from_utf8_lossy(&reply).starts_with("HTTP/1.1 200"));
    wait_for(&seen, &["requests"]).await;

    idle_client.shutdown().await.unwrap();
    let mut rest = Vec::new();
    idle_client.read_to_end(&mut rest).await.unwrap();
    wait_for(&seen, &["requests", "no_request"]).await;
    serving.abort();
}

/// This test opens a port, so it runs only in the container test set and in CI.
#[tokio::test]
#[ignore = "opens a port: runs in the container test set and in CI"]
async fn a_real_connection_closed_with_no_request_counts_no_request() {
    use axum::serve::Listener;

    let (seen, record) = recorder();
    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = tcp.local_addr().unwrap();
    let mut listener = CountedListener::with_recorder(tcp, record);
    let client = tokio::net::TcpStream::connect(address).await.unwrap();
    let (mut stream, _) = listener.accept().await;
    drop(client);
    let mut buffer = [0u8; 8];
    assert_eq!(stream.read(&mut buffer).await.unwrap(), 0);
    drop(stream);
    wait_for(&seen, &["no_request"]).await;
}

/// Other tests start the real server and add to the global counter at the same time, so
/// this test checks for at least one more, not for an exact value.
#[tokio::test]
async fn the_production_listener_adds_to_the_global_counter() {
    let count = |outcome| {
        test_support::exact_counter_value("harold.web_app.connections", &[("outcome", outcome)])
            .unwrap_or(0)
    };
    let (requests_before, none_before) = (count("requests"), count("no_request"));
    let (mut client, server) = tokio::io::duplex(1024);
    let mut stream = CountedStream::new(server);
    client.write_all(b"x").await.unwrap();
    let mut buffer = [0u8; 1];
    stream.read_exact(&mut buffer).await.unwrap();
    drop(stream);
    assert!(count("requests") > requests_before);
    let (_client, server) = tokio::io::duplex(1024);
    drop(CountedStream::new(server));
    assert!(count("no_request") > none_before);
}
