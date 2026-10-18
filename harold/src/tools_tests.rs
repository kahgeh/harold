use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use harold_api::harold as hp;
use harold_api::tools::questions::v1 as pb;
use tokio::sync::watch;
use tonic::service::Routes;
use tonic::{Request, Response, Status};

use super::*;
use crate::agent::domain::EffectiveAgentState;
use crate::outbound::fake::CapturedLogs;
use crate::telemetry::test_support;
use crate::timed_prompts::tests::{CHIEF, Fake, Key, wait_until};
use crate::timed_prompts::{AgentPane, AgentsView, Attempt, Ports, Reason, type_when_ready};

const CANARY: &str = "QUEUE-CANARY-2207";
const DAY: &str = "2026-10-14";

/// A folder under the system's temporary folder, removed when dropped.
struct TempFolder(PathBuf);

impl TempFolder {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("harold-tools-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    /// A store path inside the folder, so the tools' data folder is `<folder>/tools/`.
    fn store_path(&self) -> String {
        self.0.join("store").to_str().unwrap().to_string()
    }
}

impl Drop for TempFolder {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A question id no other test uses, so exported records can be told apart.
fn unique_id() -> String {
    format!("C-{}", &uuid::Uuid::new_v4().simple().to_string()[..12])
}

fn pane(pane_id: &str, folder: &str) -> AgentPane {
    AgentPane {
        pane_id: pane_id.into(),
        folder: folder.into(),
        reads_as_claude: true,
        state: EffectiveAgentState::Idle,
    }
}

fn panes(panes: Vec<AgentPane>) -> AgentsView {
    AgentsView {
        listing_failed: false,
        panes,
    }
}

/// The host's typing and chief lookup over the fake ports, as `start` builds them over tmux.
fn host_ports(fake: &Fake) -> (Arc<dyn PaneTyping>, Arc<dyn ChiefPane>) {
    let ports = Arc::new(fake.clone());
    let typing = TmuxTyping {
        ports: Arc::clone(&ports),
        writer: Writer::Tool(questions::NAME),
    };
    let chief = SnapshotChiefPane {
        ports,
        folder: Some(CHIEF.to_string()),
    };
    (Arc::new(typing), Arc::new(chief))
}

/// One unary call to the question service through `routes`, framed as a gRPC client
/// frames it. A refusal comes back as its `grpc-status` code and message.
async fn call<Req: prost::Message, Resp: prost::Message + Default>(
    routes: &Routes,
    method: &str,
    request: Req,
) -> Result<Resp, String> {
    use tonic::codegen::Service;

    let message = request.encode_to_vec();
    let mut framed = vec![0u8];
    framed.extend_from_slice(&(message.len() as u32).to_be_bytes());
    framed.extend_from_slice(&message);
    let request = tonic::codegen::http::Request::builder()
        .method("POST")
        .uri(format!(
            "/harold.tools.questions.v1.QuestionService/{method}"
        ))
        .header("content-type", "application/grpc")
        .header("te", "trailers")
        .body(axum::body::Body::from(framed))
        .unwrap();
    let mut router = routes.clone().into_axum_router();
    let response = router.call(request).await.unwrap();
    if let Some(status) = response.headers().get("grpc-status")
        && status != "0"
    {
        let message = response
            .headers()
            .get("grpc-message")
            .map(|value| value.to_str().unwrap().to_string())
            .unwrap_or_default();
        return Err(format!("{} {message}", status.to_str().unwrap()));
    }
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    Ok(Resp::decode(&body[5..]).unwrap())
}

fn question(id: &str) -> pb::Question {
    pb::Question {
        id: id.into(),
        product: "harold".into(),
        spec: "canary-spec".into(),
        sha: "abc123".into(),
        link: "https://example.com/spec".into(),
        intro: "An intro.".into(),
        decisions: vec![pb::Decision {
            ask: "Which?".into(),
            options: vec![pb::DecisionOption {
                key: "a".into(),
                label: "A".into(),
                impact: "Some impact.".into(),
                suggested: true,
                free: false,
            }],
        }],
    }
}

#[test]
fn data_folder_is_beside_the_store() {
    assert_eq!(
        data_folder("/Users/me/.harold/store", "questions"),
        PathBuf::from("/Users/me/.harold/tools/questions")
    );
    assert_eq!(
        data_folder("/Users/me/bin/harold/data/store", questions::NAME),
        PathBuf::from("/Users/me/bin/harold/data/tools/questions")
    );
    println!(
        "evidence: store /Users/me/.harold/store -> data folder {}",
        data_folder("/Users/me/.harold/store", "questions").display()
    );
}

#[tokio::test]
async fn a_tool_that_fails_to_start_is_logged_and_harold_runs_on() {
    let logs = CapturedLogs::start();
    let folder = TempFolder::new();
    // `tools` is a file, so the tool's data folder `<folder>/tools/questions` cannot be made.
    std::fs::write(folder.0.join("tools"), "not a folder").unwrap();
    let (typing, chief) = host_ports(&Fake::at(DAY, "09:00:00"));
    let (_shutdown, shutdown_rx) = watch::channel(());

    let tools = start_with(&folder.store_path(), typing, chief, shutdown_rx).await;

    assert!(tools.tasks.is_empty());
    let text = logs.text();
    let failed: Vec<&str> = text
        .lines()
        .filter(|line| line.contains("tool failed to start"))
        .collect();
    assert_eq!(failed.len(), 1, "{text}");
    // Only `tool` and a fixed `reason`: no error text, so no path.
    assert_eq!(
        failed[0],
        " WARN harold::tools: tool failed to start tool=\"questions\" reason=\"store_open_failed\""
    );
    let folder_name = folder.0.file_name().unwrap().to_str().unwrap();
    assert!(!text.contains(folder_name), "{text}");
    assert!(
        !test_support::logs()
            .iter()
            .any(|log| format!("{:?}", log.record).contains(folder_name)),
        "an exported record names the data folder"
    );
    // Harold runs on: the routes answer, as unimplemented, and the host stops at once.
    let listed: Result<pb::ListQuestionsResponse, String> =
        call(&tools.routes, "ListQuestions", pb::ListQuestionsRequest {}).await;
    assert_eq!(listed.unwrap_err(), "12 ");
    tokio::time::timeout(Duration::from_secs(1), tools.stopped())
        .await
        .expect("a host with no tool stops at once");
    println!("evidence: log line: {}", failed[0]);
}

#[tokio::test]
async fn the_host_types_as_writer_tool_questions() {
    let fake = Fake::at(DAY, "09:00:00");
    let typing = TmuxTyping {
        ports: Arc::new(fake.clone()),
        writer: Writer::Tool(questions::NAME),
    };
    let line = "ANSWERS 1 | Q1 s1 sha abc: Which? A";

    assert_eq!(typing.type_line("%2", line).await, Typing::Typed);

    assert_eq!(
        fake.keys(),
        [Key::Text("%2".into(), line.into()), Key::Enter("%2".into())]
    );
    // The pane is held for the question tool: an alert waits as if it were busy.
    let view = fake.agents();
    let alert = type_when_ready(&fake, Writer::Alert, CHIEF, "alert", &view, &[]).await;
    assert!(matches!(alert, Attempt::Wait(Reason::AgentBusy)));
    assert_eq!(fake.keys().len(), 2);
    println!(
        "evidence: typed as Writer::Tool(\"{}\"): {:?}; an alert then waits with agent_busy",
        questions::NAME,
        fake.keys()
    );
}

#[tokio::test]
async fn the_host_maps_each_line_outcome() {
    let fake = Fake::at(DAY, "09:00:00");
    let typing = TmuxTyping {
        ports: Arc::new(fake.clone()),
        writer: Writer::Tool(questions::NAME),
    };

    assert_eq!(typing.type_line("%9", "line").await, Typing::NoPane);
    fake.set(|state| state.fail_enter = true);
    assert_eq!(typing.type_line("%2", "line").await, Typing::EnterFailed);
}

#[tokio::test(start_paused = true)]
async fn a_pane_busy_for_two_minutes_is_pane_busy() {
    let fake = Fake::at(DAY, "09:00:00");
    fake.set(|state| {
        state.view = panes(vec![AgentPane {
            state: EffectiveAgentState::Busy,
            ..pane("%2", CHIEF)
        }])
    });
    let typing = TmuxTyping {
        ports: Arc::new(fake.clone()),
        writer: Writer::Tool(questions::NAME),
    };

    assert_eq!(typing.type_line("%2", "line").await, Typing::PaneBusy);
    assert!(fake.keys().is_empty());
}

#[test]
fn the_chief_pane_is_the_agent_in_the_chief_folder() {
    let fake = Fake::at(DAY, "09:00:00");
    let chief = |folder: Option<&str>| SnapshotChiefPane {
        ports: Arc::new(fake.clone()),
        folder: folder.map(str::to_string),
    };
    fake.set(|state| {
        state.view = panes(vec![
            pane("%3", "/work/harold"),
            pane("%2", &format!("{CHIEF}/")),
        ])
    });

    assert_eq!(chief(Some(CHIEF)).chief_pane().as_deref(), Some("%2"));
    assert_eq!(chief(None).chief_pane(), None);
    assert_eq!(chief(Some("/work/elsewhere")).chief_pane(), None);

    // Two agents in the chief's folder: which one is the chief is not known.
    fake.set(|state| state.view.panes.push(pane("%4", CHIEF)));
    assert_eq!(chief(Some(CHIEF)).chief_pane(), None);
    println!(
        "evidence: chief pane %2 in {CHIEF}/; none with no folder, no pane, or two panes there"
    );
}

#[test]
fn tools_rs_names_no_database_type() {
    let host = include_str!("tools.rs");
    assert!(!host.to_lowercase().contains("turso"));
    assert!(!host.contains("QuestionStore"));
}

#[tokio::test]
async fn the_canary_reaches_no_log_or_export() {
    test_support::install();
    let logs = CapturedLogs::start();
    let folder = TempFolder::new();
    let fake = Fake::at(DAY, "09:00:00");
    let (typing, chief) = host_ports(&fake);
    let (shutdown, shutdown_rx) = watch::channel(());
    let tools = start_with(&folder.store_path(), typing, chief, shutdown_rx).await;
    assert_eq!(tools.tasks.len(), 1);
    let id = unique_id();

    let put: pb::PutQuestionResponse = call(
        &tools.routes,
        "PutQuestion",
        pb::PutQuestionRequest {
            question: Some(question(&id)),
        },
    )
    .await
    .unwrap();
    let _: pb::AnswerQuestionResponse = call(
        &tools.routes,
        "AnswerQuestion",
        pb::AnswerQuestionRequest {
            id: id.clone(),
            version: put.version,
            kind: pb::AnswerKind::Revise as i32,
            picks: Vec::new(),
            note: CANARY.into(),
        },
    )
    .await
    .unwrap();

    // The chief takes the batch, canary and all.
    wait_until("the batch is typed", || fake.keys().len() == 2).await;
    let typed = match &fake.keys()[0] {
        Key::Text(pane, text) => format!("{pane}: {text}"),
        other => panic!("not a typed line: {other:?}"),
    };
    assert!(typed.contains(CANARY), "{typed}");
    drop(shutdown);
    tokio::time::timeout(Duration::from_secs(5), tools.stopped())
        .await
        .expect("the tool stops on shutdown");

    // The captured `harold.log` lines name the question and hold no canary.
    let text = logs.text();
    let ours: Vec<&str> = text.lines().filter(|line| line.contains(&id)).collect();
    assert!(
        ours.iter().any(|line| line.contains("question stored")),
        "{text}"
    );
    assert!(
        ours.iter().any(|line| line.contains("question answered")),
        "{text}"
    );
    assert!(text.contains("answers batch typed"), "{text}");
    assert!(!text.contains(CANARY), "{text}");

    // The question's lines are exported (without `tool` and `question_id`, which are not
    // exported fields), and no record from any test holds the canary.
    let records: Vec<String> = test_support::logs()
        .iter()
        .map(|log| format!("{:?}", log.record))
        .collect();
    for line in [
        "question stored",
        "question answered",
        "answers batch typed",
    ] {
        assert!(
            records.iter().any(|record| record.contains(line)),
            "{line} not exported"
        );
    }
    let holding: Vec<&String> = records
        .iter()
        .filter(|record| record.contains(CANARY))
        .collect();
    assert!(holding.is_empty(), "{holding:#?}");
    let spans = format!("{:?}", test_support::spans());
    assert!(!spans.contains(CANARY));
    println!(
        "evidence: typed {typed:?}; {} captured log lines and {} exported records, none holds {CANARY}",
        text.lines().count(),
        records.len()
    );
}

/// The core service, as far as routing goes: every call reaches it and is refused with
/// `NotFound`, which the empty router's `Unimplemented` fallback never gives.
struct CoreStub;

#[tonic::async_trait]
impl hp::harold_server::Harold for CoreStub {
    type WatchAgentStatesStream =
        tokio_stream::wrappers::ReceiverStream<Result<hp::AgentStateSnapshot, Status>>;

