use std::sync::Arc;
use std::sync::atomic::Ordering;

use harold_api::tools::questions::v1 as pb;
use harold_api::tools::questions::v1::question_service_server::QuestionService;
use tokio::sync::Notify;
use tonic::{Code, Request};

use super::*;
use crate::tools::questions::test_support::*;

fn service<S: QuestionStore>(store: Arc<S>) -> QuestionToolService<S> {
    QuestionToolService::new(store, Arc::new(Notify::new()), no_products())
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
            trace_parent: String::new(),
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
            trace_parent: String::new(),
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

fn listed(response: &pb::ListQuestionsResponse) -> Vec<(String, u32)> {
    response
        .questions
        .iter()
        .map(|q| (q.question.as_ref().unwrap().id.clone(), q.version))
        .collect()
}

#[tokio::test]
async fn put_list_answer_round_trip() {
    let folder = TempFolder::new();
    let wake = Arc::new(Notify::new());
    let service =
        QuestionToolService::new(open_store(&folder).await, Arc::clone(&wake), no_products());

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
    assert_eq!(listed(&listing), vec![("Q1".into(), 1), ("Q2".into(), 1)]);
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
    let listing = list(&service).await;
    assert_eq!(listed(&listing), vec![("Q2".into(), 1)]);
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
        "evidence: put Q1 v1, answered; Q2 put again as v2; dismissed Q2 listed with its reason; waiting = {}",
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
    assert_eq!(listed(&list(&service).await), vec![("Q2".into(), 1)]);
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
    let refused = answer(&service, "Q9", 1, pb::AnswerKind::Relegate, vec![], "")
        .await
        .unwrap_err();
    assert_eq!(
        (refused.code(), refused.message()),
        (Code::FailedPrecondition, "not_waiting")
    );
    println!(
        "evidence: answer at v1 after v2 -> FailedPrecondition version_changed; answer and dismiss after dismissal, and relegate of an unknown id, -> FailedPrecondition not_waiting"
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
    let reorder = service
        .reorder_questions(Request::new(pb::ReorderQuestionsRequest {
            ids: vec!["Q1".into()],
        }))
        .await
        .unwrap_err();
    assert_eq!(reorder.code(), Code::Unavailable);

    // Reads still work, and nothing changed.
    assert_eq!(listed(&list(&service).await), vec![("Q1".into(), 1)]);
    println!(
        "evidence: put, answer, dismiss and reorder on a failing database -> Unavailable ({:?}); Q1 still waiting at v1",
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
            let expected: &[&str] = if message == "question answered" {
                &["tool", "question_id", "version", "kind"]
            } else {
                &["tool", "question_id", "version"]
            };
            assert_eq!(names, expected, "{line}");
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
        trace_parent: String::new(),
    };
    let bytes = prost::Message::encode_to_vec(&old);
    let request = <pb::AnswerQuestionRequest as prost::Message>::decode(bytes.as_slice()).unwrap();
    service
        .answer_question(Request::new(request))
        .await
        .unwrap();
    assert!(!store.marked("Q1").await);
}

fn instructions_proto(id: &str, steps: &[&str]) -> pb::Question {
    pb::Question {
        id: id.into(),
        product: "harold".into(),
        title: "Do things".into(),
        steps: steps
            .iter()
            .map(|text| pb::Step {
                text: (*text).into(),
                detail: String::new(),
            })
            .collect(),
        ..Default::default()
    }
}

async fn tick<S: QuestionStore>(
    service: &QuestionToolService<S>,
    id: &str,
    version: u32,
    step: u32,
    ticked: bool,
) -> Result<(), tonic::Status> {
    service
        .tick_step(Request::new(pb::TickStepRequest {
            id: id.into(),
            version,
            step,
            ticked,
        }))
        .await
        .map(|_| ())
}

#[tokio::test]
async fn put_instructions_then_list_carries_title_steps_and_ticked() {
    let folder = TempFolder::new();
    let service = service(open_store(&folder).await);
    put(&service, instructions_proto("I1", &["a", "b", "c"]))
        .await
        .unwrap();
    tick(&service, "I1", 1, 2, true).await.unwrap();
    let listing = list(&service).await;
    let item = &listing.questions[0];
    let question = item.question.as_ref().unwrap();
    assert_eq!(question.title, "Do things");
    assert_eq!(question.steps.len(), 3);
    assert_eq!(item.ticked, [2]);
}

#[tokio::test]
async fn tick_step_ticks_and_logs_a_fixed_line_without_step_text() {
    let (logs, _guard) = capture_logs();
    let folder = TempFolder::new();
    let wake = Arc::new(Notify::new());
    let service =
        QuestionToolService::new(open_store(&folder).await, Arc::clone(&wake), no_products());
    put(&service, instructions_proto("I1", &["TICK-CANARY-77 step"]))
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), wake.notified())
        .await
        .unwrap();
    tick(&service, "I1", 1, 1, true).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), wake.notified())
        .await
        .expect("a tick wakes the sender");
    tick(&service, "I1", 1, 1, false).await.unwrap();
    let lines = logs.with("step ticked");
    assert_eq!(lines.len(), 2, "{lines:#?}");
    for line in &lines {
        assert_eq!(
            field_names(line, "step ticked"),
            ["tool", "question_id", "version", "reason"],
            "{line}"
        );
    }
    assert!(lines[0].contains("reason=\"ticked\""), "{}", lines[0]);
    assert!(lines[1].contains("reason=\"unticked\""), "{}", lines[1]);
    assert!(logs.lines().iter().all(|l| !l.contains("TICK-CANARY-77")));
}

