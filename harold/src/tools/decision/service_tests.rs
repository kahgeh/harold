use tonic::Request;

use super::*;
use crate::outbound::fake::CapturedLogs;
use crate::telemetry::test_support as telemetry_support;
use crate::tools::decision::test_support::{CHIEF, FakeChief, FakeDetails, Temp};

/// 2026-11-14 03:10 UTC.
const NOW: i64 = 1_794_625_800_000;
const DAY_MS: i64 = 86_400_000;

/// A change that a test makes to a valid add request.
type Change = Box<dyn Fn(&mut pb::AddRequest)>;

struct Rig {
    svc: DecisionToolService,
    details: Arc<FakeDetails>,
    now: Arc<std::sync::atomic::AtomicI64>,
    _dir: Temp,
}

async fn rig() -> Rig {
    rig_with(DecisionSettings::default()).await
}

async fn rig_with(settings: DecisionSettings) -> Rig {
    let dir = Temp::new();
    let details = Arc::new(FakeDetails::default());
    let now = Arc::new(std::sync::atomic::AtomicI64::new(NOW));
    let clock_now = Arc::clone(&now);
    let svc = DecisionToolService::new(
        dir.store().await,
        Arc::new(FakeChief),
        details.clone(),
        Arc::new(move || clock_now.load(std::sync::atomic::Ordering::Relaxed)),
        settings,
    );
    Rig {
        svc,
        details,
        now,
        _dir: dir,
    }
}

fn add_request(id: &str) -> pb::AddRequest {
    pb::AddRequest {
        pane_id: CHIEF.into(),
        role: "pnem".into(),
        kind: "slicing".into(),
        details_owner: "pnem:harold".into(),
        accountable: "pnem".into(),
        title: "Fold the naming fix into agent-restart".into(),
        link: "spec:harold/pod-role-record".into(),
        check_on: "2026-12-01".into(),
        details: format!("pod:harold/specs/pod-role-record.decisions.md#{id}"),
        asked_at: "2026-11-14T03:00Z".into(),
    }
}

async fn added(rig: &Rig, id: &str) -> pb::Decision {
    rig.svc
        .add(Request::new(add_request(id)))
        .await
        .unwrap()
        .into_inner()
        .decision
        .unwrap()
}

async fn refusal<T: std::fmt::Debug>(
    result: Result<tonic::Response<T>, Status>,
) -> (tonic::Code, String) {
    let status = result.expect_err("refused");
    (status.code(), status.message().to_owned())
}

fn denied(reason: &str) -> (tonic::Code, String) {
    (tonic::Code::PermissionDenied, reason.to_owned())
}

fn invalid(reason: &str) -> (tonic::Code, String) {
    (tonic::Code::InvalidArgument, reason.to_owned())
}

#[tokio::test]
async fn add_keeps_the_anchor_id_and_every_field() {
    let rig = rig().await;
    let row = added(&rig, "d-20261114-02").await;
    assert_eq!(row.id, "d-20261114-02");
    assert_eq!(
        (
            row.role.as_str(),
            row.kind.as_str(),
            row.details_owner.as_str()
        ),
        ("pnem", "slicing", "pnem:harold")
    );
    assert_eq!(row.accountable, "pnem");
    assert_eq!(row.organisation, "personal-chief");
    assert_eq!(row.status, "open");
    assert_eq!(row.check_on, "2026-12-01");
    assert_eq!(row.asked_at_ms, NOW - 10 * 60_000);
    let asked = rig.details.asked.lock().unwrap().clone();
    assert_eq!(asked[0].1, "pnem:harold", "the details check got the owner");
}