    async fn turn_complete(
        &self,
        _: Request<hp::TurnCompleteRequest>,
    ) -> Result<Response<hp::TurnCompleteResponse>, Status> {
        Err(Status::not_found("core"))
    }

    async fn report_agent_state(
        &self,
        _: Request<hp::ReportAgentStateRequest>,
    ) -> Result<Response<hp::ReportAgentStateResponse>, Status> {
        Err(Status::not_found("core"))
    }

    async fn set_tts_muted(
        &self,
        _: Request<hp::SetTtsMutedRequest>,
    ) -> Result<Response<hp::SetTtsMutedResponse>, Status> {
        Err(Status::not_found("core"))
    }

    async fn set_messaging_paused(
        &self,
        _: Request<hp::SetMessagingPausedRequest>,
    ) -> Result<Response<hp::SetMessagingPausedResponse>, Status> {
        Err(Status::not_found("core"))
    }

    async fn report_notification(
        &self,
        _: Request<hp::ReportNotificationRequest>,
    ) -> Result<Response<hp::ReportNotificationResponse>, Status> {
        Err(Status::not_found("core"))
    }

    async fn forward_reply(
        &self,
        _: Request<hp::ForwardReplyRequest>,
    ) -> Result<Response<hp::ForwardReplyResponse>, Status> {
        Err(Status::not_found("core"))
    }

