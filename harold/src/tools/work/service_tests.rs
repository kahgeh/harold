use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use harold_api::tools::questions::v1 as qp;
use harold_api::tools::questions::v1::question_service_server::{
    QuestionService, QuestionServiceServer,
};
use harold_api::tools::work::v1 as pb;
use harold_api::tools::work::v1::work_service_server::WorkService;
use tonic::{Code, Request, Response};

use super::*;
use crate::tools::backlog::guard::Guard;
use crate::tools::work::files::WorkFiles;
use crate::tools::work::import::{OpenQueue, QueueItem, QueueUnreadable};
use crate::tools::work::rotation::{Clock, Rotator};
use crate::tools::work::test_support::TempFolder;
use crate::tools::{BoxFuture, ChiefPane, PaneFolder, Products};

const CHIEF: &str = "%2";
const OTHER: &str = "%9";
/// Noon UTC of 2026-11-03.
const NOW_MS: i64 = 1_793_707_200_000;

struct FixedClock;

impl Clock for FixedClock {
    fn now_ms(&self) -> i64 {
        NOW_MS
    }
}

struct Chief;

impl ChiefPane for Chief {
    fn chief_pane(&self) -> Option<String> {
        Some(CHIEF.into())
    }
}

struct NoFolders;

impl PaneFolder for NoFolders {
    fn folder_of(&self, _pane_id: &str) -> Option<PathBuf> {
        None
    }
}

struct Registry(Result<Vec<(String, PathBuf)>, &'static str>);

impl Products for Registry {
    fn products(&self) -> Result<Vec<(String, PathBuf)>, &'static str> {
        self.0.clone()
    }
}

/// Question states from a set map. An id not in it is `Missing`.
#[derive(Default)]
struct FakeQuestions(HashMap<String, QuestionFact>);

impl QuestionStates for FakeQuestions {
    fn states<'a>(&'a self, ids: &'a [String]) -> BoxFuture<'a, HashMap<String, QuestionFact>> {
        Box::pin(async move {
            ids.iter()
                .map(|id| {
                    let fact = self.0.get(id).copied().unwrap_or(QuestionFact::Missing);
                    (id.clone(), fact)
                })
                .collect()
        })
    }
}

/// A work folder and a product `p` with its repo in a temp folder.
struct Setup {
    tmp: TempFolder,
}

impl Setup {
    fn new() -> Self {
        let tmp = TempFolder::new();
        std::fs::create_dir_all(tmp.0.join("p/.superpowers/pod/specs")).unwrap();
        Self { tmp }
    }

    fn pod_file(&self, folder: &str, id: &str) {
        let folder = self.tmp.0.join("p/.superpowers/pod").join(folder);
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join(format!("{id}.md")), "x").unwrap();
    }

    fn service_with(&self, questions: impl QuestionStates + 'static) -> WorkToolService {
        let files = WorkFiles::new(self.tmp.0.join("work"));
        let clock: Arc<dyn Clock> = Arc::new(FixedClock);
        let rotator = Arc::new(Rotator::new(files.clone(), Arc::clone(&clock)));
        let guard = Guard::new(
            Arc::new(NoFolders),
            Arc::new(Registry(Ok(vec![("p".into(), self.tmp.0.join("p"))]))),
            Arc::new(Chief),
        );
        WorkToolService::new(
            rotator,
            files,
            guard,
            Arc::new(questions),
            Arc::new(OneItemQueue),
            clock,
        )
    }

    fn service(&self) -> WorkToolService {
        self.service_with(FakeQuestions::default())
    }
}

fn record(name: &str) -> pb::WorkRecord {
    pb::WorkRecord {
        name: name.into(),
        title: format!("Title {name}"),
        priority: "later".into(),
        status: "idea".into(),
        ..pb::WorkRecord::default()
    }
}

fn refusal<T>(result: Result<T, tonic::Status>) -> (Code, String) {
    match result {
        Ok(_) => panic!("not refused"),
        Err(status) => (status.code(), status.message().to_string()),
    }
}

async fn put(
    service: &WorkToolService,
    pane: &str,
    record: pb::WorkRecord,
) -> Result<pb::PutResponse, tonic::Status> {
    service
        .put(Request::new(pb::PutRequest {
            record: Some(record),
            caller_pane: pane.into(),
        }))
        .await
        .map(Response::into_inner)
}

