use std::sync::Arc;
use std::sync::atomic::Ordering;

use harold_api::tools::questions::v1 as pb;
use harold_api::tools::questions::v1::question_service_server::QuestionService;
use tokio::sync::Notify;
use tonic::{Code, Request};

use super::*;
use crate::tools::questions::test_support::*;

fn service<S: QuestionStore>(store: Arc<S>) -> QuestionToolService<S> {
    QuestionToolService::new(store, Arc::new(Notify::new()))
}

async fn put<S: QuestionStore>(
    service: &QuestionToolService<S>,
    question: pb::Question,
) -> Result<pb::PutQuestionResponse, tonic::Status> {
    service
        .put_question(Request::new(pb::PutQuestionRequest {
            question: Some(question),
        }))
        .await
        .map(|response| response.into_inner())
}

async fn list<S: QuestionStore>(service: &QuestionToolService<S>) -> pb::ListQuestionsResponse {
    service
        .list_questions(Request::new(pb::ListQuestionsRequest {}))
        .await
        .unwrap()
        .into_inner()
}

async fn answer<S: QuestionStore>(
    service: &QuestionToolService<S>,
    id: &str,
    version: u32,
    kind: pb::AnswerKind,
    picks: Vec<pb::Pick>,
    note: &str,
) -> Result<(), tonic::Status> {
    service
        .answer_question(Request::new(pb::AnswerQuestionRequest {
            id: id.into(),
            version,
            kind: kind as i32,
            picks,
            note: note.into(),
            send_now: false,
        }))
        .await
        .map(|_| ())
}

async fn answer_marked<S: QuestionStore>(
    service: &QuestionToolService<S>,
    id: &str,
    send_now: bool,
) -> Result<(), tonic::Status> {
    service
        .answer_question(Request::new(pb::AnswerQuestionRequest {
            id: id.into(),
            version: 1,
            kind: pb::AnswerKind::Answer as i32,
            picks: vec![pick(0, "a")],
            note: String::new(),
            send_now,
        }))
        .await
        .map(|_| ())
}

async fn dismiss<S: QuestionStore>(
    service: &QuestionToolService<S>,
    id: &str,
    reason: &str,
) -> Result<(), tonic::Status> {
    service
        .dismiss_question(Request::new(pb::DismissQuestionRequest {
            id: id.into(),
            reason: reason.into(),
        }))
        .await
        .map(|_| ())
}

async fn defer<S: QuestionStore>(
    service: &QuestionToolService<S>,
    id: &str,
) -> Result<(), tonic::Status> {
    service
        .defer_question(Request::new(pb::DeferQuestionRequest { id: id.into() }))
        .await
        .map(|_| ())
}

fn listed(response: &pb::ListQuestionsResponse) -> Vec<(String, u32, bool)> {
    response
        .questions
        .iter()
        .map(|q| {
            (
                q.question.as_ref().unwrap().id.clone(),
                q.version,
                q.deferred,
            )
        })
        .collect()
}

#[tokio::test]
async fn put_list_answer_round_trip() {
    let folder = TempFolder::new();
    let wake = Arc::new(Notify::new());
    let service = QuestionToolService::new(open_store(&folder).await, Arc::clone(&wake));

    let stored = put(&service, question("Q1", "spec-1", "abc123"))
        .await
        .unwrap();
    assert_eq!((stored.id.as_str(), stored.version), ("Q1", 1));
    put(&service, question("Q2", "spec-2", "def456"))
        .await
        .unwrap();

    // A successful write leaves a wake for the batch sender.
    tokio::time::timeout(std::time::Duration::from_secs(1), wake.notified())
        .await
        .expect("a write wakes the sender");

    let listing = list(&service).await;
    assert_eq!(
        listed(&listing),
        vec![("Q1".into(), 1, false), ("Q2".into(), 1, false)]
    );
    assert_eq!(listing.waiting, 2);
    assert_eq!(
        listing.questions[0].question.as_ref().unwrap().spec,
        "spec-1"
    );

    answer(
        &service,
        "Q1",
        1,
        pb::AnswerKind::Answer,
        vec![pick(0, "a")],
        "",
    )
    .await
    .unwrap();
    defer(&service, "Q2").await.unwrap();
    let listing = list(&service).await;
    assert_eq!(listed(&listing), vec![("Q2".into(), 1, true)]);
    assert_eq!(listing.waiting, 1);

    let again = put(&service, question("Q2", "spec-2", "fff000"))
        .await
        .unwrap();
    assert_eq!(again.version, 2);

    service
        .reorder_questions(Request::new(pb::ReorderQuestionsRequest {
            ids: vec!["Q3".into(), "Q2".into()],
        }))
        .await
        .unwrap();
    dismiss(&service, "Q2", "not needed").await.unwrap();
    let listing = list(&service).await;
    assert!(listing.questions.is_empty());
    assert_eq!(listing.waiting, 0);
    assert_eq!(listing.dismissed.len(), 1);
    assert_eq!(listing.dismissed[0].id, "Q2");
    assert_eq!(listing.dismissed[0].reason, "not needed");
    println!(
        "evidence: put Q1 v1, answered; Q2 deferred then put again as v2; dismissed Q2 listed with its reason; waiting = {}",
        listing.waiting
    );
}

