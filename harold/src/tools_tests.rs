use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use harold_api::harold as hp;
use harold_api::tools::backlog::v1 as bp;
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
const QUESTIONS: &str = "harold.tools.questions.v1.QuestionService";
const BACKLOG: &str = "harold.tools.backlog.v1.BacklogService";
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

/// Agents and pods that hold nothing, for a test of a tool that does not read them.
pub(crate) struct NoAgents;

impl Agents for NoAgents {
    fn agents(&self) -> Vec<AgentFact> {
        Vec::new()
    }

    fn input_box<'a>(&'a self, _pane_id: &'a str) -> BoxFuture<'a, Option<InputBox>> {
        Box::pin(async { None })
    }
}

pub(crate) struct NoPods;

impl Pods for NoPods {
    fn pods(&self) -> PodSnapshot {
        PodSnapshot::default()
    }
}

/// The extras the host reads, empty.
pub(crate) fn no_extras() -> Extras {
    Extras {
        watch_typing: Arc::new(NoTyping),
        agents: Arc::new(NoAgents),
        pods: Arc::new(NoPods),
        watch_settings: WatchSettings::default(),
        model_environment: Vec::new(),
        cli_path: "claude".into(),
    }
}

struct NoTyping;

impl PaneTyping for NoTyping {
    fn type_line<'a>(&'a self, _pane_id: &'a str, _text: &'a str) -> BoxFuture<'a, Typing> {
        Box::pin(async { Typing::NoPane })
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

/// A `PaneFolder` that knows these panes.
struct FixedFolders(Vec<(String, PathBuf)>);

impl PaneFolder for FixedFolders {
    fn folder_of(&self, pane_id: &str) -> Option<PathBuf> {
        self.0
            .iter()
            .find(|(id, _)| id == pane_id)
            .map(|(_, folder)| folder.clone())
    }
}

/// A `Products` that answers with a fixed result.
struct FixedProducts(Result<Vec<(String, PathBuf)>, &'static str>);

impl Products for FixedProducts {
    fn products(&self) -> Result<Vec<(String, PathBuf)>, &'static str> {
        self.0.clone()
    }
}

type HostPorts = (
    Arc<dyn PaneTyping>,
    Arc<dyn ChiefPane>,
    Arc<dyn PaneFolder>,
    Arc<dyn Products>,
);

/// The host's ports over the fake ports, as `start` builds them over tmux. The
/// pane folder and products answer nothing.
fn host_ports(fake: &Fake) -> HostPorts {
    let ports = Arc::new(fake.clone());
    let typing = TmuxTyping {
        ports: Arc::clone(&ports),
        writer: Writer::Tool(questions::NAME),
    };
    let chief = SnapshotChiefPane {
        ports,
        folder: Some(CHIEF.to_string()),
    };
    (
        Arc::new(typing),
        Arc::new(chief),
        Arc::new(FixedFolders(Vec::new())),
        Arc::new(FixedProducts(Ok(Vec::new()))),
    )
}

