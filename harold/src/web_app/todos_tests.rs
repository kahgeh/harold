use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use askama::Template;
use harold_api::tools::questions::v1 as pb;
use harold_api::tools::questions::v1::question_service_server::{
    QuestionService, QuestionServiceServer,
};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tonic::{Request, Response, Status};

use super::origin::loopback_hosts;
use super::todos::Questions;
use super::{AppState, router};
use crate::outbound::fake::CapturedLogs;
use crate::settings::GrpcSettings;
use crate::store::HaroldStore;

const LOGIN: &str = "chief@example.com";

/// What the fake question service was asked.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Call {
    Answer(pb::AnswerQuestionRequest),
    Defer(String),
    Tick(pb::TickStepRequest),
}

#[derive(Default)]
pub(super) struct FakeState {
    pub list: Vec<pb::ListedQuestion>,
    pub list_fails: bool,
    /// The refusal every write answers with, if any.
    pub write_refusal: Option<Status>,
    pub calls: Vec<Call>,
}

/// A question service that lists what the test says and records the writes.
#[derive(Clone, Default)]
pub(super) struct FakeQuestions(pub Arc<Mutex<FakeState>>);

impl FakeQuestions {
    fn lock(&self) -> std::sync::MutexGuard<'_, FakeState> {
        self.0.lock().unwrap()
    }

    pub fn set_list(&self, list: Vec<pb::ListedQuestion>) {
        self.lock().list = list;
    }

    pub fn calls(&self) -> Vec<Call> {
        self.lock().calls.clone()
    }

    fn write(&self, call: Call) -> Result<(), Status> {
        let mut state = self.lock();
        if let Some(refusal) = &state.write_refusal {
            return Err(Status::new(refusal.code(), refusal.message()));
        }
        state.calls.push(call);
        Ok(())
    }
}

#[tonic::async_trait]
impl QuestionService for FakeQuestions {
    async fn put_question(
        &self,
        _: Request<pb::PutQuestionRequest>,
    ) -> Result<Response<pb::PutQuestionResponse>, Status> {
        Err(Status::unimplemented("put"))
    }
    async fn dismiss_question(
        &self,
        _: Request<pb::DismissQuestionRequest>,
    ) -> Result<Response<pb::DismissQuestionResponse>, Status> {
        Err(Status::unimplemented("dismiss"))
    }
    async fn reorder_questions(
        &self,
        _: Request<pb::ReorderQuestionsRequest>,
    ) -> Result<Response<pb::ReorderQuestionsResponse>, Status> {
        Err(Status::unimplemented("reorder"))
    }
    async fn list_questions(
        &self,
        _: Request<pb::ListQuestionsRequest>,
    ) -> Result<Response<pb::ListQuestionsResponse>, Status> {
        let state = self.lock();
        if state.list_fails {
            return Err(Status::unavailable("down"));
        }
        Ok(Response::new(pb::ListQuestionsResponse {
            questions: state.list.clone(),
            waiting: 0,
            dismissed: Vec::new(),
        }))
    }
    async fn answer_question(
        &self,
        request: Request<pb::AnswerQuestionRequest>,
    ) -> Result<Response<pb::AnswerQuestionResponse>, Status> {
        self.write(Call::Answer(request.into_inner()))?;
        Ok(Response::new(pb::AnswerQuestionResponse {}))
    }
    async fn defer_question(
        &self,
        request: Request<pb::DeferQuestionRequest>,
    ) -> Result<Response<pb::DeferQuestionResponse>, Status> {
        self.write(Call::Defer(request.into_inner().id))?;
        Ok(Response::new(pb::DeferQuestionResponse {}))
    }
    async fn tick_step(
        &self,
        request: Request<pb::TickStepRequest>,
    ) -> Result<Response<pb::TickStepResponse>, Status> {
        self.write(Call::Tick(request.into_inner()))?;
        Ok(Response::new(pb::TickStepResponse {}))
    }
    async fn save_picks(
        &self,
        _: Request<pb::SavePicksRequest>,
    ) -> Result<Response<pb::SavePicksResponse>, Status> {
        Err(Status::unimplemented("save"))
    }
    async fn show_question(
        &self,
        _: Request<pb::ShowQuestionRequest>,
    ) -> Result<Response<pb::ShowQuestionResponse>, Status> {
        Err(Status::unimplemented("show"))
    }
}

/// A fake question service on a free local port; it stops when this is dropped.
pub(super) struct FakeServer {
    pub fake: FakeQuestions,
    pub grpc: GrpcSettings,
    handle: JoinHandle<()>,
}