#[tokio::test]
async fn tick_step_refusals_have_exact_messages() {
    let folder = TempFolder::new();
    let service = service(open_store(&folder).await);
    put(&service, instructions_proto("I1", &["a", "b"]))
        .await
        .unwrap();
    put(&service, question("Q1", "spec", "abc")).await.unwrap();

    let stale = tick(&service, "I1", 9, 1, true).await.unwrap_err();
    assert_eq!(stale.code(), Code::FailedPrecondition);
    assert_eq!(stale.message(), "version_changed");

    for step in [0, 3] {
        let bad = tick(&service, "I1", 1, step, true).await.unwrap_err();
        assert_eq!(bad.code(), Code::InvalidArgument);
        assert_eq!(bad.message(), "invalid step");
    }
    let on_question = tick(&service, "Q1", 1, 1, true).await.unwrap_err();
    assert_eq!(on_question.code(), Code::InvalidArgument);

    answer(&service, "I1", 1, pb::AnswerKind::Done, vec![], "")
        .await
        .unwrap();
    let gone = tick(&service, "I1", 1, 1, true).await.unwrap_err();
    assert_eq!(gone.code(), Code::FailedPrecondition);
    assert_eq!(gone.message(), "not_waiting");
}

#[tokio::test]
async fn answer_done_and_ignore_for_instructions_and_refusals_for_questions() {
    let folder = TempFolder::new();
    let service = service(open_store(&folder).await);
    put(&service, instructions_proto("I1", &["a"]))
        .await
        .unwrap();
    put(&service, instructions_proto("I2", &["a"]))
        .await
        .unwrap();
    put(&service, question("Q1", "spec", "abc")).await.unwrap();
    answer(&service, "I1", 1, pb::AnswerKind::Done, vec![], "")
        .await
        .unwrap();
    answer(&service, "I2", 1, pb::AnswerKind::Ignore, vec![], "")
        .await
        .unwrap();
    for kind in [pb::AnswerKind::Done, pb::AnswerKind::Ignore] {
        let err = answer(&service, "Q1", 1, kind, vec![], "")
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::InvalidArgument);
    }
    put(&service, instructions_proto("I3", &["a"]))
        .await
        .unwrap();
    let err = answer(
        &service,
        "I3",
        1,
        pb::AnswerKind::Answer,
        vec![pick(0, "a")],
        "",
    )
    .await
    .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn listing_counts_waiting_without_instructions() {
    let folder = TempFolder::new();
    let service = service(open_store(&folder).await);
    put(&service, instructions_proto("I1", &["a"]))
        .await
        .unwrap();
    put(&service, question("Q1", "spec", "abc")).await.unwrap();
    let listing = list(&service).await;
    assert_eq!(listing.questions.len(), 2);
    assert_eq!(listing.waiting, 1);
}

async fn save_picks<S: QuestionStore>(
    service: &QuestionToolService<S>,
    id: &str,
    version: u32,
    picks: Vec<pb::Pick>,
) -> Result<(), tonic::Status> {
    service
        .save_picks(Request::new(pb::SavePicksRequest {
            id: id.into(),
            version,
            picks,
        }))
        .await
        .map(|_| ())
}

async fn show<S: QuestionStore>(
    service: &QuestionToolService<S>,
    id: &str,
) -> Result<pb::ShowQuestionResponse, tonic::Status> {
    service
        .show_question(Request::new(pb::ShowQuestionRequest { id: id.into() }))
        .await
        .map(|response| response.into_inner())
}