#[tokio::test]
async fn a_broken_rule_is_invalid_argument_naming_the_field_and_stores_nothing() {
    let folder = TempFolder::new();
    let service = service(open_store(&folder).await);

    let mut bad_link = question("Q1", "spec", "abc");
    bad_link.link = "ftp://example.com".into();
    let refused = put(&service, bad_link).await.unwrap_err();
    assert_eq!(refused.code(), Code::InvalidArgument);
    assert_eq!(refused.message(), "invalid link");

    let refused = put(&service, question("Q 1", "spec", "abc"))
        .await
        .unwrap_err();
    assert_eq!(
        (refused.code(), refused.message()),
        (Code::InvalidArgument, "invalid id")
    );

    let refused = service
        .put_question(Request::new(pb::PutQuestionRequest { question: None }))
        .await
        .unwrap_err();
    assert_eq!(
        (refused.code(), refused.message()),
        (Code::InvalidArgument, "invalid question")
    );
    assert!(list(&service).await.questions.is_empty());

    put(&service, question("Q2", "spec", "abc")).await.unwrap();
    let refused = answer(
        &service,
        "Q2",
        1,
        pb::AnswerKind::Answer,
        vec![pick(0, "nope")],
        "",
    )
    .await
    .unwrap_err();
    assert_eq!(
        (refused.code(), refused.message()),
        (Code::InvalidArgument, "invalid picks")
    );
    let refused = answer(&service, "Q2", 1, pb::AnswerKind::Revise, vec![], "  ")
        .await
        .unwrap_err();
    assert_eq!(
        (refused.code(), refused.message()),
        (Code::InvalidArgument, "invalid note")
    );
    let refused = answer(
        &service,
        "Q2",
        1,
        pb::AnswerKind::Unspecified,
        vec![pick(0, "a")],
        "",
    )
    .await
    .unwrap_err();
    assert_eq!(
        (refused.code(), refused.message()),
        (Code::InvalidArgument, "invalid kind")
    );
    assert_eq!(listed(&list(&service).await), vec![("Q2".into(), 1, false)]);
    println!(
        "evidence: bad link -> invalid link, bad id -> invalid id, no question -> invalid question, bad picks -> invalid picks, blank revise note -> invalid note, unspecified kind -> invalid kind; nothing stored"
    );
}

#[tokio::test]
async fn version_changed_and_not_waiting_reach_the_caller() {
    let folder = TempFolder::new();
    let service = service(open_store(&folder).await);
    put(&service, question("Q1", "spec", "abc")).await.unwrap();
    put(&service, question("Q1", "spec", "abd")).await.unwrap();

    let refused = answer(
        &service,
        "Q1",
        1,
        pb::AnswerKind::Answer,
        vec![pick(0, "a")],
        "",
    )
    .await
    .unwrap_err();
    assert_eq!(
        (refused.code(), refused.message()),
        (Code::FailedPrecondition, "version_changed")
    );

    dismiss(&service, "Q1", "withdrawn").await.unwrap();
    let refused = answer(
        &service,
        "Q1",
        2,
        pb::AnswerKind::Answer,
        vec![pick(0, "a")],
        "",
    )
    .await
    .unwrap_err();
    assert_eq!(
        (refused.code(), refused.message()),
        (Code::FailedPrecondition, "not_waiting")
    );
    let refused = dismiss(&service, "Q1", "again").await.unwrap_err();
    assert_eq!(
        (refused.code(), refused.message()),
        (Code::FailedPrecondition, "not_waiting")
    );
    let refused = defer(&service, "Q9").await.unwrap_err();
    assert_eq!(
        (refused.code(), refused.message()),
        (Code::FailedPrecondition, "not_waiting")
    );
    println!(
        "evidence: answer at v1 after v2 -> FailedPrecondition version_changed; answer, dismiss after dismissal and defer of an unknown id -> FailedPrecondition not_waiting"
    );
}