async fn set_status(service: &WorkToolService, name: &str, status: &str) {
    service
        .set(Request::new(pb::SetRequest {
            name: name.into(),
            status: Some(status.into()),
            caller_pane: CHIEF.into(),
            ..pb::SetRequest::default()
        }))
        .await
        .unwrap();
}

async fn link(service: &WorkToolService, name: &str, to: &str) -> Result<(), tonic::Status> {
    service
        .link(Request::new(pb::LinkRequest {
            name: name.into(),
            link: to.into(),
            caller_pane: CHIEF.into(),
        }))
        .await
        .map(|_| ())
}

async fn listed(service: &WorkToolService, all: bool) -> Vec<pb::WorkRecord> {
    service
        .list(Request::new(pb::ListRequest { all }))
        .await
        .unwrap()
        .into_inner()
        .records
}

#[tokio::test]
async fn a_write_from_another_pane_is_refused_with_not_chief() {
    let setup = Setup::new();
    let service = setup.service();
    put(&service, CHIEF, record("a")).await.unwrap();

    let denied = (Code::PermissionDenied, "not_chief".to_string());
    assert_eq!(refusal(put(&service, OTHER, record("b")).await), denied);
    assert_eq!(
        refusal(
            service
                .set(Request::new(pb::SetRequest {
                    name: "a".into(),
                    priority: Some("focus".into()),
                    caller_pane: OTHER.into(),
                    ..pb::SetRequest::default()
                }))
                .await
        ),
        denied
    );
    assert_eq!(
        refusal(
            service
                .decide(Request::new(pb::DecideRequest {
                    name: "a".into(),
                    mark: "draft".into(),
                    who: "the chief".into(),
                    text: "t".into(),
                    caller_pane: OTHER.into(),
                }))
                .await
        ),
        denied
    );
    assert_eq!(
        refusal(
            service
                .mark(Request::new(pb::MarkRequest {
                    name: "a".into(),
                    number: 1,
                    mark: "agreed".into(),
                    caller_pane: OTHER.into(),
                }))
                .await
        ),
        denied
    );
    for unlink in [false, true] {
        let request = || pb::LinkRequest {
            name: "a".into(),
            link: "note:x".into(),
            caller_pane: OTHER.into(),
        };
        let result = match unlink {
            false => service.link(Request::new(request())).await.map(|_| ()),
            true => {
                let pb::LinkRequest {
                    name,
                    link,
                    caller_pane,
                } = request();
                service
                    .unlink(Request::new(pb::UnlinkRequest {
                        name,
                        link,
                        caller_pane,
                    }))
                    .await
                    .map(|_| ())
            }
        };
        assert_eq!(refusal(result), denied);
    }
    assert_eq!(
        refusal(put(&service, "", record("b")).await),
        (Code::PermissionDenied, "no_pane".to_string())
    );
    // Nothing changed.
    let records = listed(&service, true).await;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].priority, "later");
    assert!(records[0].decisions.is_empty() && records[0].links.is_empty());
}