#[tokio::test]
async fn save_picks_stores_picks_lists_them_and_wakes_nothing() {
    let (logs, _guard) = capture_logs();
    let folder = TempFolder::new();
    let wake = Arc::new(Notify::new());
    let service =
        QuestionToolService::new(open_store(&folder).await, Arc::clone(&wake), no_products());
    put(&service, question("Q1", "spec", "abc")).await.unwrap();
    // The put left a wake permit; take it so only a save could leave another.
    wake.notified().await;
    let free = pb::Pick {
        decision: 0,
        key: "z".into(),
        text: "PICK-CANARY-91 typed".into(),
    };
    save_picks(&service, "Q1", 1, vec![free.clone()])
        .await
        .unwrap();
    let woken = tokio::time::timeout(std::time::Duration::from_millis(100), wake.notified()).await;
    assert!(woken.is_err(), "a save must not wake the answers sender");
    let listing = list(&service).await;
    assert_eq!(listing.questions[0].picks, [free]);
    let lines = logs.with("picks saved");
    assert_eq!(lines.len(), 1, "{lines:#?}");
    assert_eq!(
        field_names(&lines[0], "picks saved"),
        ["tool", "question_id", "version"]
    );
    assert!(logs.lines().iter().all(|l| !l.contains("PICK-CANARY-91")));
}

#[tokio::test]
async fn save_picks_refusals_have_exact_messages() {
    let folder = TempFolder::new();
    let service = service(open_store(&folder).await);
    put(&service, question("Q1", "spec", "abc")).await.unwrap();
    let stale = save_picks(&service, "Q1", 9, vec![]).await.unwrap_err();
    assert_eq!(stale.code(), Code::FailedPrecondition);
    assert_eq!(stale.message(), "version_changed");
    let bad = save_picks(&service, "Q1", 1, vec![pick(0, "nope")])
        .await
        .unwrap_err();
    assert_eq!(bad.code(), Code::InvalidArgument);
    assert_eq!(bad.message(), "invalid picks");
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
    let gone = save_picks(&service, "Q1", 1, vec![]).await.unwrap_err();
    assert_eq!(gone.code(), Code::FailedPrecondition);
    assert_eq!(gone.message(), "not_waiting");
}

#[tokio::test]
async fn a_failed_store_on_save_picks_is_unavailable_and_logs_picks_not_saved() {
    let (logs, _guard) = capture_logs();
    let folder = TempFolder::new();
    let failing = FailingStore::new(open_store(&folder).await);
    failing.writes.store(false, Ordering::SeqCst);
    let service = service(Arc::clone(&failing));
    put(&service, question("Q1", "spec", "abc")).await.unwrap();
    failing.writes.store(true, Ordering::SeqCst);
    let refused = save_picks(
        &service,
        "Q1",
        1,
        vec![free_pick(0, "KEY-CANARY", "TEXT-CANARY")],
    )
    .await
    .unwrap_err();
    assert_eq!(refused.code(), Code::Unavailable);
    let lines = logs.with("picks not saved");
    assert_eq!(lines.len(), 1, "{lines:#?}");
    // `error` holds the message, with spaces, so the names are read up to it.
    let (before_error, _) = lines[0].split_once(" error=").expect("error field");
    assert_eq!(
        field_names(before_error, "picks not saved"),
        ["tool", "question_id", "version", "reason", "kind"]
    );
    assert!(lines[0].contains("reason=\"store_failed\""), "{}", lines[0]);
    assert!(lines[0].contains("kind=\"data\""), "{}", lines[0]);
    assert!(lines[0].contains("write failed"), "{}", lines[0]);
    assert!(!lines[0].contains("KEY-CANARY"), "{}", lines[0]);
    assert!(!lines[0].contains("TEXT-CANARY"), "{}", lines[0]);
    assert!(lines[0].contains("WARN"), "{}", lines[0]);
}

#[tokio::test]
async fn show_question_returns_a_dismissed_questions_picks() {
    let folder = TempFolder::new();
    let service = service(open_store(&folder).await);
    put(&service, question("Q1", "spec", "abc")).await.unwrap();
    save_picks(&service, "Q1", 1, vec![pick(0, "a")])
        .await
        .unwrap();
    dismiss(&service, "Q1", "stale").await.unwrap();
    let shown = show(&service, "Q1").await.unwrap();
    assert_eq!(shown.question.unwrap().id, "Q1");
    assert_eq!(
        (shown.version, shown.state.as_str(), shown.reason.as_str()),
        (1, "dismissed", "stale")
    );
    assert_eq!(shown.picks, [pick(0, "a")]);
}