    async fn send_message_to_away_device(
        &self,
        _: Request<hp::SendMessageToAwayDeviceRequest>,
    ) -> Result<Response<hp::SendMessageToAwayDeviceResponse>, Status> {
        Err(Status::not_found("core"))
    }

    async fn speak_line(
        &self,
        _: Request<hp::SpeakLineRequest>,
    ) -> Result<Response<hp::SpeakLineResponse>, Status> {
        Err(Status::not_found("core"))
    }

    async fn call(
        &self,
        _: Request<hp::CallRequest>,
    ) -> Result<Response<hp::CallResponse>, Status> {
        Err(Status::not_found("core"))
    }

    async fn watch_agent_states(
        &self,
        _: Request<hp::WatchAgentStatesRequest>,
    ) -> Result<Response<Self::WatchAgentStatesStream>, Status> {
        Err(Status::not_found("core"))
    }
}

#[tokio::test]
async fn the_core_service_and_the_tool_share_one_server() {
    let folder = TempFolder::new();
    let (typing, chief) = host_ports(&Fake::at(DAY, "09:00:00"));
    let (shutdown, shutdown_rx) = watch::channel(());
    let tools = start_with(&folder.store_path(), typing, chief, shutdown_rx).await;

    // The server as `main` builds it, on a local port.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let (incoming, connections) = tokio::sync::mpsc::channel(4);
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
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(
        tonic::transport::Server::builder()
            .add_routes(tools.routes.clone())
            .add_service(hp::harold_server::HaroldServer::new(CoreStub))
            .serve_with_incoming_shutdown(
                tokio_stream::wrappers::ReceiverStream::new(connections),
                async {
                    let _ = stopped.await;
                },
            ),
    );

    let mut core = hp::harold_client::HaroldClient::connect(address.clone())
        .await
        .unwrap();
    let refused = core
        .set_tts_muted(hp::SetTtsMutedRequest::default())
        .await
        .unwrap_err();
    assert_eq!(
        (refused.code(), refused.message()),
        (tonic::Code::NotFound, "core")
    );
    let mut queue = pb::question_service_client::QuestionServiceClient::connect(address)
        .await
        .unwrap();
    let listing = queue
        .list_questions(pb::ListQuestionsRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(listing.waiting, 0);

    stop.send(()).unwrap();
    server.await.unwrap().unwrap();
    accept.abort();
    drop(shutdown);
    tokio::time::timeout(Duration::from_secs(5), tools.stopped())
        .await
        .expect("the tool stops on shutdown");
    println!(
        "evidence: one server: SetTtsMuted reached the core ({:?} \"core\"), ListQuestions reached the tool (waiting = {})",
        refused.code(),
        listing.waiting
    );
}