#[tokio::test]
async fn a_read_from_any_pane_works() {
    let setup = Setup::new();
    let service = setup.service();
    put(&service, CHIEF, record("a")).await.unwrap();

    assert_eq!(listed(&service, false).await.len(), 1);
    let shown = service
        .show(Request::new(pb::ShowRequest { name: "a".into() }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(shown.record.unwrap().name, "a");
    let history = service
        .history(Request::new(pb::HistoryRequest { name: "a".into() }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(history.file, "work-2026-11-03.db");
    let found = service
        .search(Request::new(pb::SearchRequest {
            word: "title".into(),
        }))
        .await
        .unwrap()
        .into_inner()
        .found;
    assert_eq!(found.len(), 1);
    let checked = service
        .check(Request::new(pb::CheckRequest {}))
        .await
        .unwrap()
        .into_inner();
    assert!(checked.mismatches.is_empty());
    let dry = service
        .rotate(Request::new(pb::RotateRequest { dry_run: true }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(dry.open_records, 1);
    assert_eq!(dry.records, ["a"]);
    assert_eq!(
        refusal(
            service
                .show(Request::new(pb::ShowRequest { name: "b".into() }))
                .await
        ),
        (Code::NotFound, "not_found".to_string())
    );
    assert_eq!(
        refusal(
            service
                .search(Request::new(pb::SearchRequest { word: " ".into() }))
                .await
        ),
        (Code::InvalidArgument, "invalid word".to_string())
    );
}

#[tokio::test]
async fn rotate_without_dry_run_is_refused() {
    let setup = Setup::new();
    let service = setup.service();
    put(&service, CHIEF, record("a")).await.unwrap();
    let before: Vec<_> = std::fs::read_dir(setup.tmp.0.join("work"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();

    let refused = refusal(
        service
            .rotate(Request::new(pb::RotateRequest { dry_run: false }))
            .await,
    );

    assert_eq!(
        refused,
        (Code::FailedPrecondition, "rotation_is_harolds".to_string())
    );
    let after: Vec<_> = std::fs::read_dir(setup.tmp.0.join("work"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(before, after);
}

/// A queue with one open item, N12.
struct OneItemQueue;

impl OpenQueue for OneItemQueue {
    fn open_items(&self) -> BoxFuture<'_, Result<Vec<QueueItem>, QueueUnreadable>> {
        Box::pin(async {
            Ok(vec![QueueItem {
                id: "N12".into(),
                text: "Fix the bar".into(),
            }])
        })
    }
}

/// A folder of the chief's files with one file, `good.md`.
fn chief_folder(setup: &Setup, text: &str) -> String {
    let folder = setup.tmp.0.join("chief");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("good.md"), text).unwrap();
    folder.display().to_string()
}

const GOOD: &str = "# good-one: Good\nPriority: next\nStatus: idea\n";

async fn import_call(
    service: &WorkToolService,
    folder: &str,
    queue: &[&str],
    apply: bool,
    pane: &str,
) -> Result<pb::ImportResponse, tonic::Status> {
    service
        .import(Request::new(pb::ImportRequest {
            folder: folder.into(),
            queue: queue.iter().map(|id| (*id).to_string()).collect(),
            apply,
            caller_pane: pane.into(),
        }))
        .await
        .map(Response::into_inner)
}

#[tokio::test]
async fn an_applied_import_from_another_pane_or_no_pane_is_refused() {
    let setup = Setup::new();
    let service = setup.service();
    let folder = chief_folder(&setup, GOOD);
    for (pane, reason) in [(OTHER, "not_chief"), ("", "no_pane")] {
        let result = import_call(&service, &folder, &[], true, pane).await;
        assert_eq!(
            refusal(result),
            (Code::PermissionDenied, reason.to_string())
        );
    }
    assert!(listed(&service, true).await.is_empty());
}

#[tokio::test]
async fn a_dry_run_import_needs_no_pane_and_stores_nothing() {
    let setup = Setup::new();
    let service = setup.service();
    let folder = chief_folder(&setup, &format!("{GOOD}Owner: x\n"));

    let dry = import_call(&service, &folder, &["n12"], false, "")
        .await
        .unwrap();

    let names: Vec<&str> = dry.records.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, ["good-one", "queue-n12"]);
    assert_eq!(dry.records[1].next_step, "Fix the bar");
    assert_eq!(dry.unplaced.len(), 1);
    assert_eq!(
        (dry.unplaced[0].file.as_str(), dry.unplaced[0].line),
        ("good.md", 4)
    );
    assert!(listed(&service, true).await.is_empty());
}

#[tokio::test]
async fn an_applied_import_from_the_chief_stores_the_records() {
    let setup = Setup::new();
    let service = setup.service();
    let folder = chief_folder(&setup, GOOD);

    let applied = import_call(&service, &folder, &["N12"], true, CHIEF)
        .await
        .unwrap();

    assert_eq!(applied.records.len(), 2);
    let names: Vec<String> = listed(&service, true)
        .await
        .into_iter()
        .map(|r| r.name)
        .collect();
    assert_eq!(names, ["good-one", "queue-n12"]);

    // A second apply finds both names in use and stores nothing more.
    let again = import_call(&service, &folder, &["N12"], true, CHIEF).await;
    assert_eq!(
        refusal(again),
        (
            Code::FailedPrecondition,
            "name_in_use good-one, queue-n12".to_string()
        )
    );
    let dry = import_call(&service, &folder, &[], false, "")
        .await
        .unwrap();
    assert_eq!(dry.in_use, ["good-one"]);
}

#[tokio::test]
async fn import_refusals_name_their_reason() {
    let setup = Setup::new();
    let service = setup.service();
    let good = chief_folder(&setup, GOOD);
    let odd = {
        let folder = setup.tmp.0.join("odd");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("odd.md"), format!("{GOOD}Owner: x\n")).unwrap();
        folder.display().to_string()
    };
    let cases: [(&str, &[&str], bool, Code, &str); 5] = [
        (
            "relative",
            &[],
            false,
            Code::InvalidArgument,
            "invalid folder",
        ),
        (&good, &[""], false, Code::InvalidArgument, "invalid queue"),
        (
            &good,
            &["N99"],
            false,
            Code::NotFound,
            "not_open_queue_item N99",
        ),
        (
            &odd,
            &[],
            true,
            Code::FailedPrecondition,
            "unplaced_lines 1",
        ),
        (
            "/no/such/folder",
            &[],
            false,
            Code::InvalidArgument,
            "invalid folder",
        ),
    ];
    for (folder, queue, apply, code, message) in cases {
        let result = import_call(&service, folder, queue, apply, CHIEF).await;
        assert_eq!(refusal(result), (code, message.to_string()), "{folder}");
    }

    // A third open work record in focus refuses the whole import.
    let focus = setup.tmp.0.join("focus");
    std::fs::create_dir_all(&focus).unwrap();
    for name in ["f-1", "f-2", "f-3"] {
        std::fs::write(
            focus.join(format!("{name}.md")),
            format!("# {name}: F\nPriority: focus\nStatus: idea\n"),
        )
        .unwrap();
    }
    let result = import_call(&service, &focus.display().to_string(), &[], true, CHIEF).await;
    assert_eq!(
        refusal(result),
        (Code::FailedPrecondition, "focus_full f-3".to_string())
    );
    assert!(listed(&service, true).await.is_empty());
}

#[tokio::test]
async fn check_over_fake_facts_returns_lines() {
    let setup = Setup::new();
    setup.pod_file("specs", "s1");
    setup.pod_file("merged", "s1");
    setup.pod_file("specs", "s2");
    let questions = FakeQuestions(HashMap::from([
        ("Q1".to_string(), QuestionFact::Waiting),
        ("Q2".to_string(), QuestionFact::Answered),
        ("Q3".to_string(), QuestionFact::Unreadable),
    ]));
    let service = setup.service_with(questions);
    for name in ["behind", "waits", "clean", "gone"] {
        put(&service, CHIEF, record(name)).await.unwrap();
    }
    link(&service, "behind", "spec:p/s1").await.unwrap();
    set_status(&service, "behind", "building").await;
    link(&service, "waits", "question:Q1").await.unwrap();
    set_status(&service, "waits", "approved").await;
    link(&service, "clean", "spec:p/s2").await.unwrap();
    link(&service, "clean", "question:Q2").await.unwrap();
    link(&service, "clean", "question:Q3").await.unwrap();
    link(&service, "gone", "question:Q9").await.unwrap();
    link(&service, "gone", "spec:p/s9").await.unwrap();

    let lines: Vec<(String, String, String)> = service
        .check(Request::new(pb::CheckRequest {}))
        .await
        .unwrap()
        .into_inner()
        .mismatches
        .into_iter()
        .map(|m| (m.record, m.kind, m.line))
        .collect();

    let line = |record: &str, kind: &str, text: &str| {
        (record.to_string(), kind.to_string(), text.to_string())
    };
    let mut expected = vec![
        line(
            "behind",
            "merged_status_behind",
            "behind is building but spec p/s1 is merged.",
        ),
        line(
            "waits",
            "waiting_question",
            "waits is approved but question Q1 still waits.",
        ),
        line(
            "gone",
            "missing_link",
            "gone links spec p/s9, which does not exist.",
        ),
        line(
            "gone",
            "missing_link",
            "gone links question Q9, which does not exist.",
        ),
    ];
    let mut lines = lines;
    lines.sort();
    expected.sort();
    assert_eq!(lines, expected);
}

#[tokio::test]
async fn an_unreadable_registry_gives_no_missing_spec_line() {
    let setup = Setup::new();
    let files = WorkFiles::new(setup.tmp.0.join("work"));
    let clock: Arc<dyn Clock> = Arc::new(FixedClock);
    let guard = Guard::new(
        Arc::new(NoFolders),
        Arc::new(Registry(Err("registry_unreadable"))),
        Arc::new(Chief),
    );
    let service = WorkToolService::new(
        Arc::new(Rotator::new(files.clone(), Arc::clone(&clock))),
        files,
        guard,
        Arc::new(FakeQuestions::default()),
        Arc::new(OneItemQueue),
        clock,
    );
    put(&service, CHIEF, record("a")).await.unwrap();
    link(&service, "a", "spec:p/s9").await.unwrap();

    let checked = service
        .check(Request::new(pb::CheckRequest {}))
        .await
        .unwrap()
        .into_inner();

    assert!(checked.mismatches.is_empty(), "{:?}", checked.mismatches);
}

#[tokio::test]
async fn put_set_decide_mark_link_list_round_trip_over_the_service() {
    let setup = Setup::new();
    let service = setup.service();

    let created = put(&service, CHIEF, record("a")).await.unwrap();
    assert!(created.created);
    let mut b = record("b");
    b.priority = "focus".into();
    put(&service, CHIEF, b).await.unwrap();
    service
        .set(Request::new(pb::SetRequest {
            name: "a".into(),
            priority: Some("focus".into()),
            status: Some("building".into()),
            waits_for: Some("the user".into()),
            next_step: Some("merge it".into()),
            caller_pane: CHIEF.into(),
            ..pb::SetRequest::default()
        }))
        .await
        .unwrap();
    let decided = service
        .decide(Request::new(pb::DecideRequest {
            name: "a".into(),
            mark: "open".into(),
            who: "the user".into(),
            text: "use turso".into(),
            caller_pane: CHIEF.into(),
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(decided.number, 1);
    service
        .mark(Request::new(pb::MarkRequest {
            name: "a".into(),
            number: 1,
            mark: "agreed".into(),
            caller_pane: CHIEF.into(),
        }))
        .await
        .unwrap();
    link(&service, "a", "spec:p/s1").await.unwrap();
    link(&service, "a", "note:keep it small").await.unwrap();
    service
        .unlink(Request::new(pb::UnlinkRequest {
            name: "a".into(),
            link: "note:keep it small".into(),
            caller_pane: CHIEF.into(),
        }))
        .await
        .unwrap();

    let listing = service
        .list(Request::new(pb::ListRequest { all: false }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(listing.file, "work-2026-11-03.db");
    let a = listing
        .records
        .iter()
        .find(|r| r.name == "a")
        .expect("a listed");
    assert_eq!(
        (
            a.priority.as_str(),
            a.status.as_str(),
            a.waits_for.as_str(),
            a.next_step.as_str()
        ),
        ("focus", "building", "the user", "merge it")
    );
    assert_eq!(a.created_in, "work-2026-11-03.db");
    assert_eq!(
        a.decisions,
        [pb::Decision {
            at: "2026-11-03".into(),
            who: "the user".into(),
            mark: "agreed".into(),
            text: "use turso".into(),
        }]
    );
    assert_eq!(a.links, ["spec:p/s1"]);

    // A third focus is refused.
    let mut c = record("c");
    c.priority = "focus".into();
    assert_eq!(
        refusal(put(&service, CHIEF, c).await),
        (Code::FailedPrecondition, "focus_full".to_string())
    );
    // A closed name stays in use.
    set_status(&service, "b", "checked by the user").await;
    assert_eq!(
        refusal(put(&service, CHIEF, record("b")).await),
        (Code::FailedPrecondition, "name_in_use".to_string())
    );
    assert_eq!(listed(&service, false).await.len(), 1);
    assert_eq!(listed(&service, true).await.len(), 2);
}

#[tokio::test]
async fn bad_fields_are_refused_before_the_store() {
    let setup = Setup::new();
    let service = setup.service();
    put(&service, CHIEF, record("a")).await.unwrap();

    let invalid = |field: &str| (Code::InvalidArgument, format!("invalid {field}"));
    assert_eq!(
        refusal(link(&service, "a", "spec:no-slash").await),
        invalid("link")
    );
    assert_eq!(refusal(link(&service, "a", "web:x").await), invalid("link"));
    let mut bad = record("Bad Name");
    assert_eq!(
        refusal(put(&service, CHIEF, bad.clone()).await),
        invalid("name")
    );
    bad = record("x");
    bad.priority = "now".into();
    assert_eq!(
        refusal(put(&service, CHIEF, bad).await),
        invalid("priority")
    );
    let mut bad = record("x");
    bad.status = "done".into();
    assert_eq!(refusal(put(&service, CHIEF, bad).await), invalid("status"));
    let mut bad = record("x");
    bad.title = "two\nlines".into();
    assert_eq!(refusal(put(&service, CHIEF, bad).await), invalid("title"));
    let mut bad = record("x");
    bad.links = vec!["nothing".into()];
    assert_eq!(refusal(put(&service, CHIEF, bad).await), invalid("link"));
    assert_eq!(
        refusal(
            service
                .mark(Request::new(pb::MarkRequest {
                    name: "a".into(),
                    number: 1,
                    mark: "maybe".into(),
                    caller_pane: CHIEF.into(),
                }))
                .await
        ),
        invalid("mark")
    );
    assert_eq!(
        refusal(
            service
                .decide(Request::new(pb::DecideRequest {
                    name: "a".into(),
                    mark: "draft".into(),
                    who: String::new(),
                    text: "t".into(),
                    caller_pane: CHIEF.into(),
                }))
                .await
        ),
        invalid("who")
    );
    assert_eq!(
        refusal(
            service
                .mark(Request::new(pb::MarkRequest {
                    name: "a".into(),
                    number: 4,
                    mark: "agreed".into(),
                    caller_pane: CHIEF.into(),
                }))
                .await
        ),
        (Code::NotFound, "not_found".to_string())
    );
    assert!(listed(&service, false).await[0].links.is_empty());
}

// --- The question tool over loopback ---------------------------------------

/// A question service that answers `ShowQuestion` from a set map, and
/// nothing else.
struct FakeQuestionService(HashMap<String, Result<String, tonic::Status>>);

#[tonic::async_trait]
impl QuestionService for FakeQuestionService {
    async fn put_question(
        &self,
        _: Request<qp::PutQuestionRequest>,
    ) -> Result<Response<qp::PutQuestionResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("fake"))
    }
    async fn dismiss_question(
        &self,
        _: Request<qp::DismissQuestionRequest>,
    ) -> Result<Response<qp::DismissQuestionResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("fake"))
    }
    async fn reorder_questions(
        &self,
        _: Request<qp::ReorderQuestionsRequest>,
    ) -> Result<Response<qp::ReorderQuestionsResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("fake"))
    }
    async fn list_questions(
        &self,
        _: Request<qp::ListQuestionsRequest>,
    ) -> Result<Response<qp::ListQuestionsResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("fake"))
    }
    async fn answer_question(
        &self,
        _: Request<qp::AnswerQuestionRequest>,
    ) -> Result<Response<qp::AnswerQuestionResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("fake"))
    }
    async fn tick_step(
        &self,
        _: Request<qp::TickStepRequest>,
    ) -> Result<Response<qp::TickStepResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("fake"))
    }
    async fn save_picks(
        &self,
        _: Request<qp::SavePicksRequest>,
    ) -> Result<Response<qp::SavePicksResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("fake"))
    }
    async fn show_question(
        &self,
        request: Request<qp::ShowQuestionRequest>,
    ) -> Result<Response<qp::ShowQuestionResponse>, tonic::Status> {
        let id = request.into_inner().id;
        match self.0.get(&id) {
            Some(Ok(state)) if state == "hang" => std::future::pending().await,
            Some(Ok(state)) => Ok(Response::new(qp::ShowQuestionResponse {
                state: state.clone(),
                ..qp::ShowQuestionResponse::default()
            })),
            Some(Err(status)) => Err(status.clone()),
            None => Err(tonic::Status::not_found("unknown_question")),
        }
    }
    async fn mark_checked(
        &self,
        _: Request<qp::MarkCheckedRequest>,
    ) -> Result<Response<qp::MarkCheckedResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("fake"))
    }
}

