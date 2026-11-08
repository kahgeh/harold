//! Tests of the evidence line to the chief. A fake writer takes the place of the chief's
//! pane: no test types into a real pane or looks up the real chief.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use time::{OffsetDateTime, UtcOffset};

use super::check_tests::{FakeSource, SERIAL, SQL, tagged};
use super::chief_line::{ChiefLine, LineResult, evidence_line};
use super::service::EvidenceToolService;
use super::state::QueryResult;
use crate::pod_stats::SystemGit;
use crate::telemetry::test_support;

/// Answers each call with the next scripted result; the last one repeats. Keeps each line.
struct FakeChief {
    results: Mutex<Vec<LineResult>>,
    lines: Mutex<Vec<String>>,
}

impl FakeChief {
    fn new(results: Vec<LineResult>) -> Arc<Self> {
        Arc::new(Self {
            results: Mutex::new(results),
            lines: Mutex::new(Vec::new()),
        })
    }

    fn lines(&self) -> Vec<String> {
        self.lines.lock().unwrap().clone()
    }
}

impl ChiefLine for FakeChief {
    fn type_line<'a>(
        &'a self,
        line: &'a str,
    ) -> Pin<Box<dyn Future<Output = LineResult> + Send + 'a>> {
        self.lines.lock().unwrap().push(line.to_string());
        let mut results = self.results.lock().unwrap();
        let result = if results.len() > 1 {
            results.remove(0)
        } else {
            results[0]
        };
        Box::pin(async move { result })
    }
}

fn at(text: &str) -> OffsetDateTime {
    OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339).unwrap()
}

fn service(
    fixture: &super::check_tests::Fixture,
    source: Arc<FakeSource>,
    chief: Arc<FakeChief>,
) -> EvidenceToolService {
    let registry = fixture.root.join("products.toml");
    std::fs::write(
        &registry,
        format!(
            "[products.evidence-fixture]\nrepo = \"{}\"\nbase = \"main\"\n",
            fixture.repo.display()
        ),
    )
    .unwrap();
    EvidenceToolService::with_parts(
        Some(registry),
        fixture.settings(),
        SystemGit::default(),
        source,
    )
    .with_chief(chief)
}

const LATE: &str = "2026-11-20T10:00:00Z";

fn query(name: &str, reason: &str) -> QueryResult {
    QueryResult {
        name: name.to_string(),
        present: false,
        reason: reason.to_string(),
    }
}

#[test]
fn the_line_has_the_fixed_form_with_each_absent_query() {
    let one = evidence_line("harold", "s", &[query("s-2", "no_use_yet")]);
    assert_eq!(one, "EVIDENCE harold s missing: s-2 (no_use_yet)");
    let two = evidence_line(
        "harold",
        "s",
        &[query("s-1", "no_use_yet"), query("s-2", "no_gauge")],
    );
    assert_eq!(
        two,
        "EVIDENCE harold s missing: s-1 (no_use_yet), s-2 (no_gauge)"
    );
}

#[tokio::test]
async fn a_missing_file_gives_one_line_and_a_told_line_and_no_second_line() {
    let _serial = SERIAL.lock().await;
    let fixture = tagged(&[("s-1.sql", SQL)]);
    let source = Arc::new(FakeSource::new(vec![FakeSource::row("0", "no_use_yet")]));
    let chief = FakeChief::new(vec![LineResult::Typed]);
    let service = service(&fixture, source, Arc::clone(&chief));
    service.pass(at(LATE), UtcOffset::UTC).await;
    assert_eq!(
        chief.lines(),
        vec!["EVIDENCE evidence-fixture s missing: s-1 (no_use_yet)".to_string()]
    );
    let text = std::fs::read_to_string(fixture.pod().join("evidence/s.md")).unwrap();
    assert_eq!(
        text.matches("told: 2026-11-20T10:00:00Z").count(),
        1,
        "{text}"
    );
    service
        .pass(at("2026-11-21T10:00:00Z"), UtcOffset::UTC)
        .await;
    service
        .check_named(
            "evidence-fixture",
            "s",
            at("2026-11-22T10:00:00Z"),
            UtcOffset::UTC,
        )
        .await
        .unwrap();
    assert_eq!(chief.lines().len(), 1);
    let after = std::fs::read_to_string(fixture.pod().join("evidence/s.md")).unwrap();
    assert_eq!(after.matches("told:").count(), 1, "{after}");
}