#[tokio::test]
async fn the_anchor_id_must_be_well_formed_unused_and_not_after_the_add_date() {
    let rig = rig().await;
    for bad in [
        "d-2026114-02",
        "d-20261114-2",
        "x-20261114-02",
        "d-20261314-02",
        "d-20261114-aa",
    ] {
        let result = rig.svc.add(Request::new(add_request(bad))).await;
        assert_eq!(refusal(result).await, invalid("details_missing"), "{bad}");
    }
    let no_anchor = pb::AddRequest {
        details: "pod:harold/specs/x.md".into(),
        ..add_request("d-20261114-02")
    };
    assert_eq!(
        refusal(rig.svc.add(Request::new(no_anchor)).await).await,
        invalid("details_missing")
    );
    added(&rig, "d-20261114-02").await;
    let used = rig
        .svc
        .add(Request::new(add_request("d-20261114-02")))
        .await;
    assert_eq!(refusal(used).await, invalid("details_missing"));
    let future = rig
        .svc
        .add(Request::new(add_request("d-20261115-01")))
        .await;
    assert_eq!(refusal(future).await, invalid("details_missing"));
    // An earlier date is allowed: the owner wrote the entry a day before.
    let earlier = added(&rig, "d-20261113-04").await;
    assert_eq!(earlier.decided_on, "2026-11-14");
}

#[tokio::test]
async fn a_write_from_another_pane_is_refused_with_not_chief_for_each_write() {
    let rig = rig().await;
    let row = added(&rig, "d-20261114-01").await;
    let from_other = |pane: &str| pane.to_owned();
    let other = from_other("%9");
    let mut request = add_request("d-20261114-02");
    request.pane_id = other.clone();
    assert_eq!(
        refusal(rig.svc.add(Request::new(request)).await).await,
        denied("not_chief")
    );
    let evaluate = pb::EvaluateRequest {
        pane_id: other.clone(),
        id: row.id.clone(),
        process_grade: "good".into(),
        outcome_grade: "good".into(),
    };
    assert_eq!(
        refusal(rig.svc.evaluate(Request::new(evaluate)).await).await,
        denied("not_chief")
    );
    let recheck = pb::RecheckRequest {
        pane_id: other.clone(),
        id: row.id.clone(),
        check_on: "2027-01-01".into(),
    };
    assert_eq!(
        refusal(rig.svc.recheck(Request::new(recheck)).await).await,
        denied("not_chief")
    );
    let link = pb::LinkRequest {
        pane_id: other,
        id: row.id.clone(),
        link: "work:x".into(),
    };
    assert_eq!(
        refusal(rig.svc.link(Request::new(link)).await).await,
        denied("not_chief")
    );
    let nopane = pb::LinkRequest {
        pane_id: String::new(),
        id: row.id,
        link: "work:x".into(),
    };
    assert_eq!(
        refusal(rig.svc.link(Request::new(nopane)).await).await,
        denied("no_pane")
    );
}