/// Serves the fake on a loopback port of this test, and gives its address.
async fn serve(fake: FakeQuestionService) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (sender, connections) = tokio::sync::mpsc::channel(4);
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            if sender.send(Ok::<_, std::io::Error>(socket)).await.is_err() {
                break;
            }
        }
    });
    let incoming = tokio_stream::wrappers::ReceiverStream::new(connections);
    tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(QuestionServiceServer::new(fake))
            .serve_with_incoming(incoming),
    );
    address
}

#[tokio::test]
async fn show_question_gives_the_four_question_states() {
    let address = serve(FakeQuestionService(HashMap::from([
        ("Q1".to_string(), Ok("waiting".to_string())),
        ("Q2".to_string(), Ok("answered".to_string())),
        ("Q3".to_string(), Ok("dismissed".to_string())),
        (
            "Q4".to_string(),
            Err(tonic::Status::unavailable("the database write failed")),
        ),
        ("Q5".to_string(), Err(tonic::Status::not_found("other"))),
    ])))
    .await;
    let ids: Vec<String> = ["Q1", "Q2", "Q3", "Q4", "Q5", "Q9"]
        .iter()
        .map(|id| id.to_string())
        .collect();

    let states = LoopbackQuestions::new(Some(address)).states(&ids).await;

    let expected = HashMap::from([
        ("Q1".to_string(), QuestionFact::Waiting),
        ("Q2".to_string(), QuestionFact::Answered),
        ("Q3".to_string(), QuestionFact::Answered),
        ("Q4".to_string(), QuestionFact::Unreadable),
        ("Q5".to_string(), QuestionFact::Unreadable),
        ("Q9".to_string(), QuestionFact::Missing),
    ]);
    assert_eq!(states, expected);
}