/// One unary call to a tool's `service` through `routes`, framed as a gRPC client
/// frames it. A refusal comes back as its `grpc-status` code and message.
async fn call<Req: prost::Message, Resp: prost::Message + Default>(
    routes: &Routes,
    service: &str,
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
        .uri(format!("/{service}/{method}"))
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
        title: String::new(),
        steps: vec![],
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
    // `tools` is a file, so no tool's data folder `<folder>/tools/<name>` can be made.
    std::fs::write(folder.0.join("tools"), "not a folder").unwrap();
    let (typing, chief, folders, products) = host_ports(&Fake::at(DAY, "09:00:00"));
    let (_shutdown, shutdown_rx) = watch::channel(());

    let tools = start_with(
        &folder.store_path(),
        typing,
        chief,
        folders,
        products,
        no_extras(),
        shutdown_rx,
    )
    .await;

    assert!(tools.tasks.is_empty());
    let text = logs.text();
    let failed: Vec<&str> = text
        .lines()
        .filter(|line| line.contains("tool failed to start"))
        .collect();
    // Only `tool` and a fixed `reason`: no error text, so no path.
    assert_eq!(
        failed,
        [
            " WARN harold::tools: tool failed to start tool=\"questions\" reason=\"store_open_failed\"",
            " WARN harold::tools: tool failed to start tool=\"backlog\" reason=\"store_open_failed\"",
            " WARN harold::tools: tool failed to start tool=\"watch\" reason=\"store_open_failed\"",
        ],
        "{text}"
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
    let listed: Result<pb::ListQuestionsResponse, String> = call(
        &tools.routes,
        QUESTIONS,
        "ListQuestions",
        pb::ListQuestionsRequest {},
    )
    .await;
    assert_eq!(listed.unwrap_err(), "12 ");
    tokio::time::timeout(Duration::from_secs(1), tools.stopped())
        .await
        .expect("a host with no tool stops at once");
    println!("evidence: log lines: {failed:?}");
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

#[tokio::test]
async fn an_enter_step_that_fails_after_the_text_is_typed_is_enter_failed() {
    let fake = Fake::at(DAY, "09:00:00");
    fake.set(|state| state.fail_enter = true);
    let typing = TmuxTyping {
        ports: Arc::new(fake.clone()),
        writer: Writer::Tool(questions::NAME),
    };

    let outcome = typing.type_line("%2", "line").await;

    println!("evidence: outcome {outcome:?}, keys {:?}", fake.keys());
    assert_eq!(outcome, Typing::EnterFailed);
    assert_eq!(fake.keys(), vec![Key::Text("%2".into(), "line".into())]);
}

#[tokio::test(start_paused = true)]
async fn a_busy_pane_is_pane_busy_at_once() {
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

    let started = tokio::time::Instant::now();

    assert_eq!(typing.type_line("%2", "line").await, Typing::PaneBusy);
    assert_eq!(started.elapsed(), Duration::ZERO, "one try, no wait");
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
    let logs = crate::outbound::fake::CapturedLogs::start();
    fake.set(|state| state.view.panes.push(pane("%4", CHIEF)));
    assert_eq!(chief(Some(CHIEF)).chief_pane(), None);
    let text = logs.text();
    assert!(text.contains("chief pane changed"), "{text}");
    assert!(text.contains("state=\"ambiguous\"") && text.contains("count=2"), "{text}");
    println!(
        "evidence: chief pane %2 in {CHIEF}/; none with no folder, no pane, or two panes there"
    );
}

#[test]
fn tools_rs_names_no_database_type() {
    let host = include_str!("tools.rs");
    assert!(!host.to_lowercase().contains("turso"));
    assert!(!host.contains("QuestionStore"));
    assert!(!host.contains("BacklogStore"));
}

/// Every `backlog::` in `source` that names the tool's module is `backlog::start` or
/// `backlog::NAME`. `harold_api::tools::backlog::` is the generated proto, not the module.
fn backlog_paths_outside_start_and_name(source: &str) -> Vec<String> {
    source
        .match_indices("backlog::")
        .filter(|(at, _)| !source[..*at].ends_with("harold_api::tools::"))
        .map(|(at, found)| &source[at + found.len()..])
        .filter(|rest| !rest.starts_with("start") && !rest.starts_with("NAME"))
        .map(|rest| rest.lines().next().unwrap_or_default().to_string())
        .collect()
}

#[test]
fn outside_the_module_only_start_and_name_are_used() {
    for (file, source) in [
        ("tools.rs", include_str!("tools.rs")),
        ("cli.rs", include_str!("cli.rs")),
    ] {
        let others = backlog_paths_outside_start_and_name(source);
        assert!(others.is_empty(), "{file} uses backlog::{others:?}");
    }
    assert!(include_str!("tools.rs").contains("backlog::start"));
    assert_eq!(
        backlog_paths_outside_start_and_name(
            "backlog::store::Open; harold_api::tools::backlog::v1"
        ),
        ["store::Open; harold_api::tools::backlog::v1"]
    );
}

#[tokio::test]
async fn the_backlog_tool_starts_with_no_typing_and_creates_backlog_db() {
    let folder = TempFolder::new();
    let (typing, chief, folders, products) = host_ports(&Fake::at(DAY, "09:00:00"));
    let (shutdown, shutdown_rx) = watch::channel(());

    let tools = start_with(
        &folder.store_path(),
        typing,
        chief,
        folders,
        products,
        no_extras(),
        shutdown_rx,
    )
    .await;

    assert_eq!(tools.tasks.len(), 3);
    assert!(folder.0.join("tools/backlog/backlog.db").is_file());
    let listing: bp::ListQueueResponse = call(
        &tools.routes,
        BACKLOG,
        "ListQueue",
        bp::ListQueueRequest { done_since_ms: 0 },
    )
    .await
    .unwrap();
    assert!(listing.now.is_none() && listing.parked.is_empty());
    // The question tool still answers beside it.
    let questions: pb::ListQuestionsResponse = call(
        &tools.routes,
        QUESTIONS,
        "ListQuestions",
        pb::ListQuestionsRequest {},
    )
    .await
    .unwrap();
    assert_eq!(questions.waiting, 0);
    drop(shutdown);
    tokio::time::timeout(Duration::from_secs(5), tools.stopped())
        .await
        .expect("both tools stop on shutdown");
}

#[test]
fn the_backlog_tool_is_given_no_typing() {
    let (typing, chief, folders, products) = host_ports(&Fake::at(DAY, "09:00:00"));
    let (_shutdown, shutdown_rx) = watch::channel(());

    let [(first, questions), (second, backlog), (third, watch_ctx)] = contexts(
        "/Users/me/.harold/store",
        typing,
        chief,
        folders,
        products,
        no_extras(),
        shutdown_rx,
    );

    assert_eq!(
        (first, second, third),
        (questions::NAME, backlog::NAME, crate::tools::watch::NAME)
    );
    assert!(questions.typing.is_some());
    assert!(backlog.typing.is_none());
    assert!(watch_ctx.typing.is_some());
    assert_eq!(
        backlog.data_folder,
        PathBuf::from("/Users/me/.harold/tools/backlog")
    );
}

#[tokio::test]
async fn the_canary_reaches_no_log_or_export() {
    test_support::install();
    let logs = CapturedLogs::start();
    let folder = TempFolder::new();
    let fake = Fake::at(DAY, "09:00:00");
    let (typing, chief, folders, products) = host_ports(&fake);
    let (shutdown, shutdown_rx) = watch::channel(());
    let tools = start_with(
        &folder.store_path(),
        typing,
        chief,
        folders,
        products,
        no_extras(),
        shutdown_rx,
    )
    .await;
    assert_eq!(tools.tasks.len(), 3);
    let id = unique_id();

    let put: pb::PutQuestionResponse = call(
        &tools.routes,
        QUESTIONS,
        "PutQuestion",
        pb::PutQuestionRequest {
            question: Some(question(&id)),
        },
    )
    .await
    .unwrap();
    let _: pb::AnswerQuestionResponse = call(
        &tools.routes,
        QUESTIONS,
        "AnswerQuestion",
        pb::AnswerQuestionRequest {
            id: id.clone(),
            version: put.version,
            kind: pb::AnswerKind::Revise as i32,
            picks: Vec::new(),
            note: CANARY.into(),
            send_now: true,
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

const BACKLOG_CANARY: &str = "BACKLOG-CANARY-3141";

/// A `ChiefPane` that is always this pane.
struct FixedChief(&'static str);

impl ChiefPane for FixedChief {
    fn chief_pane(&self) -> Option<String> {
        Some(self.0.into())
    }
}

#[tokio::test]
async fn the_backlog_canary_reaches_no_log_or_export() {
    test_support::install();
    let logs = CapturedLogs::start();
    let folder = TempFolder::new();
    let (typing, _, folders, _) = host_ports(&Fake::at(DAY, "09:00:00"));
    let products = FixedProducts(Ok(vec![("harold".into(), folder.0.join("harold"))]));
    let (shutdown, shutdown_rx) = watch::channel(());
    let tools = start_with(
        &folder.store_path(),
        typing,
        Arc::new(FixedChief("%2")),
        folders,
        Arc::new(products),
        no_extras(),
        shutdown_rx,
    )
    .await;
    assert_eq!(tools.tasks.len(), 3);
    // An item id no other test uses, so this test's lines can be told apart.
    let id = unique_id();

    let _: bp::PutItemResponse = call(
        &tools.routes,
        BACKLOG,
        "PutItem",
        bp::PutItemRequest {
            product: "harold".into(),
            item: Some(bp::Item {
                id: id.clone(),
                title: "A canary item".into(),
                risk: format!("the risk is {BACKLOG_CANARY}"),
                evidence: vec!["harold.log:1".into()],
                ..Default::default()
            }),
            caller_pane: "%2".into(),
        },
    )
    .await
    .unwrap();
    let note: bp::AddNoteResponse = call(
        &tools.routes,
        BACKLOG,
        "AddNote",
        bp::AddNoteRequest {
            product: "harold".into(),
            kind: "rule".into(),
            title: "A canary note".into(),
            body: format!("the body is {BACKLOG_CANARY}"),
            caller_pane: "%2".into(),
        },
    )
    .await
    .unwrap();
    let _: bp::AddQueueItemResponse = call(
        &tools.routes,
        BACKLOG,
        "AddQueueItem",
        bp::AddQueueItemRequest {
            text: format!("the queue item is {BACKLOG_CANARY}"),
            caller_pane: "%2".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    // Each text is stored, canary and all.
    let items: bp::ListItemsResponse = call(
        &tools.routes,
        BACKLOG,
        "ListItems",
        bp::ListItemsRequest {
            product: "harold".into(),
        },
    )
    .await
    .unwrap();
    assert!(items.items[0].risk.contains(BACKLOG_CANARY));
    let notes: bp::ListNotesResponse = call(
        &tools.routes,
        BACKLOG,
        "ListNotes",
        bp::ListNotesRequest {
            product: "harold".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(notes.notes[0].body.contains(BACKLOG_CANARY));
    let queue: bp::ListQueueResponse = call(
        &tools.routes,
        BACKLOG,
        "ListQueue",
        bp::ListQueueRequest { done_since_ms: 0 },
    )
    .await
    .unwrap();
    assert!(queue.parked[0].text.contains(BACKLOG_CANARY));
    drop(shutdown);
    tokio::time::timeout(Duration::from_secs(5), tools.stopped())
        .await
        .expect("the tools stop on shutdown");

    // The captured `harold.log` lines name the item and the note, and hold no canary.
    let text = logs.text();
    let note_id = format!("note_id={}", note.id);
    assert!(
        text.lines()
            .any(|line| line.contains("backlog item stored") && line.contains(&id)),
        "{text}"
    );
    assert!(
        text.lines()
            .any(|line| line.contains("backlog note added") && line.contains(&note_id)),
        "{text}"
    );
    assert!(text.contains("queue changed"), "{text}");
    assert!(!text.contains(BACKLOG_CANARY), "{text}");

    // No exported record or span from any test holds the canary.
    let records: Vec<String> = test_support::logs()
        .iter()
        .map(|log| format!("{:?}", log.record))
        .collect();
    let holding: Vec<&String> = records
        .iter()
        .filter(|record| record.contains(BACKLOG_CANARY))
        .collect();
    assert!(holding.is_empty(), "{holding:#?}");
    let spans = format!("{:?}", test_support::spans());
    assert!(!spans.contains(BACKLOG_CANARY));
    let exported = ["backlog item stored", "backlog note added", "queue changed"]
        .iter()
        .filter(|line| records.iter().any(|record| record.contains(*line)))
        .count();
    assert_eq!(exported, 3, "the backlog lines are exported");
    println!(
        "evidence: {} captured log lines ({} backlog), {} exported records ({exported} of the 3 \
         backlog lines among them), none holds {BACKLOG_CANARY}",
        text.lines().count(),
        text.lines()
            .filter(|line| line.contains("tool=\"backlog\"") || line.contains("tool=backlog"))
            .count(),
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
    let (typing, chief, folders, products) = host_ports(&Fake::at(DAY, "09:00:00"));
    let (shutdown, shutdown_rx) = watch::channel(());
    let tools = start_with(
        &folder.store_path(),
        typing,
        chief,
        folders,
        products,
        no_extras(),
        shutdown_rx,
    )
    .await;

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

#[tokio::test]
async fn the_question_tool_refuses_to_start_without_typing() {
    let folder = TempFolder::new();
    let (_typing, chief, pane_folder, products) = host_ports(&Fake::at(DAY, "09:00:00"));
    let (_shutdown, shutdown_rx) = watch::channel(());

    let started = questions::start(ToolContext {
        data_folder: folder.0.join("tools").join(questions::NAME),
        typing: None,
        chief_pane: chief,
        pane_folder,
        products,
        shutdown: shutdown_rx,
        agents: Arc::new(NoAgents),
        pods: Arc::new(NoPods),
        watch_settings: WatchSettings::default(),
        model_environment: Vec::new(),
        cli_path: "claude".into(),
    })
    .await;

    let Err(error) = started else {
        panic!("started without typing")
    };
    assert_eq!(error.reason, "no_typing");
}

#[test]
fn registry_products_reads_each_product_with_its_repo() {
    let tmp = TempFolder::new();
    let file = tmp.0.join("products.toml");
    std::fs::write(
        &file,
        format!(
            "[products.harold]\nrepo = \"{0}/harold\"\nbase = \"main\"\n[products.turso]\nrepo = \"{0}/turso\"\nbase = \"main\"\n",
            tmp.0.display()
        ),
    )
    .unwrap();

    assert_eq!(
        RegistryProducts { path: Some(file) }.products(),
        Ok(vec![
            ("harold".into(), tmp.0.join("harold")),
            ("turso".into(), tmp.0.join("turso")),
        ])
    );
    assert_eq!(RegistryProducts { path: None }.products(), Ok(vec![]));
    assert_eq!(
        RegistryProducts {
            path: Some(tmp.0.join("missing.toml"))
        }
        .products(),
        Err("registry_unreadable")
    );
}

#[test]
fn the_pane_folder_is_the_agents_folder_in_the_snapshot() {
    let fake = Fake::at(DAY, "09:00:00");
    fake.set(|state| state.view = panes(vec![pane("%40", "/work/harold"), pane("%2", CHIEF)]));
    let folder = SnapshotPaneFolder {
        ports: Arc::new(fake),
    };

    assert_eq!(folder.folder_of("%40"), Some(PathBuf::from("/work/harold")));
    assert_eq!(folder.folder_of("%99"), None);
}

#[test]
fn the_agents_port_copies_each_pane_with_its_state_time_and_work_summary() {
    use crate::agent::domain::AgentSnapshot;
    use crate::settings::ScreenAdapter;
    use crate::timed_prompts::tests::{projection, provider, snapshot};

    let mut pane = projection("%7", "claude", "/work/harold", EffectiveAgentState::Busy);
    pane.last_transition_at_ms = 9_000;
    pane.work_summary = Some("Writing the watch tool".into());
    let hub = AgentSnapshotHub::new(snapshot(None, vec![pane]));
    let agents = SnapshotAgents {
        ports: Arc::new(Fake::at(DAY, "09:00:00")),
        snapshots: hub.clone(),
        providers: vec![provider("claude", ScreenAdapter::ClaudeV1)],
    };

    let mut listed = agents.agents();
    assert_eq!(
        listed,
        vec![AgentFact {
            pane_id: "%7".into(),
            tmux_target: "pods:0.1".into(),
            folder: "/work/harold".into(),
            provider_id: "claude".into(),
            reads_as_claude: true,
            state: EffectiveAgentState::Busy,
            state_since_ms: 9_000,
            work_summary: Some("Writing the watch tool".into()),
        }]
    );
    // A copy: changing it changes nothing in Harold's snapshot.
    listed[0].work_summary = None;
    listed.clear();
    let again = agents.agents();
    assert_eq!(again.len(), 1);
    assert_eq!(
        again[0].work_summary.as_deref(),
        Some("Writing the watch tool")
    );
    let current: AgentSnapshot = hub.current();
    assert_eq!(current.panes.len(), 1);
}

#[tokio::test]
async fn the_agents_port_reads_the_input_box_from_a_fresh_screen() {
    let fake = Fake::at(DAY, "09:00:00");
    let agents = SnapshotAgents {
        ports: Arc::new(fake.clone()),
        snapshots: AgentSnapshotHub::new(crate::timed_prompts::tests::snapshot(None, Vec::new())),
        providers: Vec::new(),
    };
    // The fake's screen starts with an empty box.
    assert_eq!(agents.input_box("%2").await, Some(InputBox::Empty));
    fake.set(|state| state.screen = None);
    assert_eq!(agents.input_box("%2").await, None);
}

#[test]
fn the_pods_port_returns_a_copy_of_the_gauge_snapshot() {
    let mut first = GaugePods.pods();
    first.slots.insert(
        "invented".into(),
        pod_stats::SlotCounts { busy: 1, free: 1 },
    );
    assert!(!GaugePods.pods().slots.contains_key("invented"));
}