#[tokio::test]
async fn reads_need_no_pane() {
    let rig = rig().await;
    let row = added(&rig, "d-20261114-01").await;
    let listed = rig
        .svc
        .list(Request::new(pb::ListRequest::default()))
        .await
        .unwrap();
    assert_eq!(listed.into_inner().decisions.len(), 1);
    let shown = rig
        .svc
        .show(Request::new(pb::ShowRequest { id: row.id }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(shown.details_path, "/fixture/pod/entry.md");
    let missing = rig
        .svc
        .show(Request::new(pb::ShowRequest {
            id: "d-20990101-01".into(),
        }))
        .await;
    assert_eq!(
        refusal(missing).await,
        (tonic::Code::NotFound, "unknown_decision".into())
    );
}

#[tokio::test]
async fn each_bad_field_refuses_with_its_reason_and_writes_nothing() {
    let rig = rig().await;
    let cases: Vec<(&str, Change)> = vec![
        ("unknown_kind", Box::new(|r| r.kind = "weather".into())),
        (
            "bad_details_owner",
            Box::new(|r| r.details_owner = "pnem".into()),
        ),
        (
            "bad_accountable",
            Box::new(|r| r.accountable = "nobody".into()),
        ),
        ("bad_role", Box::new(|r| r.role = "king".into())),
        ("bad_title", Box::new(|r| r.title = "x".repeat(121))),
        ("bad_title", Box::new(|r| r.title = "  ".into())),
        ("bad_link", Box::new(|r| r.link = "elsewhere:1".into())),
        ("bad_date", Box::new(|r| r.check_on = "2026-02-30".into())),
        ("bad_date", Box::new(|r| r.check_on = "01/12/2026".into())),
        ("bad_date", Box::new(|r| r.asked_at = "yesterday".into())),
    ];
    for (reason, change) in cases {
        let mut request = add_request("d-20261114-02");
        change(&mut request);
        assert_eq!(
            refusal(rig.svc.add(Request::new(request)).await).await,
            invalid(reason),
            "{reason}"
        );
    }
    let listed = rig
        .svc
        .list(Request::new(pb::ListRequest {
            all: true,
            ..Default::default()
        }))
        .await
        .unwrap();
    assert!(listed.into_inner().decisions.is_empty());
}

#[tokio::test]
async fn a_details_refusal_passes_its_reason_on() {
    let rig = rig().await;
    *rig.details.refuse_with.lock().unwrap() = Some("details_owner_mismatch");
    let result = rig
        .svc
        .add(Request::new(add_request("d-20261114-02")))
        .await;
    assert_eq!(refusal(result).await, invalid("details_owner_mismatch"));
}

#[tokio::test]
async fn a_kind_added_to_the_settings_is_accepted() {
    let settings = DecisionSettings {
        kinds: vec!["hiring".into()],
        ..DecisionSettings::default()
    };
    let rig = rig_with(settings).await;
    let mut request = add_request("d-20261114-02");
    request.kind = "hiring".into();
    assert!(rig.svc.add(Request::new(request)).await.is_ok());
    let mut old = add_request("d-20261114-03");
    old.kind = "slicing".into();
    assert_eq!(
        refusal(rig.svc.add(Request::new(old)).await).await,
        invalid("unknown_kind")
    );
}

#[tokio::test]
async fn evaluate_sets_the_grades_and_a_second_evaluate_is_refused() {
    let rig = rig().await;
    let row = added(&rig, "d-20261114-01").await;
    rig.now
        .store(NOW + 3 * DAY_MS, std::sync::atomic::Ordering::Relaxed);
    let request = |process: &str| pb::EvaluateRequest {
        pane_id: CHIEF.into(),
        id: row.id.clone(),
        process_grade: process.into(),
        outcome_grade: "poor".into(),
    };
    let done = rig
        .svc
        .evaluate(Request::new(request("good")))
        .await
        .unwrap()
        .into_inner()
        .decision
        .unwrap();
    assert_eq!(
        (
            done.status.as_str(),
            done.process_grade.as_str(),
            done.outcome_grade.as_str()
        ),
        ("evaluated", "good", "poor")
    );
    let again = rig.svc.evaluate(Request::new(request("good"))).await;
    assert_eq!(
        refusal(again).await,
        (tonic::Code::FailedPrecondition, "already_evaluated".into())
    );
    let fresh = added(&rig, "d-20261114-05").await;
    let bad = pb::EvaluateRequest {
        pane_id: CHIEF.into(),
        id: fresh.id,
        process_grade: "fine".into(),
        outcome_grade: "good".into(),
    };
    assert_eq!(
        refusal(rig.svc.evaluate(Request::new(bad)).await).await,
        invalid("bad_grade")
    );
}

#[tokio::test]
async fn recheck_validates_the_date_and_link_sets_the_link() {
    let rig = rig().await;
    let row = added(&rig, "d-20261114-01").await;
    let bad = pb::RecheckRequest {
        pane_id: CHIEF.into(),
        id: row.id.clone(),
        check_on: "soon".into(),
    };
    assert_eq!(
        refusal(rig.svc.recheck(Request::new(bad)).await).await,
        invalid("bad_date")
    );
    let moved = rig
        .svc
        .recheck(Request::new(pb::RecheckRequest {
            pane_id: CHIEF.into(),
            id: row.id.clone(),
            check_on: "2027-01-05".into(),
        }))
        .await
        .unwrap()
        .into_inner()
        .decision
        .unwrap();
    assert_eq!(moved.check_on, "2027-01-05");
    let linked = rig
        .svc
        .link(Request::new(pb::LinkRequest {
            pane_id: CHIEF.into(),
            id: row.id.clone(),
            link: "work:rotated-away".into(),
        }))
        .await
        .unwrap()
        .into_inner()
        .decision
        .unwrap();
    assert_eq!(linked.link, "work:rotated-away");
    let bad_link = pb::LinkRequest {
        pane_id: CHIEF.into(),
        id: row.id,
        link: "nope".into(),
    };
    assert_eq!(
        refusal(rig.svc.link(Request::new(bad_link)).await).await,
        invalid("bad_link")
    );
}

#[tokio::test]
async fn list_defaults_and_filters() {
    let rig = rig().await;
    let a = added(&rig, "d-20261114-01").await;
    let mut other = add_request("d-20261114-02");
    other.role = "chief".into();
    other.details_owner = "chief".into();
    other.details = "vault:decisions.md#d-20261114-02".into();
    other.accountable = "user".into();
    other.kind = "model".into();
    other.check_on = "2026-11-02".into();
    let b = rig
        .svc
        .add(Request::new(other))
        .await
        .unwrap()
        .into_inner()
        .decision
        .unwrap();
    rig.svc
        .evaluate(Request::new(pb::EvaluateRequest {
            pane_id: CHIEF.into(),
            id: a.id.clone(),
            process_grade: "good".into(),
            outcome_grade: "good".into(),
        }))
        .await
        .unwrap();
    let ids = |response: tonic::Response<pb::ListResponse>| {
        response
            .into_inner()
            .decisions
            .into_iter()
            .map(|d| d.id)
            .collect::<Vec<_>>()
    };
    let list = |request: pb::ListRequest| rig.svc.list(Request::new(request));
    assert_eq!(
        ids(list(pb::ListRequest::default()).await.unwrap()),
        std::slice::from_ref(&b.id),
        "default is open and due"
    );
    assert_eq!(
        ids(list(pb::ListRequest {
            all: true,
            ..Default::default()
        })
        .await
        .unwrap()),
        [b.id.clone(), a.id.clone()]
    );
    assert_eq!(
        ids(list(pb::ListRequest {
            all: true,
            role: "chief".into(),
            ..Default::default()
        })
        .await
        .unwrap()),
        std::slice::from_ref(&b.id)
    );
    assert_eq!(
        ids(list(pb::ListRequest {
            all: true,
            kind: "slicing".into(),
            ..Default::default()
        })
        .await
        .unwrap()),
        std::slice::from_ref(&a.id)
    );
    assert_eq!(
        ids(list(pb::ListRequest {
            all: true,
            details_owner: "chief".into(),
            ..Default::default()
        })
        .await
        .unwrap()),
        std::slice::from_ref(&b.id)
    );
    assert_eq!(
        ids(list(pb::ListRequest {
            all: true,
            accountable: "pnem".into(),
            ..Default::default()
        })
        .await
        .unwrap()),
        std::slice::from_ref(&a.id)
    );
    assert!(
        ids(list(pb::ListRequest {
            due: true,
            ..Default::default()
        })
        .await
        .unwrap())
        .is_empty()
    );
    assert_eq!(
        ids(list(pb::ListRequest {
            open: true,
            ..Default::default()
        })
        .await
        .unwrap()),
        [b.id]
    );
}

#[tokio::test]
async fn add_and_evaluate_count_with_the_right_attributes_and_one_feature_use() {
    telemetry_support::install();
    let rig = rig_with(DecisionSettings {
        organisation: "org-svc".into(),
        ..DecisionSettings::default()
    })
    .await;
    let series = |name: &str, extra: &[(&str, &str)]| {
        let mut attrs = vec![
            ("organisation", "org-svc"),
            ("role", "pnem"),
            ("kind", "slicing"),
            ("accountable", "pnem"),
        ];
        attrs.extend_from_slice(extra);
        telemetry_support::exact_counter_value(name, &attrs).unwrap_or(0)
    };
    let before_added = series("harold.decisions.added", &[]);
    let before_eval = series(
        "harold.decisions.evaluated",
        &[("process_grade", "good"), ("outcome_grade", "poor")],
    );
    // The shared counter moves with every test that evaluates, so the feature use is read
    // from this test's own notes.
    let uses = || {
        crate::telemetry::take_noted_work()
            .into_iter()
            .filter(|noted| {
                *noted == crate::telemetry::WorkNoted::FeatureUsed("decision-tool".into())
            })
            .count()
    };
    uses();
    let row = added(&rig, "d-20261114-01").await;
    assert_eq!(uses(), 0, "an add is not a use of the feature");
    rig.svc
        .evaluate(Request::new(pb::EvaluateRequest {
            pane_id: CHIEF.into(),
            id: row.id.clone(),
            process_grade: "good".into(),
            outcome_grade: "poor".into(),
        }))
        .await
        .unwrap();
    let _ = rig
        .svc
        .evaluate(Request::new(pb::EvaluateRequest {
            pane_id: CHIEF.into(),
            id: row.id,
            process_grade: "good".into(),
            outcome_grade: "poor".into(),
        }))
        .await;
    assert_eq!(series("harold.decisions.added", &[]), before_added + 1);
    assert_eq!(
        series(
            "harold.decisions.evaluated",
            &[("process_grade", "good"), ("outcome_grade", "poor")]
        ),
        before_eval + 1
    );
    assert_eq!(uses(), 1, "one use is one accepted evaluate");
}

#[tokio::test]
async fn the_gauge_follows_the_open_count_after_each_write() {
    telemetry_support::install();
    let rig = rig_with(DecisionSettings {
        organisation: "org-gauge".into(),
        ..DecisionSettings::default()
    })
    .await;
    added(&rig, "d-20261114-01").await;
    added(&rig, "d-20261114-02").await;
    let read = |status: &str| {
        telemetry_support::gauge_value(
            "harold.decisions.open",
            &[("organisation", "org-gauge"), ("status", status)],
        )
    };
    assert_eq!((read("open"), read("due")), (Some(2), Some(0)));
}

#[tokio::test]
async fn the_log_lines_carry_no_title_and_the_refusal_carries_its_reason() {
    let logs = CapturedLogs::start();
    let rig = rig().await;
    let row = added(&rig, "d-20261114-02").await;
    let mut other = add_request("d-20261114-03");
    other.pane_id = "%9".into();
    let _ = rig.svc.add(Request::new(other)).await;
    rig.now
        .store(NOW + 2 * DAY_MS, std::sync::atomic::Ordering::Relaxed);
    rig.svc
        .evaluate(Request::new(pb::EvaluateRequest {
            pane_id: CHIEF.into(),
            id: row.id,
            process_grade: "good".into(),
            outcome_grade: "good".into(),
        }))
        .await
        .unwrap();
    let text = logs.text();
    assert!(
        text.contains("decision added") && text.contains("decision.id=d-20261114-02"),
        "{text}"
    );
    assert!(text.contains("decide_s=600"), "{text}");
    assert!(text.contains("place=pod"), "{text}");
    assert!(
        text.contains("decision evaluated") && text.contains("to_evaluate_days=2"),
        "{text}"
    );
    assert!(
        text.contains("decision refused") && text.contains("reason=not_chief"),
        "{text}"
    );
    assert!(
        !text.contains("Fold the naming fix"),
        "no title in a log line: {text}"
    );
}

#[tokio::test]
async fn evaluate_reads_the_details_again_and_a_refusal_changes_nothing() {
    let rig = rig().await;
    let row = added(&rig, "d-20261114-01").await;
    rig.details.asked.lock().unwrap().clear();
    let evaluate = || {
        rig.svc.evaluate(Request::new(pb::EvaluateRequest {
            pane_id: CHIEF.into(),
            id: row.id.clone(),
            process_grade: "good".into(),
            outcome_grade: "good".into(),
        }))
    };
    for reason in [
        "details_missing",
        "details_unreadable",
        "no_vault",
        "details_owner_mismatch",
    ] {
        *rig.details.refuse_with.lock().unwrap() = Some(reason);
        assert_eq!(refusal(evaluate().await).await, invalid(reason), "{reason}");
    }
    let stored = rig
        .svc
        .show(Request::new(pb::ShowRequest { id: row.id.clone() }))
        .await
        .unwrap();
    assert_eq!(stored.into_inner().decision.unwrap().status, "open");
    *rig.details.refuse_with.lock().unwrap() = None;
    rig.details.asked.lock().unwrap().clear();
    assert!(evaluate().await.is_ok());
    let asked = rig.details.asked.lock().unwrap().clone();
    assert_eq!(
        asked,
        [(row.details.clone(), "pnem:harold".to_owned())],
        "the check used the stored details and owner"
    );
    // An evaluated record is refused as evaluated, before any details read.
    *rig.details.refuse_with.lock().unwrap() = Some("details_missing");
    assert_eq!(
        refusal(evaluate().await).await,
        (tonic::Code::FailedPrecondition, "already_evaluated".into())
    );
}