#[tokio::test]
async fn show_question_of_an_unknown_id_is_not_found() {
    let folder = TempFolder::new();
    let service = service(open_store(&folder).await);
    let missing = show(&service, "nope").await.unwrap_err();
    assert_eq!(missing.code(), Code::NotFound);
    assert_eq!(missing.message(), "unknown_question");
}

#[tokio::test]
async fn a_relegate_is_stored_logged_with_its_kind_and_does_not_come_back() {
    let (logs, _guard) = capture_logs();
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    let service = service(Arc::clone(&store));
    for id in ["Q1", "Q2", "Q3"] {
        put(&service, question(id, "spec-CANARY-77", "abc"))
            .await
            .unwrap();
    }
    answer(
        &service,
        "Q1",
        1,
        pb::AnswerKind::Relegate,
        vec![pick(0, "a")],
        "CANARY-NOTE park it",
    )
    .await
    .unwrap();
    answer(&service, "Q2", 1, pb::AnswerKind::Relegate, vec![], "")
        .await
        .unwrap();
    assert_eq!(listed(&list(&service).await), vec![("Q3".into(), 1)]);
    assert_eq!(show(&service, "Q1").await.unwrap().state, "answered");

    let lines = logs.with("question answered");
    assert_eq!(lines.len(), 2, "{lines:#?}");
    for line in &lines {
        assert!(line.contains("kind=\"relegate\""), "{line}");
        assert!(!line.contains("CANARY"), "{line}");
    }

    // The line goes, and the relegated question stays out of the todos.
    let batch = store
        .start_batch(
            &[("Q1".to_string(), 1), ("Q2".to_string(), 1)],
            "ANSWERS 2",
            10,
        )
        .await
        .unwrap()
        .expect("batch started");
    store.batch_typed(batch.id, 11).await.unwrap();
    assert_eq!(listed(&list(&service).await), vec![("Q3".into(), 1)]);
    assert_eq!(show(&service, "Q1").await.unwrap().state, "sent");
    assert!(logs.with("set-aside questions returned").is_empty());
}

#[tokio::test]
async fn question_answered_names_every_kind() {
    let (logs, _guard) = capture_logs();
    let folder = TempFolder::new();
    let service = service(open_store(&folder).await);
    let kinds = [
        (pb::AnswerKind::Answer, "answer", vec![pick(0, "a")], ""),
        (
            pb::AnswerKind::FreeText,
            "free_text",
            vec![free_pick(0, "z", "typed")],
            "",
        ),
        (pb::AnswerKind::Revise, "revise", vec![], "fix"),
        (pb::AnswerKind::Reject, "reject", vec![], "no"),
        (pb::AnswerKind::Relegate, "relegate", vec![], ""),
    ];
    for (n, (kind, _, picks, note)) in kinds.iter().enumerate() {
        let id = format!("Q{n}");
        put(&service, question(&id, "spec", "abc")).await.unwrap();
        answer(&service, &id, 1, *kind, picks.clone(), note)
            .await
            .unwrap();
    }
    put(&service, instructions_proto("I1", &["a"]))
        .await
        .unwrap();
    put(&service, instructions_proto("I2", &["a"]))
        .await
        .unwrap();
    answer(&service, "I1", 1, pb::AnswerKind::Done, vec![], "")
        .await
        .unwrap();
    answer(&service, "I2", 1, pb::AnswerKind::Ignore, vec![], "")
        .await
        .unwrap();
    let lines = logs.with("question answered");
    let found: Vec<bool> = [
        "answer",
        "free_text",
        "revise",
        "reject",
        "relegate",
        "done",
        "ignore",
    ]
    .iter()
    .zip(&lines)
    .map(|(word, line)| line.contains(&format!("kind=\"{word}\"")))
    .collect();
    assert_eq!(lines.len(), 7, "{lines:#?}");
    assert!(found.iter().all(|ok| *ok), "{lines:#?}");
}

#[tokio::test]
async fn instructions_refuse_a_relegate() {
    let folder = TempFolder::new();
    let service = service(open_store(&folder).await);
    put(&service, instructions_proto("I1", &["a"]))
        .await
        .unwrap();
    let refused = answer(&service, "I1", 1, pb::AnswerKind::Relegate, vec![], "")
        .await
        .unwrap_err();
    assert_eq!(refused.code(), Code::InvalidArgument);
    assert_eq!(refused.message(), "invalid kind");
    assert_eq!(listed(&list(&service).await), vec![("I1".into(), 1)]);
}