#[tokio::test]
async fn a_failing_write_is_unavailable() {
    let folder = TempFolder::new();
    let inner = open_store(&folder).await;
    let failing = FailingStore::new(Arc::clone(&inner));
    failing.writes.store(false, Ordering::SeqCst);
    let service = service(Arc::clone(&failing));
    put(&service, question("Q1", "spec", "abc")).await.unwrap();
    failing.writes.store(true, Ordering::SeqCst);

    let refused = put(&service, question("Q2", "spec", "abc"))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), Code::Unavailable);
    let refused = answer(
        &service,
        "Q1",
        1,
        pb::AnswerKind::Answer,
        vec![pick(0, "a")],
        "",
    )
    .await
    .unwrap_err();
    assert_eq!(refused.code(), Code::Unavailable);
    assert_eq!(
        dismiss(&service, "Q1", "x").await.unwrap_err().code(),
        Code::Unavailable
    );
    assert_eq!(
        defer(&service, "Q1").await.unwrap_err().code(),
        Code::Unavailable
    );
    let reorder = service
        .reorder_questions(Request::new(pb::ReorderQuestionsRequest {
            ids: vec!["Q1".into()],
        }))
        .await
        .unwrap_err();
    assert_eq!(reorder.code(), Code::Unavailable);

    // Reads still work, and nothing changed.
    assert_eq!(listed(&list(&service).await), vec![("Q1".into(), 1, false)]);
    println!(
        "evidence: put, answer, dismiss, defer and reorder on a failing database -> Unavailable ({:?}); Q1 still waiting at v1",
        refused.message()
    );
}

#[tokio::test]
async fn log_lines_carry_only_tool_id_version_reason() {
    let (logs, _guard) = capture_logs();
    let folder = TempFolder::new();
    let service = service(open_store(&folder).await);
    let mut canary = question("Q1", "spec", "abc");
    canary.intro = "QUEUE-CANARY-2207 intro".into();
    put(&service, canary).await.unwrap();
    put(&service, question("Q2", "spec", "abc")).await.unwrap();
    put(&service, question("Q3", "spec", "abc")).await.unwrap();
    answer(
        &service,
        "Q1",
        1,
        pb::AnswerKind::Revise,
        vec![],
        "QUEUE-CANARY-2207 make it bigger",
    )
    .await
    .unwrap();
    answer(
        &service,
        "Q2",
        1,
        pb::AnswerKind::FreeText,
        vec![free_pick(0, "z", "QUEUE-CANARY-2207 typed")],
        "",
    )
    .await
    .unwrap();
    dismiss(&service, "Q3", "QUEUE-CANARY-2207 reason")
        .await
        .unwrap();

    let lines = logs.lines();
    assert_eq!(logs.with("question stored").len(), 3, "{lines:#?}");
    assert_eq!(logs.with("question answered").len(), 2, "{lines:#?}");
    assert_eq!(logs.with("question dismissed").len(), 1, "{lines:#?}");
    for message in ["question stored", "question answered", "question dismissed"] {
        for line in logs.with(message) {
            let names = field_names(&line, message);
            assert_eq!(names, ["tool", "question_id", "version"], "{line}");
            assert!(line.contains("tool=\"questions\""), "{line}");
        }
    }
    assert!(
        lines.iter().all(|line| !line.contains("QUEUE-CANARY-2207")),
        "{lines:#?}"
    );
    for line in &lines {
        println!("evidence: log line: {line}");
    }
    println!(
        "evidence: {} log lines, none holds QUEUE-CANARY-2207",
        lines.len()
    );
}

#[tokio::test]
async fn answer_question_passes_send_now_to_the_store() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    let service = service(Arc::clone(&store));
    put(&service, question("Q1", "spec-1", "abc123"))
        .await
        .unwrap();
    answer_marked(&service, "Q1", true).await.unwrap();
    assert!(store.marked("Q1").await);
}

#[tokio::test]
async fn an_old_client_omitting_send_now_records_unmarked() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    let service = service(Arc::clone(&store));
    put(&service, question("Q1", "spec-1", "abc123"))
        .await
        .unwrap();
    // An old client's bytes carry no field 6.
    let old = pb::AnswerQuestionRequest {
        id: "Q1".into(),
        version: 1,
        kind: pb::AnswerKind::Answer as i32,
        picks: vec![pick(0, "a")],
        note: String::new(),
        send_now: false,
    };
    let bytes = prost::Message::encode_to_vec(&old);
    let request = <pb::AnswerQuestionRequest as prost::Message>::decode(bytes.as_slice()).unwrap();
    service
        .answer_question(Request::new(request))
        .await
        .unwrap();
    assert!(!store.marked("Q1").await);
}
