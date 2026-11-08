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

// ---------------------------------------------------------------------------
// The writer and the turn end, on the fake ports of the timed prompts
// ---------------------------------------------------------------------------

use super::chief_line::{EvidenceTurnEnds, TmuxChiefLine};
use crate::agent::domain::EffectiveAgentState;
use crate::restart_reminder::TurnEnds;
use crate::timed_prompts::tests::{CHIEF, Fake, Key};

fn chief_fake() -> Fake {
    Fake::at("2026-10-23", "10:00:00").with_chief(CHIEF, None)
}

fn typed_lines(fake: &Fake) -> Vec<String> {
    fake.keys()
        .into_iter()
        .filter_map(|key| match key {
            Key::Text(_, text) => Some(text),
            Key::Enter(_) | Key::Press(..) => None,
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn the_writer_types_the_line_into_the_chief_pane_and_presses_enter() {
    let fake = chief_fake();
    let writer = TmuxChiefLine::new(Arc::new(fake.clone()));
    let result = writer
        .type_line("EVIDENCE p s missing: s-1 (no_use_yet)")
        .await;
    assert_eq!(result, LineResult::Typed);
    assert_eq!(
        fake.keys(),
        vec![
            Key::Text("%2".into(), "EVIDENCE p s missing: s-1 (no_use_yet)".into()),
            Key::Enter("%2".into()),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn the_writer_without_a_chief_types_nothing() {
    let fake = Fake::at("2026-10-23", "10:00:00");
    let writer = TmuxChiefLine::new(Arc::new(fake.clone()));
    assert_eq!(
        writer.type_line("EVIDENCE p s missing: s-1 (x)").await,
        LineResult::NoChief
    );
    assert!(fake.keys().is_empty());
}

#[tokio::test(start_paused = true)]
async fn the_writer_does_not_type_into_a_busy_chief() {
    let fake = chief_fake();
    fake.set(|state| {
        for pane in &mut state.view.panes {
            pane.state = EffectiveAgentState::Working;
        }
    });
    let writer = TmuxChiefLine::new(Arc::new(fake.clone()));
    let result = writer.type_line("EVIDENCE p s missing: s-1 (x)").await;
    assert!(matches!(result, LineResult::NotTyped(_)), "{result:?}");
    assert!(fake.keys().is_empty());
}

/// A product folder with a `missing` evidence file and no `told:` line, and its registry.
fn missing_product() -> (super::check_tests::Fixture, std::path::PathBuf) {
    let fixture = tagged(&[("s-1.sql", SQL)]);
    let folder = fixture.pod().join("evidence");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(
        folder.join("s.md"),
        "# Evidence: s\n\nchecked: 2026-11-20T10:00:00Z\nsince: 2026-11-12T03:05:00Z\nreleased: 2026-11-13T10:00:00Z\nstate: missing\nquery: s-1 absent no_use_yet\n",
    )
    .unwrap();
    let registry = fixture.root.join("products.toml");
    std::fs::write(
        &registry,
        format!(
            "[products.evidence-fixture]\nrepo = \"{}\"\nbase = \"main\"\n",
            fixture.repo.display()
        ),
    )
    .unwrap();
    (fixture, registry)
}

#[tokio::test(start_paused = true)]
async fn a_chief_turn_end_types_the_waiting_line_and_a_turn_end_of_another_pane_does_not() {
    let (fixture, registry) = missing_product();
    let fake = chief_fake();
    let ends = EvidenceTurnEnds::new(Arc::new(fake.clone()), Some(registry), None);
    ends.on_turn_end("%9", "", "").await;
    assert!(
        typed_lines(&fake).is_empty(),
        "a pane that is not the chief's"
    );
    ends.on_turn_end("%2", "", "").await;
    assert_eq!(
        typed_lines(&fake),
        vec!["EVIDENCE evidence-fixture s missing: s-1 (no_use_yet)".to_string()]
    );
    let text = std::fs::read_to_string(fixture.pod().join("evidence/s.md")).unwrap();
    assert_eq!(text.matches("told:").count(), 1, "{text}");
    ends.on_turn_end("%2", "", "").await;
    assert_eq!(typed_lines(&fake).len(), 1, "one line for each spec");
}

#[tokio::test(start_paused = true)]
async fn the_turn_end_hook_also_passes_the_turn_end_on_to_the_next_hook() {
    struct Seen(Mutex<Vec<String>>);
    impl TurnEnds for Seen {
        fn turn_ended(&self, pane_id: &str, _: &str, _: &str) {
            self.0.lock().unwrap().push(pane_id.to_string());
        }
    }
    let next = Arc::new(Seen(Mutex::new(Vec::new())));
    let ends = EvidenceTurnEnds::new(
        Arc::new(chief_fake()),
        None,
        Some(Arc::clone(&next) as Arc<dyn TurnEnds>),
    );
    ends.turn_ended("%2", "s", "t");
    assert_eq!(*next.0.lock().unwrap(), vec!["%2".to_string()]);
}
