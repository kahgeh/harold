use std::net::SocketAddr;

use super::*;

fn address(text: &str) -> SocketAddr {
    text.parse().unwrap()
}

#[test]
fn only_a_loopback_address_is_allowed() {
    assert!(require_loopback(address("127.0.0.1:1729")).is_ok());
    for refused in [
        "0.0.0.0:1729",
        "192.168.1.5:1729",
        "[::]:1729",
        "[::1]:1729",
    ] {
        assert!(require_loopback(address(refused)).is_err(), "{refused}");
    }
}

#[test]
fn the_config_refuses_an_address_that_is_not_loopback() {
    let folder = Path::new("/x/typedb");
    assert!(config_text(address("0.0.0.0:1"), address("127.0.0.1:2"), folder).is_err());
    assert!(config_text(address("127.0.0.1:1"), address("0.0.0.0:2"), folder).is_err());
}

#[test]
fn the_config_listens_on_loopback_and_reports_nothing() {
    let text = config_text(
        address("127.0.0.1:1"),
        address("127.0.0.1:2"),
        Path::new("/x/ty\"pedb"),
    )
    .unwrap();
    assert!(text.contains("    listen-address: 127.0.0.1:1\n"));
    assert!(text.contains("        listen-address: 127.0.0.1:2\n"));
    assert!(!text.contains("0.0.0.0"));
    assert!(text.contains("    reporting:\n        metrics: false\n        errors: false\n"));
    assert!(text.contains("    monitoring:\n        enabled: false\n"));
    assert!(text.contains("data-directory: \"/x/ty\\\"pedb/data\""));
    assert!(text.ends_with('\n'));
}

#[test]
fn two_free_ports_differ_while_both_are_held() {
    let first = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let second = free_port().unwrap();
    assert_ne!(first.local_addr().unwrap().port(), second);
}

/// A server whose process is `sleep`, signed in, with the idle time as given. The binary is
/// missing, so a start fails: a call that finds no server gets an error.
async fn running(idle: Duration) -> Arc<TypeDbServer> {
    let server = TypeDbServer::new(ServerConfig {
        binary: PathBuf::from("/nonexistent/typedb"),
        folder: PathBuf::from("/nonexistent/typedb-folder"),
        idle,
    });
    let child = Command::new("/bin/sleep")
        .arg("30")
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    *server.live.lock().await = Some(Live {
        child,
        base: "http://127.0.0.1:1".into(),
        token: Some("token".into()),
        last_used: Instant::now(),
    });
    server
}

async fn is_live(server: &TypeDbServer) -> bool {
    server.live.lock().await.is_some()
}

#[tokio::test]
async fn a_server_that_is_not_idle_stays_and_an_idle_one_is_stopped() {
    let server = running(Duration::from_secs(3600)).await;
    server.stop_if_idle().await;
    assert!(is_live(&server).await);
    let server = running(Duration::ZERO).await;
    server.stop_if_idle().await;
    assert!(!is_live(&server).await);
}

#[tokio::test]
async fn a_call_that_waits_during_an_idle_stop_is_never_stopped_under() {
    let server = running(Duration::ZERO).await;
    // Hold the lock, so that the idle stop and the call both wait for it, the call second.
    let held = server.live.lock().await;
    let idle_stop = tokio::spawn({
        let server = Arc::clone(&server);
        async move { server.stop_if_idle().await }
    });
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    let call = tokio::spawn({
        let server = Arc::clone(&server);
        async move { server.lease().await }
    });
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    drop(held);
    idle_stop.await.unwrap();
    let lease = call.await.unwrap();
    // The call either got a server that is still there, or started a new one (here it cannot).
    assert!(
        lease.is_err() || is_live(&server).await,
        "the call has a lease on a server that was stopped"
    );
    server.stop().await;
}

#[tokio::test]
async fn a_look_with_peek_lease_neither_starts_the_server_nor_marks_it_used() {
    let none = TypeDbServer::new(ServerConfig {
        binary: PathBuf::from("/nonexistent/typedb"),
        folder: PathBuf::from("/nonexistent/typedb-folder"),
        idle: Duration::from_secs(60),
    });
    assert!(none.peek_lease().await.is_none());
    assert!(!is_live(&none).await);

    let server = running(Duration::from_secs(3600)).await;
    let before = server.live.lock().await.as_ref().unwrap().last_used;
    let lease = server.peek_lease().await.unwrap();
    assert_eq!(lease.token, "token");
    assert_eq!(server.live.lock().await.as_ref().unwrap().last_used, before);
    server.stop().await;
    assert!(server.peek_lease().await.is_none());
}

#[tokio::test]
async fn the_reaper_stops_the_server_at_shutdown_and_its_handle_ends_after() {
    let server = running(Duration::from_secs(3600)).await;
    let (shutdown, signal) = tokio::sync::watch::channel(());
    let reaper = server.spawn_reaper(signal);
    shutdown.send(()).unwrap();
    reaper.await.unwrap();
    assert!(!is_live(&server).await);
}
