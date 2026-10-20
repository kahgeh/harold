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
        for_minutes: 120,
        report_to: "chief".into(),
        run_budget_usd: 0.0,
        every_run: false,
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
    assert!(started.ends_at_ms > 0);
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
    asked.for_minutes = 1441;
    assert_eq!(
        refusal(&service, asked).await,
        (invalid, "invalid for_minutes".into())
    );
    let mut asked = request(&h, "w");
    asked.report_to = "/Users/x/Dev/p/other".into();
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
