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
    let service = QuestionToolService::new(open_store(&folder).await, Arc::clone(&wake));
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
    let service = QuestionToolService::new(open_store(&folder).await, Arc::clone(&wake));
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

const RETURNED: &str = "set-aside questions returned";

#[tokio::test]
async fn returning_set_aside_questions_is_logged_once_for_each_trigger() {
    let (logs, _guard) = capture_logs();
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    let service = service(Arc::clone(&store));
    for id in ["Q1", "Q2", "Q3"] {
        put(&service, question(id, "spec-CANARY-77", "abc"))
            .await
            .unwrap();
    }

    // dismiss: Q2 and Q3 set aside, Q1 withdrawn.
    defer(&service, "Q2").await.unwrap();
    defer(&service, "Q3").await.unwrap();
    assert!(logs.with(RETURNED).is_empty());
    dismiss(&service, "Q1", "withdrawn").await.unwrap();
    let lines = logs.with(RETURNED);
    assert_eq!(lines.len(), 1, "{lines:#?}");
    assert!(
        lines[0].contains("count=2") && lines[0].contains("after=\"dismiss\""),
        "{lines:#?}"
    );
    assert_eq!(field_names(&lines[0], RETURNED), ["tool", "count", "after"]);

    // later: Q2 waits alone after Q3 is set aside, then Q2 itself is set aside.
    defer(&service, "Q3").await.unwrap();
    assert_eq!(logs.with(RETURNED).len(), 1, "Q2 still waits");
    defer(&service, "Q2").await.unwrap();
    let lines = logs.with(RETURNED);
    assert_eq!(lines.len(), 2, "{lines:#?}");
    assert!(lines[1].contains("count=2") && lines[1].contains("after=\"later\""));

    // answers_line: Q2 answered and Q3 set aside; the typed line returns Q3.
    // Deferring Q2 returned both, so both wait again.
    answer(&service, "Q2", 1, pb::AnswerKind::Revise, vec![], "fix it")
        .await
        .unwrap();
    defer(&service, "Q3").await.unwrap();
    assert_eq!(logs.with(RETURNED).len(), 2, "an answer is still unsent");
    let batch = store
        .start_batch(&[("Q2".to_string(), 1)], "ANSWERS 1", 10)
        .await
        .unwrap()
        .expect("batch started");
    store.batch_typed(batch.id, 11).await.unwrap();
    let lines = logs.with(RETURNED);
    assert_eq!(lines.len(), 3, "{lines:#?}");
    assert!(lines[2].contains("count=1") && lines[2].contains("after=\"answers_line\""));

    // count 0 writes nothing, after a dismiss and after a typed line, and no
    // line holds an id or any question text.
    dismiss(&service, "Q3", "withdrawn").await.unwrap();
    put(&service, question("Q4", "spec", "abc")).await.unwrap();
    answer(&service, "Q4", 1, pb::AnswerKind::Revise, vec![], "fix it")
        .await
        .unwrap();
    let batch = store
        .start_batch(&[("Q4".to_string(), 1)], "ANSWERS 1", 20)
        .await
        .unwrap()
        .expect("batch started");
    store.batch_typed(batch.id, 21).await.unwrap();
    let lines = logs.with(RETURNED);
    assert_eq!(lines.len(), 3, "{lines:#?}");
    for line in &lines {
        assert_eq!(field_names(line, RETURNED), ["tool", "count", "after"]);
        assert!(
            !line.contains("CANARY")
                && !line.contains("Q1")
                && !line.contains("Q2")
                && !line.contains("Q3")
        );
    }
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
