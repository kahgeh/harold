//! Tests of the evidence service, on a temporary registry and a temporary repository.

use harold_api::tools::evidence::v1::CheckRequest;
use harold_api::tools::evidence::v1::evidence_service_server::EvidenceService;
use std::sync::Arc;
use time::{OffsetDateTime, UtcOffset};
use tonic::Request;

use super::check_tests::{FakeSource, Fixture, SERIAL, SQL, tagged};
use super::service::EvidenceToolService;
use crate::pod_stats::SystemGit;

fn registry(fixture: &Fixture, paused: bool) -> std::path::PathBuf {
    let path = fixture.root.join("products.toml");
    std::fs::write(
        &path,
        format!(
            "[products.evidence-fixture]\nrepo = \"{}\"\nbase = \"main\"\npaused = {paused}\n",
            fixture.repo.display()
        ),
    )
    .unwrap();
    path
}

fn service(fixture: &Fixture, source: Arc<FakeSource>, paused: bool) -> EvidenceToolService {
    EvidenceToolService::with_parts(
        Some(registry(fixture, paused)),
        fixture.settings(),
        SystemGit::default(),
        source,
    )
}

fn call(product: &str, id: &str) -> Request<CheckRequest> {
    Request::new(CheckRequest {
        product: product.into(),
        id: id.into(),
    })
}

#[tokio::test]
async fn an_unknown_product_is_not_found() {
    let fixture = tagged(&[("s-1.sql", SQL)]);
    let service = service(
        &fixture,
        Arc::new(FakeSource::new(vec![FakeSource::row("1", "x")])),
        false,
    );
    let error = service.check(call("nope", "")).await.expect_err("refused");
    assert_eq!(error.code(), tonic::Code::NotFound);
    assert_eq!(error.message(), "unknown_product");
}

#[tokio::test]
async fn no_registry_is_unavailable() {
    let fixture = tagged(&[("s-1.sql", SQL)]);
    let source = Arc::new(FakeSource::new(vec![FakeSource::row("1", "x")]));
    let service =
        EvidenceToolService::with_parts(None, fixture.settings(), SystemGit::default(), source);
    let error = service
        .check(call("evidence-fixture", ""))
        .await
        .expect_err("refused");
    assert_eq!(error.code(), tonic::Code::Unavailable);
    assert_eq!(error.message(), "registry_unreadable");
}

#[tokio::test]
async fn check_gives_the_state_and_the_results_of_the_named_spec() {
    let _serial = SERIAL.lock().await;
    let fixture = tagged(&[("s-1.sql", SQL)]);
    let source = Arc::new(FakeSource::new(vec![FakeSource::row("1", "use_seen")]));
    let service = service(&fixture, source, false);
    let response = service
        .check(call("evidence-fixture", "s"))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(response.specs.len(), 1);
    assert_eq!(response.specs[0].id, "s");
    assert_eq!(response.specs[0].state, "present");
    assert_eq!(response.specs[0].results[0].name, "s-1");
    assert!(response.specs[0].results[0].present);
    assert_eq!(response.specs[0].results[0].reason, "use_seen");
    assert_eq!(response.specs[0].failed, "");
}

#[tokio::test]
async fn a_named_product_is_checked_even_when_paused_and_the_daily_pass_skips_it() {
    let _serial = SERIAL.lock().await;
    let fixture = tagged(&[("s-1.sql", SQL)]);
    let source = Arc::new(FakeSource::new(vec![FakeSource::row("1", "use_seen")]));
    let service = service(&fixture, Arc::clone(&source), true);
    let pass = service
        .pass(OffsetDateTime::now_utc(), UtcOffset::UTC)
        .await;
    assert!(pass.is_empty());
    assert!(source.seen.lock().unwrap().is_empty());
    let response = service
        .check(call("evidence-fixture", ""))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(response.specs.len(), 1);
}

#[tokio::test]
async fn the_daily_pass_checks_each_product_that_is_not_paused() {
    let _serial = SERIAL.lock().await;
    let fixture = tagged(&[("s-1.sql", SQL)]);
    let source = Arc::new(FakeSource::new(vec![FakeSource::row("1", "use_seen")]));
    let service = service(&fixture, source, false);
    let pass = service
        .pass(OffsetDateTime::now_utc(), UtcOffset::UTC)
        .await;
    assert_eq!(pass.len(), 1);
    assert_eq!(pass[0].state.as_str(), "present");
}
