use std::time::Duration;

use harold_api::tools::watch::v1::StartWatchRequest;

use super::*;
use crate::tools::watch::scheduler::tests::{Harness, harness, pass, settings};
use crate::tools::watch::test_support::answer;

fn service(h: &Harness) -> WatchToolService {
    WatchToolService::new(Arc::clone(&h.scheduler), settings())
}

fn request(h: &Harness, name: &str) -> StartWatchRequest {
    StartWatchRequest {
        name: name.into(),
        folder: h.folder.0.to_string_lossy().into_owned(),
        skill: "pod-watch".into(),
        every_minutes: 10,
        for_minutes: Some(120),
        report_to: "chief".into(),
        run_budget_usd: 0.0,
        every_run: false,
        run_limit_secs: 0,
    }
}

async fn refusal(service: &WatchToolService, request: StartWatchRequest) -> (tonic::Code, String) {
    let status = service
        .start_watch(Request::new(request))
        .await
        .expect_err("refused");
    (status.code(), status.message().to_string())
}

#[tokio::test(start_paused = true)]
async fn start_stop_and_list_answer_with_the_fields_the_proto_names() {
    let h = harness(settings(), vec![answer("REPORT x", 0.02)]).await;
    let service = service(&h);

    let started = service
        .start_watch(Request::new(request(&h, "after-release")))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(started.name, "after-release");
    assert!(started.ends_at_ms.unwrap() > 0);
    pass(Duration::from_secs(10)).await;

    let listed = service
        .list_watches(Request::new(pb::ListWatchesRequest {}))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(listed.watches.len(), 1);
    let watch = &listed.watches[0];
    assert_eq!(watch.name, "after-release");
    assert_eq!(watch.folder, h.folder.0.to_string_lossy());
    assert_eq!(
        (watch.skill.as_str(), watch.report_to.as_str()),
        ("pod-watch", "chief")
    );
    assert_eq!(watch.run_limit_secs, 120);
    assert!(watch.running && !watch.every_run);
    assert_eq!((watch.runs, watch.reports, watch.every_minutes), (1, 1, 10));
    assert_eq!(watch.ends_at_ms, started.ends_at_ms);
    assert!((watch.cost_usd - 0.02).abs() < 1e-9);
    assert_eq!(watch.last_outcome, "report");
    assert!((listed.spent_usd - 0.02).abs() < 1e-9);

    service
        .stop_watch(Request::new(pb::StopWatchRequest {
            name: "after-release".into(),
        }))
        .await
        .unwrap();
    let listed = service
        .list_watches(Request::new(pb::ListWatchesRequest {}))
        .await
        .unwrap()
        .into_inner();
    assert!(!listed.watches[0].running);
    assert_eq!(listed.watches[0].stop_reason, "stopped");
}

#[tokio::test(start_paused = true)]
async fn start_refuses_with_the_codes_and_messages_the_proto_names() {
    let h = harness(settings(), vec![answer("QUIET", 0.01)]).await;
    let service = service(&h);
    let invalid = tonic::Code::InvalidArgument;

    let mut asked = request(&h, "w");
    asked.name = "bad name".into();
    assert_eq!(
        refusal(&service, asked).await,
        (invalid, "invalid name".into())
    );
    let mut asked = request(&h, "w");
    asked.folder = "relative/path".into();
    assert_eq!(
        refusal(&service, asked).await,
        (invalid, "invalid folder".into())
    );
    let mut asked = request(&h, "w");
    asked.skill = "../x".into();
    assert_eq!(
        refusal(&service, asked).await,
        (invalid, "invalid skill".into())
    );
    let mut asked = request(&h, "w");
    asked.every_minutes = 1;
    assert_eq!(
        refusal(&service, asked).await,
        (invalid, "invalid every_minutes".into())
    );
    let mut asked = request(&h, "w");
    asked.for_minutes = Some(1441);
    assert_eq!(
        refusal(&service, asked).await,
        (invalid, "invalid for_minutes".into())
    );
    let mut asked = request(&h, "w");
    asked.report_to = "/Users/x/code/other".into();
    assert_eq!(
        refusal(&service, asked).await,
        (invalid, "invalid report_to".into())
    );
    let mut asked = request(&h, "w");
    asked.run_budget_usd = 0.005;
    assert_eq!(
        refusal(&service, asked).await,
        (invalid, "invalid run_budget_usd".into())
    );
    let mut asked = request(&h, "w");
    asked.skill = "nope".into();
    assert_eq!(
        refusal(&service, asked).await,
        (tonic::Code::NotFound, "unknown_skill".into())
    );
    assert!(
        h.store.running().await.unwrap().is_empty(),
        "nothing stored"
    );

    for index in 0..5 {
        service
            .start_watch(Request::new(request(&h, &format!("w{index}"))))
            .await
            .unwrap();
    }
    assert_eq!(
        refusal(&service, request(&h, "sixth")).await,
        (tonic::Code::FailedPrecondition, "too_many_watches".into())
    );
}