impl FakeServer {
    pub async fn start() -> Self {
        let fake = FakeQuestions::default();
        let port = {
            let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            probe.local_addr().unwrap().port()
        };
        let service = QuestionServiceServer::new(fake.clone());
        let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let handle = tokio::spawn(async move {
            let _ = tonic::transport::Server::builder()
                .add_service(service)
                .serve(addr)
                .await;
        });
        // The server takes a moment to bind; the lazy client retries per call.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        Self {
            fake,
            grpc: GrpcSettings {
                host: "127.0.0.1".into(),
                port,
            },
            handle,
        }
    }

    pub fn questions(&self) -> Questions {
        Questions::lazy(&self.grpc)
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

pub(super) struct TestDirectory(PathBuf);

impl TestDirectory {
    pub fn new() -> Self {
        let path = std::env::temp_dir().join(format!("harold-todos-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    pub async fn store(&self) -> Arc<HaroldStore> {
        Arc::new(HaroldStore::open(self.0.join("store")).await.unwrap())
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The router on a free local port, with `questions` as its question service.
pub(super) struct TestServer {
    pub addr: SocketAddr,
    _shutdown: watch::Sender<()>,
    handle: JoinHandle<()>,
    pub state: AppState,
}

impl TestServer {
    pub async fn serve(store: Arc<HaroldStore>, questions: Questions) -> Self {
        let (shutdown, shutdown_rx) = watch::channel(());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut state = AppState::for_tests(
            store,
            LOGIN,
            "BPublicKeyForTheTestPage",
            Arc::from(loopback_hosts("127.0.0.1:0", addr)),
            shutdown_rx,
            None,
            Default::default(),
        );
        state.questions = questions;
        let served = state.clone();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, router(served)).await;
        });
        Self {
            addr,
            _shutdown: shutdown,
            handle,
            state,
        }
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    pub async fn get(&self, path: &str) -> reqwest::Response {
        reqwest::Client::new()
            .get(self.url(path))
            .header("Tailscale-User-Login", LOGIN)
            .send()
            .await
            .unwrap()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn option(key: &str, label: &str, suggested: bool, free: bool) -> pb::DecisionOption {
    pb::DecisionOption {
        key: key.into(),
        label: label.into(),
        impact: format!("{label} impact"),
        suggested,
        free,
    }
}

/// Q1: two decisions, the first with a suggested option, the second with a free option.
pub(super) fn question(id: &str, version: u32, deferred: bool) -> pb::ListedQuestion {
    pb::ListedQuestion {
        question: Some(pb::Question {
            id: id.into(),
            product: "harold".into(),
            spec: "web-app-todos".into(),
            link: "https://example.com/spec".into(),
            intro: format!("Intro of {id}"),
            decisions: vec![
                pb::Decision {
                    ask: "Which way?".into(),
                    options: vec![
                        option("a", "Left", false, false),
                        option("b", "Right", true, false),
                    ],
                },
                pb::Decision {
                    ask: "Anything else?".into(),
                    options: vec![
                        option("x", "Nothing", false, false),
                        option("y", "Other", false, true),
                    ],
                },
            ],
            ..Default::default()
        }),
        version,
        deferred,
        ticked: Vec::new(),
        picks: Vec::new(),
    }
}

/// I1: instructions with three steps, the second ticked.
pub(super) fn instructions(id: &str, version: u32) -> pb::ListedQuestion {
    pb::ListedQuestion {
        question: Some(pb::Question {
            id: id.into(),
            product: "harold".into(),
            title: format!("Title of {id}"),
            intro: "Do these".into(),
            steps: ["one", "two", "three"]
                .map(|text| pb::Step {
                    text: text.into(),
                    detail: format!("{text} detail"),
                })
                .to_vec(),
            ..Default::default()
        }),
        version,
        deferred: false,
        ticked: vec![2],
        picks: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// The list
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn the_list_draws_question_and_instruction_cards_and_counts_the_waiting_ones() {
    let service = FakeServer::start().await;
    let mut later = question("Q3", 1, true);
    later.picks = vec![pb::Pick {
        decision: 1,
        key: "y".into(),
        text: "saved words".into(),
    }];
    let mut saved = question("Q2", 4, false);
    saved.picks = vec![pb::Pick {
        decision: 0,
        key: "a".into(),
        text: String::new(),
    }];
    service.fake.set_list(vec![
        later,
        question("Q1", 2, false),
        instructions("I1", 3),
        saved,
    ]);
    let directory = TestDirectory::new();
    let server = TestServer::serve(directory.store().await, service.questions()).await;

    let page = server.get("/").await.text().await.unwrap();

    // Waiting first in the tool's order, then the set-aside question.
    let q1 = page.find(r#"id="todo-Q1""#).expect("Q1");
    let i1 = page.find(r#"id="todo-I1""#).expect("I1");
    let q2 = page.find(r#"id="todo-Q2""#).expect("Q2");
    let q3 = page.find(r#"id="todo-Q3""#).expect("Q3");
    assert!(q1 < i1 && i1 < q2 && q2 < q3, "{page}");
    assert!(page.contains("Q1 · question · harold</p>"), "{page}");
    assert!(page.contains("I1 · instructions · harold</p>"), "{page}");
    assert!(
        page.contains("Q3 · question · harold · later</p>"),
        "{page}"
    );
    assert!(page.contains("<h3>Intro of Q1</h3>"), "{page}");
    assert!(
        page.contains("<h3>Title of I1</h3>") && page.contains("Do these"),
        "{page}"
    );
    assert!(
        page.contains(r#"<a href="https://example.com/spec" target="_blank" rel="noopener noreferrer">Read the spec: web-app-todos</a>"#),
        "{page}"
    );
    // The suggested option is picked when nothing is saved; a saved pick wins.
    assert!(
        page.contains(r#"name="pick.0" value="b" checked>"#),
        "{page}"
    );
    assert!(
        page.contains(r#"<em class="suggested">suggested</em>"#),
        "{page}"
    );
    assert!(page.contains("Left impact"), "{page}");
    let q2_card = &page[q2..q3];
    assert!(
        q2_card.contains(r#"name="pick.0" value="a" checked>"#),
        "{q2_card}"
    );
    assert!(
        !q2_card.contains(r#"name="pick.0" value="b" checked>"#),
        "{q2_card}"
    );
    // A saved free pick keeps its words.
    assert!(
        page.contains(r#"name="text.1" value="saved words""#),
        "{page}"
    );
    // Steps: the second is ticked.
    assert!(page.contains("1. one"), "{page}");
    let ticked = page
        .matches(r#"name="ticked" value="true" checked"#)
        .count();
    assert_eq!(ticked, 1, "{page}");
    // Two waiting todos (Q1, Q2) and one set of instructions; the question set aside is left out.
    assert!(
        page.contains(r#"<span id="todo-count" class="todo-count">3</span>"#),
        "{page}"
    );
    // Send is on only where every decision has a pick: Q1's second decision has none.
    assert!(
        page.contains(r#"<button type="submit" class="send" disabled>Send</button>"#),
        "{page}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_empty_list_says_so_and_shows_no_count() {
    let service = FakeServer::start().await;
    let directory = TestDirectory::new();
    let server = TestServer::serve(directory.store().await, service.questions()).await;

    let page = server.get("/").await.text().await.unwrap();

    assert!(page.contains("No todos for you."), "{page}");
    assert!(
        page.contains(r#"<span id="todo-count" class="todo-count"></span>"#),
        "{page}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_failing_read_keeps_the_last_list_and_logs_once_each_way() {
    let logs = CapturedLogs::start();
    let service = FakeServer::start().await;
    let questions = service.questions();

    // No list yet and the service down: the page says it can't read, with no count.
    service.fake.0.lock().unwrap().list_fails = true;
    let section = questions.refresh().await;
    let html = section.render().unwrap();
    assert!(
        html.contains("Harold can't read the todos right now."),
        "{html}"
    );
    assert_eq!(
        section.count().render().unwrap(),
        r#"<span id="todo-count" class="todo-count"></span>"#
    );
    questions.refresh().await;
    assert_eq!(
        logs.text().matches("app todos not read").count(),
        1,
        "{}",
        logs.text()
    );
    assert!(
        logs.text().contains("reason=\"unavailable\"")
            || logs.text().contains("reason=unavailable"),
        "{}",
        logs.text()
    );

    // A good read draws the list; a later failure keeps it.
    {
        let mut state = service.fake.0.lock().unwrap();
        state.list_fails = false;
        state.list = vec![question("Q1", 1, false)];
    }
    let html = questions.refresh().await.render().unwrap();
    assert!(html.contains(r#"id="todo-Q1""#), "{html}");
    assert_eq!(logs.text().matches("app todos read again").count(), 1);
    service.fake.0.lock().unwrap().list_fails = true;
    let kept = questions.refresh().await.render().unwrap();
    assert!(kept.contains(r#"id="todo-Q1""#), "{kept}");
    questions.refresh().await;
    assert_eq!(
        logs.text().matches("app todos not read").count(),
        2,
        "{}",
        logs.text()
    );
    service.fake.0.lock().unwrap().list_fails = false;
    questions.refresh().await;
    questions.refresh().await;
    assert_eq!(logs.text().matches("app todos read again").count(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_stream_sends_the_todos_and_the_count_when_the_list_changes() {
    let service = FakeServer::start().await;
    let directory = TestDirectory::new();
    let server = TestServer::serve(directory.store().await, service.questions()).await;
    let mut response = server.get("/updates").await;
    assert_eq!(response.status(), 200);
    let mut seen = String::new();
    // The first event is the thread; the todos are not sent while they are as the page drew them.
    service.fake.set_list(vec![question("Q1", 1, false)]);

    let patch = tokio::time::timeout(std::time::Duration::from_secs(6), async {
        loop {
            let chunk = response.chunk().await.unwrap().expect("the stream goes on");
            seen.push_str(&String::from_utf8_lossy(&chunk));
            if seen.contains(r#"id="todo-Q1""#) {
                return seen.clone();
            }
        }
    })
    .await
    .expect("a todos patch within the poll interval and a margin");

    assert!(
        patch.contains(r#"<span id="todo-count" class="todo-count">1</span>"#),
        "{patch}"
    );
    assert!(patch.contains(r#"<section id="todos""#), "{patch}");
}