#[tokio::test]
async fn no_address_or_no_server_makes_every_question_unreadable() {
    let ids = vec!["Q1".to_string()];
    let unreadable = HashMap::from([("Q1".to_string(), QuestionFact::Unreadable)]);

    assert_eq!(LoopbackQuestions::new(None).states(&ids).await, unreadable);
    // Nothing listens on a port that was bound and then closed.
    let closed = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap()
    };
    let started = std::time::Instant::now();
    assert_eq!(
        LoopbackQuestions::new(Some(closed)).states(&ids).await,
        unreadable
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
}

#[tokio::test]
async fn a_question_tool_that_never_answers_gives_no_line_in_bounded_time() {
    let ids = ["H1", "H2", "H3", "H4"];
    let address = serve(FakeQuestionService(
        ids.iter()
            .map(|id| (id.to_string(), Ok("hang".to_string())))
            .collect(),
    ))
    .await;
    let setup = Setup::new();
    // Four calls of 200 ms each would take 800 ms: the 500 ms limit ends it first.
    let service = setup.service_with(LoopbackQuestions::with_limits(
        Some(address),
        std::time::Duration::from_millis(200),
        std::time::Duration::from_millis(500),
    ));
    put(&service, CHIEF, record("a")).await.unwrap();
    for id in ids {
        link(&service, "a", &format!("question:{id}"))
            .await
            .unwrap();
    }
    set_status(&service, "a", "approved").await;

    let started = std::time::Instant::now();
    let checked = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        service.check(Request::new(pb::CheckRequest {})),
    )
    .await
    .expect("check ends")
    .unwrap()
    .into_inner();

    assert!(checked.mismatches.is_empty(), "{:?}", checked.mismatches);
    assert!(
        started.elapsed() < std::time::Duration::from_millis(1500),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn stored_links_that_climb_out_of_the_pod_read_no_file() {
    let setup = Setup::new();
    // Decoys: `spec:p/../../x` would read `p/.superpowers/x.md`, and `spec:p/..`
    // would read `...md` in each pod folder.
    std::fs::write(setup.tmp.0.join("p/.superpowers/x.md"), "decoy").unwrap();
    for folder in ["specs", "merged", "approvals"] {
        setup.pod_file(folder, "..");
    }
    let service = setup.service();
    put(&service, CHIEF, record("a")).await.unwrap();
    // The service refuses such links, so they are stored past it, as an old or
    // imported file could hold them.
    {
        let file = service.rotator.current().await.unwrap();
        for bad in ["spec:p/../../x", "spec:p/..", "spec:../p", "spec:p/a\\b"] {
            file.link("a", bad, NOW_MS).await.unwrap().unwrap();
        }
    }

    let kinds: Vec<String> = service
        .check(Request::new(pb::CheckRequest {}))
        .await
        .unwrap()
        .into_inner()
        .mismatches
        .into_iter()
        .map(|m| m.kind)
        .collect();

    assert!(kinds.iter().all(|kind| kind == "missing_link"), "{kinds:?}");
}

#[test]
fn unsafe_path_parts_are_refused() {
    for part in ["", ".", "..", "a/b", "a\\b", "a\0b"] {
        assert!(!safe_part(part), "{part:?}");
    }
    for part in ["s1", "work-records", "...", "a.b"] {
        assert!(safe_part(part), "{part:?}");
    }
}
