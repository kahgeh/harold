use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

use harold_api::harold::{
    AgentPaneState, AgentStateSnapshot, ReportAgentStateRequest, ReportAgentStateResponse,
    TurnCompleteRequest, TurnCompleteResponse, WatchAgentStatesRequest,
    harold_server::{Harold, HaroldServer},
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

struct Fixture(PathBuf);

impl Fixture {
    fn new(overlay: &str) -> Self {
        let path = std::env::temp_dir().join(format!("harold-probe-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        std::fs::write(
            path.join("default.toml"),
            include_str!("../config/default.toml"),
        )
        .unwrap();
        std::fs::write(path.join("local.toml"), overlay).unwrap();
        Self(path)
    }

    fn run(&self, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_harold"));
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("HAROLD") {
                command.env_remove(name);
            }
        }
        let mut child = command
            .args(args)
            .env("HAROLD_CONFIG_DIR", &self.0)
            .env("HAROLD_ENV", "local")
            .env("HAROLD__STORE__PATH", self.0.join("unopened-store"))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(8);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                child.kill().unwrap();
                let output = child.wait_with_output().unwrap();
                panic!("probe did not exit: {output:?}");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let output = child.wait_with_output().unwrap();
        assert!(
            !self.0.join("unopened-store").exists(),
            "probe opened storage"
        );
        output
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const VALID: &str = "[imessage]\nrecipient = 'secret-recipient'\nhandle_ids = [999]\n";

#[test]
fn conflicting_probe_modes_are_rejected_before_loading_config() {
    let fixture = Fixture::new("invalid toml!");
    for args in [
        vec!["--check-config", "--check-ready"],
        vec!["--check-config", "--diagnostics"],
        vec!["--check-ready", "--delay"],
        vec!["--unknown-option"],
    ] {
        let output = fixture.run(&args);
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("argument"));
    }
}

#[test]
fn config_probe_returns_only_resolved_nonsecret_settings_without_opening_storage() {
    let fixture = Fixture::new(VALID);
    let output = fixture.run(&["--check-config"]);
    assert!(output.status.success(), "{output:?}");
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        value,
        serde_json::json!({
            "grpc_addr": "127.0.0.1:50060",
            "store_path": fixture.0.join("unopened-store").to_str().unwrap(),
        })
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("secret-recipient"));
}

#[test]
fn config_probe_rejects_invalid_typed_config_and_address_without_opening_storage() {
    for overlay in [
        "[imessage]\nhandle_ids = 'wrong type'\n",
        "",
        "[imessage]\nrecipient = 'test'\nhandle_ids = [1]\n[grpc]\nhost = 'invalid host'\n",
    ] {
        let output = Fixture::new(overlay).run(&["--check-config"]);
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn probes_do_not_disclose_configuration_values_in_load_or_validation_errors() {
    for overlay in [
        "[telegram]\nchat_id = 'review-sentinel-secret'\n",
        "[telegram]\nbot_token = 'review-sentinel-secret' invalid-toml\n",
        "[notify]\naway_channel = 'review-sentinel-secret'\n",
    ] {
        for mode in ["--check-config", "--check-ready"] {
            let output = Fixture::new(overlay).run(&[mode]);
            assert!(!output.status.success());
            assert!(output.stdout.is_empty());
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(!stderr.contains("review-sentinel-secret"));
            assert!(stderr.contains("invalid configuration"));
        }
    }
}

#[derive(Clone, Copy)]
enum ResponseKind {
    Snapshot,
    Closed,
    Error,
    Pending,
}

struct ProbeServer {
    response: ResponseKind,
    writes: Arc<AtomicUsize>,
}

#[tonic::async_trait]
impl Harold for ProbeServer {
    type WatchAgentStatesStream = ReceiverStream<Result<AgentStateSnapshot, Status>>;

    async fn turn_complete(
        &self,
        _: Request<TurnCompleteRequest>,
    ) -> Result<Response<TurnCompleteResponse>, Status> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        Err(Status::permission_denied("probe must not write"))
    }

    async fn report_agent_state(
        &self,
        _: Request<ReportAgentStateRequest>,
    ) -> Result<Response<ReportAgentStateResponse>, Status> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        Err(Status::permission_denied("probe must not write"))
    }

    async fn watch_agent_states(
        &self,
        _: Request<WatchAgentStatesRequest>,
    ) -> Result<Response<Self::WatchAgentStatesStream>, Status> {
        let (sender, receiver) = mpsc::channel(1);
        match self.response {
            ResponseKind::Snapshot => {
                sender
                    .send(Ok(AgentStateSnapshot {
                        through_event_version: 42,
                        panes: vec![AgentPaneState {
                            work_summary: Some("private pane contents".into()),
                            ..Default::default()
                        }],
                        ..Default::default()
                    }))
                    .await
                    .unwrap();
                // Keep the stream alive: a probe must return after the first item.
                tokio::spawn(async move { sender.closed().await });
            }
            ResponseKind::Closed => {}
            ResponseKind::Error => {
                sender
                    .send(Err(Status::unavailable("snapshot unavailable")))
                    .await
                    .unwrap();
            }
            ResponseKind::Pending => {
                tokio::spawn(async move { sender.closed().await });
            }
        }
        Ok(Response::new(ReceiverStream::new(receiver)))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readiness_probe_reads_only_one_snapshot_and_handles_eof_error_and_timeout() {
    for kind in [
        ResponseKind::Snapshot,
        ResponseKind::Closed,
        ResponseKind::Error,
        ResponseKind::Pending,
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (incoming, connections) = mpsc::channel(1);
        let accept = tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                if incoming
                    .send(Ok::<_, std::io::Error>(socket))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        let writes = Arc::new(AtomicUsize::new(0));
        let server = tokio::spawn(
            tonic::transport::Server::builder()
                .add_service(HaroldServer::new(ProbeServer {
                    response: kind,
                    writes: Arc::clone(&writes),
                }))
                .serve_with_incoming(ReceiverStream::new(connections)),
        );
        let fixture = Fixture::new(&format!("{VALID}\n[grpc]\nport = {}\n", address.port()));
        let started = Instant::now();
        let output = fixture.run(&["--check-ready"]);
        server.abort();
        accept.abort();
        assert_eq!(
            writes.load(Ordering::SeqCst),
            0,
            "probe used a mutating RPC"
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains("private pane contents"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("private pane contents"));
        if matches!(kind, ResponseKind::Snapshot) {
            assert!(output.status.success(), "{output:?}");
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
                serde_json::json!({"ready": true, "through_event_version": 42})
            );
        } else {
            assert!(!output.status.success(), "{output:?}");
            assert!(output.stdout.is_empty());
        }
        if matches!(kind, ResponseKind::Pending) {
            assert!(started.elapsed() < Duration::from_secs(7));
            assert!(String::from_utf8_lossy(&output.stderr).contains("timed out"));
        }
    }
}

#[test]
fn readiness_probe_fails_for_an_unreachable_server_without_opening_storage() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    // Release a freshly assigned local port so no server accepts the probe.
    drop(listener);
    let fixture = Fixture::new(&format!("{VALID}\n[grpc]\nport = {port}\n"));
    let output = fixture.run(&["--check-ready"]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
}