#[tokio::test(start_paused = true)]
async fn stop_refuses_an_unknown_and_an_ended_watch() {
    let h = harness(settings(), vec![answer("QUIET", 0.01)]).await;
    let service = service(&h);
    let stop = |name: &str| pb::StopWatchRequest { name: name.into() };

    let status = service
        .stop_watch(Request::new(stop("nope")))
        .await
        .unwrap_err();
    assert_eq!(
        (status.code(), status.message()),
        (tonic::Code::NotFound, "unknown_watch")
    );
    service
        .start_watch(Request::new(request(&h, "w")))
        .await
        .unwrap();
    service.stop_watch(Request::new(stop("w"))).await.unwrap();
    let status = service
        .stop_watch(Request::new(stop("w")))
        .await
        .unwrap_err();
    assert_eq!(
        (status.code(), status.message()),
        (tonic::Code::FailedPrecondition, "not_running")
    );
}

#[tokio::test(start_paused = true)]
async fn a_spent_day_refuses_start_with_daily_budget_spent() {
    let settings = crate::settings::WatchSettings {
        daily_budget_usd: Some(0.05),
        ..settings()
    };
    let h = harness(settings.clone(), vec![answer("QUIET", 0.06)]).await;
    let service = WatchToolService::new(Arc::clone(&h.scheduler), settings);
    service
        .start_watch(Request::new(request(&h, "a")))
        .await
        .unwrap();
    pass(Duration::from_secs(5)).await;
    assert_eq!(
        refusal(&service, request(&h, "b")).await,
        (tonic::Code::FailedPrecondition, "daily_budget_spent".into())
    );
}

#[tokio::test(start_paused = true)]
async fn a_run_limit_above_the_setting_is_refused_with_the_setting_and_its_value() {
    let settings = crate::settings::WatchSettings {
        max_run_limit_secs: Some(300),
        ..settings()
    };
    let h = harness(settings.clone(), vec![answer("QUIET", 0.01)]).await;
    let service = WatchToolService::new(Arc::clone(&h.scheduler), settings);
    let mut asked = request(&h, "w");
    asked.run_limit_secs = 301;
    assert_eq!(
        refusal(&service, asked).await,
        (
            tonic::Code::InvalidArgument,
            "run_limit_secs is above [watch] max_run_limit_secs (300)".into()
        )
    );
    let mut asked = request(&h, "w");
    asked.run_limit_secs = 300;
    service.start_watch(Request::new(asked)).await.unwrap();
}