#[tokio::test]
async fn two_absent_queries_give_one_line_with_both() {
    let _serial = SERIAL.lock().await;
    let fixture = tagged(&[("s-1.sql", SQL), ("s-2.sql", SQL)]);
    let source = Arc::new(FakeSource::new(vec![FakeSource::row("0", "no_use_yet")]));
    let chief = FakeChief::new(vec![LineResult::Typed]);
    service(&fixture, source, Arc::clone(&chief))
        .pass(at(LATE), UtcOffset::UTC)
        .await;
    assert_eq!(
        chief.lines(),
        vec!["EVIDENCE evidence-fixture s missing: s-1 (no_use_yet), s-2 (no_use_yet)"]
    );
}

#[tokio::test]
async fn no_chief_gives_no_told_line_and_the_next_pass_tries_again() {
    let _serial = SERIAL.lock().await;
    test_support::install();
    let counted = |outcome: &str| {
        test_support::exact_counter_value("harold.evidence.lines", &[("outcome", outcome)])
            .unwrap_or(0)
    };
    let (no_chief, not_typed, typed) =
        (counted("no_chief"), counted("not_typed"), counted("typed"));
    let fixture = tagged(&[("s-1.sql", SQL)]);
    let source = Arc::new(FakeSource::new(vec![FakeSource::row("0", "no_use_yet")]));
    let chief = FakeChief::new(vec![
        LineResult::NoChief,
        LineResult::NotTyped("pane_busy"),
        LineResult::Typed,
    ]);
    let service = service(&fixture, source, Arc::clone(&chief));
    service.pass(at(LATE), UtcOffset::UTC).await;
    let path = fixture.pod().join("evidence/s.md");
    assert!(!std::fs::read_to_string(&path).unwrap().contains("told:"));
    assert_eq!(counted("no_chief"), no_chief + 1);
    service
        .pass(at("2026-11-21T10:00:00Z"), UtcOffset::UTC)
        .await;
    assert!(!std::fs::read_to_string(&path).unwrap().contains("told:"));
    assert_eq!(counted("not_typed"), not_typed + 1);
    service
        .pass(at("2026-11-22T10:00:00Z"), UtcOffset::UTC)
        .await;
    assert!(std::fs::read_to_string(&path).unwrap().contains("told:"));
    assert_eq!(counted("typed"), typed + 1);
    assert_eq!(chief.lines().len(), 3);
}

#[tokio::test]
async fn a_waiting_or_present_spec_gets_no_line() {
    let _serial = SERIAL.lock().await;
    let fixture = tagged(&[("s-1.sql", SQL)]);
    let chief = FakeChief::new(vec![LineResult::Typed]);
    let waiting = Arc::new(FakeSource::new(vec![FakeSource::row("0", "no_use_yet")]));
    service(&fixture, waiting, Arc::clone(&chief))
        .pass(at("2026-11-14T00:00:00Z"), UtcOffset::UTC)
        .await;
    let present = Arc::new(FakeSource::new(vec![FakeSource::row("1", "use_seen")]));
    service(&fixture, present, Arc::clone(&chief))
        .check_named("evidence-fixture", "s", at(LATE), UtcOffset::UTC)
        .await
        .unwrap();
    assert!(chief.lines().is_empty());
}

#[tokio::test]
async fn a_typed_line_counts_the_feature_once() {
    let _serial = SERIAL.lock().await;
    test_support::install();
    let feature = || {
        test_support::exact_counter_value(
            "harold.feature.used",
            &[("spec.id", "evidence-follow-up")],
        )
        .unwrap_or(0)
    };
    let start = feature();
    let fixture = tagged(&[("s-1.sql", SQL)]);
    let source = Arc::new(FakeSource::new(vec![FakeSource::row("0", "no_use_yet")]));
    let chief = FakeChief::new(vec![LineResult::NoChief, LineResult::Typed]);
    let service = service(&fixture, source, chief);
    service.pass(at(LATE), UtcOffset::UTC).await;
    assert_eq!(feature(), start, "no line, no use");
    service
        .pass(at("2026-11-21T10:00:00Z"), UtcOffset::UTC)
        .await;
    assert_eq!(feature(), start + 1);
}