#[tokio::test]
async fn an_answers_trace_parent_is_kept_only_when_it_is_a_well_formed_traceparent() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    let service = service(Arc::clone(&store));
    for id in ["Q1", "Q2"] {
        put(&service, question(id, "spec", "abc123")).await.unwrap();
    }
    let answer_with = |id: &str, trace_parent: &str| {
        service.answer_question(Request::new(pb::AnswerQuestionRequest {
            id: id.into(),
            version: 1,
            kind: pb::AnswerKind::Answer as i32,
            picks: vec![pick(0, "a")],
            note: String::new(),
            send_now: true,
            trace_parent: trace_parent.into(),
        }))
    };
    let phone = "00-0af7651916cd43dd8448eb211c80319c-00f067aa0ba902b7-01";

    answer_with("Q1", phone).await.unwrap();
    answer_with("Q2", "not a traceparent").await.unwrap();

    let crate::tools::questions::store::Readiness::Ready(ready) =
        store.ready(i64::MAX, 0).await.unwrap()
    else {
        panic!("expected Ready");
    };
    assert_eq!(ready.traces, [phone]);
}

// ---------------------------------------------------------------------------
// Spec files, the changed spec, and the checked time
// ---------------------------------------------------------------------------

/// A registered product with a specs folder, and a service over it.
struct Specs {
    repo: std::path::PathBuf,
    _repo_folder: TempFolder,
    _db_folder: TempFolder,
    service: QuestionToolService<crate::tools::questions::store::TursoQuestionStore>,
}

impl Specs {
    async fn new() -> Self {
        let repo_folder = TempFolder::new();
        let specs = repo_folder.0.join("repo").join(".superpowers/pod/specs");
        std::fs::create_dir_all(&specs).unwrap();
        let repo = std::fs::canonicalize(repo_folder.0.join("repo")).unwrap();
        let db_folder = TempFolder::new();
        let products = Arc::new(FakeProducts(Ok(vec![("harold".into(), repo.clone())])));
        let service = QuestionToolService::new(
            open_store(&db_folder).await,
            Arc::new(Notify::new()),
            products,
        );
        Self {
            repo,
            _repo_folder: repo_folder,
            _db_folder: db_folder,
            service,
        }
    }

    fn file(&self, name: &str) -> String {
        self.repo
            .join(".superpowers/pod/specs")
            .join(name)
            .to_str()
            .unwrap()
            .to_owned()
    }