#[test]
fn every_scheduler_refusal_maps_to_its_code_and_message() {
    use tonic::Code::{FailedPrecondition, NotFound, Unavailable};
    let cases = [
        (
            start_status(StartRefusal::TooManyWatches),
            FailedPrecondition,
            "too_many_watches",
        ),
        (
            start_status(StartRefusal::DailyBudgetSpent),
            FailedPrecondition,
            "daily_budget_spent",
        ),
        (
            start_status(StartRefusal::DailyBudgetUnset),
            FailedPrecondition,
            "daily_budget_unset",
        ),
        (
            start_status(StartRefusal::Unavailable),
            Unavailable,
            "store_failed",
        ),
        (
            pause_status(PauseRefusal::UnknownWatch),
            NotFound,
            "unknown_watch",
        ),
        (
            pause_status(PauseRefusal::NotRunning),
            FailedPrecondition,
            "not_running",
        ),
        (
            pause_status(PauseRefusal::BudgetDay),
            FailedPrecondition,
            "budget_day",
        ),
        (
            pause_status(PauseRefusal::Unavailable),
            Unavailable,
            "store_failed",
        ),
        (
            resume_status(ResumeRefusal::UnknownWatch),
            NotFound,
            "unknown_watch",
        ),
        (
            resume_status(ResumeRefusal::NotPaused),
            FailedPrecondition,
            "not_paused",
        ),
        (
            resume_status(ResumeRefusal::BudgetDay),
            FailedPrecondition,
            "budget_day",
        ),
        (
            resume_status(ResumeRefusal::Unavailable),
            Unavailable,
            "store_failed",
        ),
        (
            remind_status(RemindRefusal::NameInUse),
            FailedPrecondition,
            "name_in_use",
        ),
        (
            remind_status(RemindRefusal::Unavailable),
            Unavailable,
            "store_failed",
        ),
    ];
    for (status, code, message) in cases {
        assert_eq!((status.code(), status.message()), (code, message));
    }
}

#[tokio::test(start_paused = true)]
async fn pause_resume_and_remind_check_their_input_first() {
    let h = harness(settings(), vec![]).await;
    let service = service(&h);
    let invalid = tonic::Code::InvalidArgument;
    let status = service
        .pause(Request::new(pb::PauseRequest {
            name: "w".into(),
            all: true,
            ..Default::default()
        }))
        .await
        .expect_err("refused");
    assert_eq!((status.code(), status.message()), (invalid, "invalid name"));
    let status = service
        .pause(Request::new(pb::PauseRequest {
            name: "w".into(),
            for_minutes: Some(1441),
            ..Default::default()
        }))
        .await
        .expect_err("refused");
    assert_eq!(
        (status.code(), status.message()),
        (invalid, "invalid for_minutes")
    );
    let status = service
        .resume(Request::new(pb::ResumeRequest {
            name: "w".into(),
            caller: "chief".into(),
            ..Default::default()
        }))
        .await
        .expect_err("refused");
    assert_eq!(
        (status.code(), status.message()),
        (invalid, "invalid caller")
    );
    let status = service
        .remind(Request::new(pb::RemindRequest {
            name: "r".into(),
            folder: h.folder.0.to_string_lossy().into_owned(),
            when: None,
            report_to: "chief".into(),
            text: "x".into(),
        }))
        .await
        .expect_err("refused");
    assert_eq!((status.code(), status.message()), (invalid, "invalid when"));
}

#[tokio::test(start_paused = true)]
async fn an_open_ended_watch_pauses_and_resumes_through_the_service() {
    let budget = crate::settings::WatchSettings {
        daily_budget_usd: Some(1.0),
        ..settings()
    };
    let h = harness(budget.clone(), vec![answer("QUIET", 0.01)]).await;
    let service = WatchToolService::new(Arc::clone(&h.scheduler), budget);
    let mut open = request(&h, "pods");
    open.for_minutes = None;

    let started = service
        .start_watch(Request::new(open))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(started.ends_at_ms, None);
    pass(Duration::from_secs(5)).await;
    let paused = service
        .pause(Request::new(pb::PauseRequest {
            name: "pods".into(),
            for_minutes: Some(30),
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        (paused.paused, paused.skipped),
        (vec!["pods".to_string()], vec![])
    );

    let listed = service
        .list_watches(Request::new(pb::ListWatchesRequest {}))
        .await
        .unwrap()
        .into_inner();
    let watch = &listed.watches[0];
    assert!(watch.paused && !watch.running);
    assert_eq!(watch.ends_at_ms, None);
    assert_eq!(watch.pause_how, "cli");
    assert!(watch.paused_until_ms.is_some());

    let resumed = service
        .resume(Request::new(pb::ResumeRequest {
            all: true,
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        (resumed.resumed, resumed.skipped),
        (vec!["pods".to_string()], vec![])
    );
    let status = service
        .resume(Request::new(pb::ResumeRequest {
            name: "pods".into(),
            ..Default::default()
        }))
        .await
        .expect_err("refused");
    assert_eq!(
        (status.code(), status.message()),
        (tonic::Code::FailedPrecondition, "not_paused")
    );
}