    fn write(&self, name: &str, bytes: &[u8]) -> String {
        let path = self.file(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }
}

fn asked_with(id: &str, bytes: &[u8], spec_file: &str) -> pb::Question {
    pb::Question {
        spec_file: spec_file.into(),
        ..question(id, "knowledge-index", &sha12(bytes))
    }
}

fn state_of(response: &pb::ListQuestionsResponse, id: &str) -> pb::SpecState {
    let listed = response
        .questions
        .iter()
        .find(|q| q.question.as_ref().unwrap().id == id)
        .unwrap();
    pb::SpecState::try_from(listed.spec_state).unwrap()
}

#[tokio::test]
async fn a_spec_file_outside_every_specs_folder_is_refused_and_nothing_is_stored() {
    let specs = Specs::new().await;
    let elsewhere = TempFolder::new();
    std::fs::create_dir_all(&elsewhere.0).unwrap();
    let outside = elsewhere.0.join("a.md");
    std::fs::write(&outside, b"x").unwrap();
    let refused = put(
        &specs.service,
        asked_with("Q1", b"x", outside.to_str().unwrap()),
    )
    .await
    .unwrap_err();
    assert_eq!(refused.code(), Code::InvalidArgument);
    assert_eq!(refused.message(), "invalid spec_file");
    assert!(list(&specs.service).await.questions.is_empty());
}

#[tokio::test]
async fn a_spec_file_that_breaks_a_plain_rule_is_refused_with_the_fields_name() {
    let specs = Specs::new().await;
    let inside = specs.write("a.md", b"x");
    let mut cases = vec![
        ("relative", asked_with("Q1", b"x", "specs/a.md")),
        (
            "not markdown",
            asked_with("Q1", b"x", &inside.replace(".md", ".txt")),
        ),
        (
            "too long",
            asked_with("Q1", b"x", &format!("/{}.md", "a".repeat(1000))),
        ),
    ];
    let mut no_sha = asked_with("Q1", b"x", &inside);
    no_sha.sha = String::new();
    cases.push(("no sha", no_sha));
    for (what, question) in cases {
        let refused = put(&specs.service, question).await.unwrap_err();
        assert_eq!(refused.code(), Code::InvalidArgument, "{what}");
        assert_eq!(refused.message(), "invalid spec_file", "{what}");
    }
    assert!(list(&specs.service).await.questions.is_empty());
}

#[tokio::test]
async fn a_symlink_inside_the_specs_folder_leading_out_is_refused_at_put() {
    let specs = Specs::new().await;
    let elsewhere = TempFolder::new();
    std::fs::create_dir_all(&elsewhere.0).unwrap();
    let outside = elsewhere.0.join("a.md");
    std::fs::write(&outside, b"x").unwrap();
    let link = specs.file("link.md");
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    let refused = put(&specs.service, asked_with("Q1", b"x", &link))
        .await
        .unwrap_err();
    assert_eq!(refused.message(), "invalid spec_file");
}

#[tokio::test]
async fn the_spec_state_follows_the_file_and_the_path_is_never_returned() {
    let specs = Specs::new().await;
    let path = specs.write("a.md", b"first");
    put(&specs.service, asked_with("Q1", b"first", &path))
        .await
        .unwrap();
    put(&specs.service, question("Q2", "other", "abc"))
        .await
        .unwrap();

    let response = list(&specs.service).await;
    assert_eq!(state_of(&response, "Q1"), pb::SpecState::Current);
    assert_eq!(state_of(&response, "Q2"), pb::SpecState::Unknown);
    assert!(
        response
            .questions
            .iter()
            .all(|q| q.question.as_ref().unwrap().spec_file.is_empty())
    );

    specs.write("a.md", b"first, then revised");
    let response = list(&specs.service).await;
    assert_eq!(state_of(&response, "Q1"), pb::SpecState::Changed);

    let shown = specs
        .service
        .show_question(Request::new(pb::ShowQuestionRequest { id: "Q1".into() }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(shown.spec_state, pb::SpecState::Changed as i32);
    assert_eq!(shown.question.unwrap().spec_file, "");

    specs.write("a.md", b"first");
    assert_eq!(
        state_of(&list(&specs.service).await, "Q1"),
        pb::SpecState::Current,
        "changed back"
    );
}

#[tokio::test]
async fn a_changed_spec_refuses_an_answer_and_a_pick_until_the_chief_puts_it_again() {
    let specs = Specs::new().await;
    let path = specs.write("a.md", b"first");
    put(&specs.service, asked_with("Q1", b"first", &path))
        .await
        .unwrap();
    specs.write("a.md", b"second version");

    for kind in [
        pb::AnswerKind::Answer,
        pb::AnswerKind::Revise,
        pb::AnswerKind::Reject,
        pb::AnswerKind::Relegate,
    ] {
        let note = if kind == pb::AnswerKind::Answer {
            ""
        } else {
            "why"
        };
        let picks = if kind == pb::AnswerKind::Reject {
            vec![]
        } else {
            vec![pick(0, "a")]
        };
        let refused = answer(&specs.service, "Q1", 1, kind, picks, note)
            .await
            .unwrap_err();
        assert_eq!(refused.code(), Code::FailedPrecondition, "{kind:?}");
        assert_eq!(refused.message(), "spec_changed", "{kind:?}");
    }
    let refused = specs
        .service
        .save_picks(Request::new(pb::SavePicksRequest {
            id: "Q1".into(),
            version: 1,
            picks: vec![pick(0, "a")],
        }))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), Code::FailedPrecondition);
    assert_eq!(refused.message(), "spec_changed");
    let still = list(&specs.service).await;
    assert_eq!(listed(&still), vec![("Q1".into(), 1)], "still waiting");

    // Put again with the new checksum: the notice goes, and the answer is taken.
    put(&specs.service, asked_with("Q1", b"second version", &path))
        .await
        .unwrap();
    assert_eq!(
        state_of(&list(&specs.service).await, "Q1"),
        pb::SpecState::Current
    );
    answer(
        &specs.service,
        "Q1",
        2,
        pb::AnswerKind::Answer,
        vec![pick(0, "a")],
        "",
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn an_answer_at_an_old_version_is_version_changed_not_spec_changed() {
    let specs = Specs::new().await;
    let path = specs.write("a.md", b"first");
    put(&specs.service, asked_with("Q1", b"first", &path))
        .await
        .unwrap();
    specs.write("a.md", b"second version");
    put(&specs.service, asked_with("Q1", b"second version", &path))
        .await
        .unwrap();
    let refused = answer(
        &specs.service,
        "Q1",
        1,
        pb::AnswerKind::Answer,
        vec![pick(0, "a")],
        "",
    )
    .await
    .unwrap_err();
    assert_eq!(refused.message(), "version_changed");
}

#[tokio::test]
async fn instructions_with_a_spec_file_follow_the_same_rule() {
    let specs = Specs::new().await;
    let path = specs.write("a.md", b"first");
    let mut steps = instructions_proto("I1", &["do it"]);
    steps.sha = sha12(b"first");
    steps.spec_file = path;
    put(&specs.service, steps).await.unwrap();
    specs.write("a.md", b"second version");
    for kind in [pb::AnswerKind::Done, pb::AnswerKind::Ignore] {
        let refused = answer(&specs.service, "I1", 1, kind, vec![], "")
            .await
            .unwrap_err();
        assert_eq!(refused.message(), "spec_changed", "{kind:?}");
    }
}

#[tokio::test]
async fn a_registry_that_cannot_be_read_stores_the_todo_and_keeps_it_unknown() {
    let (logs, _guard) = capture_logs();
    let db = TempFolder::new();
    let service = QuestionToolService::new(
        open_store(&db).await,
        Arc::new(Notify::new()),
        Arc::new(FakeProducts(Err("registry_unreadable"))),
    );
    put(
        &service,
        asked_with("Q1", b"first", "/anywhere/.superpowers/pod/specs/a.md"),
    )
    .await
    .unwrap();
    let response = list(&service).await;
    assert_eq!(state_of(&response, "Q1"), pb::SpecState::Unknown);
    answer(
        &service,
        "Q1",
        1,
        pb::AnswerKind::Answer,
        vec![pick(0, "a")],
        "",
    )
    .await
    .expect("an unknown spec does not stop an answer");
    let lines = logs.with("todo spec unknown");
    assert_eq!(lines.len(), 1, "{:#?}", logs.lines());
    assert!(
        lines[0].contains("reason=\"registry_unreadable\""),
        "{}",
        lines[0]
    );
}

#[tokio::test]
async fn a_changed_or_unknown_spec_is_logged_once_per_version_without_the_path() {
    let (logs, _guard) = capture_logs();
    let specs = Specs::new().await;
    let canary = specs.write("canary-5521.md", b"first");
    put(&specs.service, asked_with("Q1", b"first", &canary))
        .await
        .unwrap();
    let gone = specs.file("gone-5521.md");
    put(&specs.service, asked_with("Q2", b"x", &gone))
        .await
        .unwrap();

    list(&specs.service).await;
    specs.write("canary-5521.md", b"second version");
    for _ in 0..3 {
        list(&specs.service).await;
    }
    let changed = logs.with("todo changed spec");
    assert_eq!(changed.len(), 1, "{:#?}", logs.lines());
    assert_eq!(
        field_names(&changed[0], "todo changed spec"),
        ["tool", "question_id", "version"]
    );
    let unknown = logs.with("todo spec unknown");
    assert_eq!(unknown.len(), 1, "{:#?}", logs.lines());
    assert_eq!(
        field_names(&unknown[0], "todo spec unknown"),
        ["tool", "question_id", "version", "reason"]
    );
    assert!(unknown[0].contains("reason=\"missing\""), "{}", unknown[0]);

    // A new version logs again.
    put(&specs.service, asked_with("Q1", b"first", &canary))
        .await
        .unwrap();
    list(&specs.service).await;
    assert_eq!(logs.with("todo changed spec").len(), 2);

    // Nothing the tool logs carries the path.
    let _ = answer(
        &specs.service,
        "Q1",
        2,
        pb::AnswerKind::Answer,
        vec![pick(0, "a")],
        "",
    )
    .await;
    assert!(
        logs.lines().iter().all(|line| !line.contains("5521")),
        "{:#?}",
        logs.lines()
    );
}

#[tokio::test]
async fn mark_checked_stores_the_time_lists_it_and_logs_the_count() {
    let (logs, _guard) = capture_logs();
    let folder = TempFolder::new();
    let service = service(open_store(&folder).await);
    assert_eq!(list(&service).await.checked_at_ms, 0);
    put(&service, question("Q1", "spec", "abc")).await.unwrap();
    put(&service, instructions_proto("I1", &["a"]))
        .await
        .unwrap();
    put(&service, question("Q2", "spec", "abc")).await.unwrap();

    let before = now_ms();
    let checked = service
        .mark_checked(Request::new(pb::MarkCheckedRequest {}))
        .await
        .unwrap()
        .into_inner()
        .checked_at_ms;
    assert!(checked >= before && checked <= now_ms());
    assert_eq!(list(&service).await.checked_at_ms, checked);

    let lines = logs.with("todos checked");
    assert_eq!(lines.len(), 1, "{:#?}", logs.lines());
    assert_eq!(field_names(&lines[0], "todos checked"), ["tool", "count"]);
    assert!(
        lines[0].contains("count=3"),
        "every listed todo waits: {}",
        lines[0]
    );
}

#[tokio::test]
async fn mark_checked_on_a_failing_store_is_unavailable() {
    let folder = TempFolder::new();
    let store = FailingStore::new(open_store(&folder).await);
    let service = QuestionToolService::new(store, Arc::new(Notify::new()), no_products());
    let refused = service
        .mark_checked(Request::new(pb::MarkCheckedRequest {}))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), Code::Unavailable);
}

fn page_of(response: &pb::ListQuestionsResponse, id: &str) -> String {
    response
        .questions
        .iter()
        .find(|q| q.question.as_ref().unwrap().id == id)
        .unwrap()
        .spec_page
        .clone()
}

#[tokio::test]
async fn a_todo_whose_spec_file_is_in_a_specs_folder_gets_its_spec_page() {
    let specs = Specs::new().await;
    let path = specs.write("serve-specs.md", b"x");
    let hidden = specs.write(".revisions.md", b"x");
    let outside_folder = TempFolder::new();
    std::fs::create_dir_all(&outside_folder.0).unwrap();
    let outside = outside_folder.0.join("elsewhere.md");
    std::fs::write(&outside, b"x").unwrap();
    let alias = specs.file("alias.md");
    std::os::unix::fs::symlink(&path, &alias).unwrap();
    let leading_out = specs.file("link.md");
    std::os::unix::fs::symlink(&outside, &leading_out).unwrap();
    put(&specs.service, asked_with("Q1", b"x", &path))
        .await
        .unwrap();
    put(&specs.service, asked_with("Q2", b"x", &alias))
        .await
        .unwrap();
    put(&specs.service, asked_with("Q3", b"x", &hidden))
        .await
        .unwrap();
    put(&specs.service, question("Q4", "other", "abc"))
        .await
        .unwrap();
    // A file that does not exist yet passes at put but has no page.
    put(
        &specs.service,
        asked_with("Q5", b"x", &specs.file("later.md")),
    )
    .await
    .unwrap();

    let response = list(&specs.service).await;

    assert_eq!(page_of(&response, "Q1"), "/specs/harold/serve-specs");
    assert_eq!(page_of(&response, "Q2"), "/specs/harold/alias");
    assert_eq!(page_of(&response, "Q3"), "", "the id has a dot");
    assert_eq!(page_of(&response, "Q4"), "", "no spec file");
    assert_eq!(page_of(&response, "Q5"), "", "no such file");
    // The path itself is still never returned.
    assert!(
        response
            .questions
            .iter()
            .all(|q| q.question.as_ref().unwrap().spec_file.is_empty())
    );
}

struct WithListener(&'static str);

impl crate::tools::Products for WithListener {
    fn products(&self) -> Result<Vec<(String, std::path::PathBuf)>, &'static str> {
        Ok(Vec::new())
    }

    fn local_specs_url(&self) -> Option<String> {
        (!self.0.is_empty()).then(|| self.0.to_owned())
    }
}

#[tokio::test]
async fn the_listing_names_the_specs_listener_only_when_it_is_open() {
    let folder = TempFolder::new();
    for (url, expected) in [("http://127.0.0.1:4331", "http://127.0.0.1:4331"), ("", "")] {
        let service = QuestionToolService::new(
            open_store(&folder).await,
            Arc::new(Notify::new()),
            Arc::new(WithListener(url)),
        );
        assert_eq!(list(&service).await.local_specs_url, expected);
    }
}

#[tokio::test]
async fn a_question_with_a_spec_file_may_leave_out_its_link_and_one_without_may_not() {
    let specs = Specs::new().await;
    let path = specs.write("a.md", b"x");
    let mut no_link = asked_with("Q1", b"x", &path);
    no_link.link = String::new();
    put(&specs.service, no_link).await.unwrap();
    let stored = list(&specs.service).await;
    assert_eq!(stored.questions.len(), 1);
    assert_eq!(stored.questions[0].question.as_ref().unwrap().link, "");

    // A link it does give must still be a web address.
    let mut bad_link = asked_with("Q2", b"x", &path);
    bad_link.link = "ftp://example.com/x".into();
    let refused = put(&specs.service, bad_link).await.unwrap_err();
    assert_eq!(refused.message(), "invalid link");

    // No spec file: the link is still required.
    let mut neither = question("Q3", "other", "abc");
    neither.link = String::new();
    let refused = put(&specs.service, neither).await.unwrap_err();
    assert_eq!(refused.code(), Code::InvalidArgument);
    assert_eq!(refused.message(), "invalid link");
}
