use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use opentelemetry::trace::{Status, TraceContextExt};
use tracing_opentelemetry::OpenTelemetrySpanExt;
use tracing_subscriber::Layer as _;
use tracing_subscriber::layer::SubscriberExt;

use super::*;

fn unique_pane() -> String {
    format!("%test-{}", uuid::Uuid::new_v4())
}

// ---------------------------------------------------------------------------
// Allowed fields
// ---------------------------------------------------------------------------

#[test]
fn the_model_failed_fallback_is_counted_from_0_at_start() {
    test_support::install();
    register_phone_counters();
    let counted = test_support::exact_counter_value(
        "harold.turn_end_updates",
        &[("outcome", "fallback"), ("reason", "model_failed")],
    );
    assert!(counted.is_some());
}

#[test]
fn only_allowed_fields_leave_a_span() {
    test_support::install();
    let pane = unique_pane();
    {
        let span = tracing::info_span!(target: TRACE_TARGET, "harold.route",
            pane_id = %pane, text = "TELEMETRY-CANARY-7731", error = "TELEMETRY-CANARY-7731");
        let _entered = span.enter();
        tracing::info!(pane_id = %pane, msg = "TELEMETRY-CANARY-7731", "inside");
        tracing::error!(pane_id = %pane, "send failed");
    }
    let spans = test_support::spans();
    let span = spans
        .iter()
        .find(|s| s.attributes.iter().any(|kv| kv.value.as_str() == pane))
        .unwrap();
    assert!(
        span.attributes
            .iter()
            .all(|kv| ALLOWED_FIELDS.contains(&kv.key.as_str())),
        "{:?}",
        span.attributes
    );
    assert!(span.events.is_empty());
    assert!(span.links.is_empty());
    // The error event inside it marks it as failed.
    assert_eq!(span.status, Status::error(""));
    let exported = test_support::exported_text();
    assert!(exported.contains(&pane));
    assert!(!exported.contains("TELEMETRY-CANARY-7731"));
}

#[test]
fn an_error_status_keeps_the_error_and_drops_its_description() {
    test_support::install();
    let pane = unique_pane();
    tracing::info_span!(target: TRACE_TARGET, "harold.delivery",
        pane_id = %pane, otel.status_description = "TELEMETRY-CANARY-4410")
    .in_scope(|| {});
    let spans = test_support::spans();
    let span = spans
        .iter()
        .find(|s| s.attributes.iter().any(|kv| kv.value.as_str() == pane))
        .unwrap();
    assert_eq!(span.status, Status::error(""));
    assert!(!test_support::exported_text().contains("TELEMETRY-CANARY-4410"));
}

#[test]
fn only_allowed_fields_leave_a_log_record_and_its_body_stays() {
    test_support::install();
    let pane = unique_pane();
    tracing::warn!(pane_id = %pane, reason = "busy", text = "TELEMETRY-CANARY-5520", recipient = "TELEMETRY-CANARY-5520", "reply not typed");
    let logs = test_support::logs();
    let record = logs
        .iter()
        .find(|l| format!("{:?}", l.record).contains(&pane))
        .unwrap();
    let keys: Vec<&str> = record
        .record
        .attributes_iter()
        .map(|(key, _)| key.as_str())
        .collect();
    assert_eq!(keys, vec!["pane_id", "reason"]);
    assert_eq!(
        record.record.body(),
        Some(&opentelemetry::logs::AnyValue::from("reply not typed"))
    );
    assert_eq!(
        record.record.severity_number(),
        Some(opentelemetry::logs::Severity::Warn)
    );
    assert!(!test_support::exported_text().contains("TELEMETRY-CANARY-5520"));
}

#[test]
fn an_exported_log_record_carries_the_time_it_was_logged() {
    test_support::install();
    let pane = unique_pane();
    let before = std::time::SystemTime::now();
    tracing::info!(pane_id = %pane, "timestamp probe");
    let after = std::time::SystemTime::now();
    let logs = test_support::logs();
    let record = logs
        .iter()
        .find(|l| format!("{:?}", l.record).contains(&pane))
        .unwrap();
    let timestamp = record.record.timestamp().expect("a record time is set");
    assert!(before <= timestamp && timestamp <= after);
}

#[test]
fn allowed_record_keeps_a_record_time_and_fills_a_missing_one() {
    use opentelemetry::logs::{LogRecord as _, Logger as _, LoggerProvider as _};
    use std::time::{SystemTime, UNIX_EPOCH};

    let logger = SdkLoggerProvider::builder().build().logger("test");
    let logged = UNIX_EPOCH + Duration::from_secs(1_760_559_413);
    let observed = logged + Duration::from_secs(5);

    let mut both = logger.create_log_record();
    both.set_timestamp(logged);
    both.set_observed_timestamp(observed);
    let allowed = allowed_record(&both);
    assert_eq!(allowed.timestamp(), Some(logged));
    assert_eq!(allowed.observed_timestamp(), Some(observed));

    let mut only_observed = logger.create_log_record();
    only_observed.set_observed_timestamp(observed);
    let allowed = allowed_record(&only_observed);
    assert_eq!(allowed.timestamp(), Some(observed));
    assert_eq!(allowed.observed_timestamp(), Some(observed));

    let before = SystemTime::now();
    let allowed = allowed_record(&logger.create_log_record());
    let after = SystemTime::now();
    let timestamp = allowed.timestamp().expect("a record time is set");
    assert!(before <= timestamp && timestamp <= after);
}

#[test]
fn every_export_carries_the_service_and_machine() {
    test_support::install();
    let pane = unique_pane();
    tracing::info!(pane_id = %pane, "resource probe");
    let logs = test_support::logs();
    let record = logs
        .iter()
        .find(|l| format!("{:?}", l.record).contains(&pane))
        .unwrap();
    let attribute = |key: &'static str| {
        record
            .resource
            .get(&opentelemetry::Key::from_static_str(key))
            .map(|value| value.to_string())
    };
    assert_eq!(attribute("service.name").as_deref(), Some("harold"));
    assert_eq!(
        attribute("harold.machine").as_deref(),
        Some(test_support::MACHINE)
    );
}

#[test]
fn internal_and_health_targets_are_not_exported_as_logs() {
    assert!(!is_exported_log_target("harold_api"));
    assert!(!is_exported_log_target("eventsource::x"));
    assert!(is_exported_log_target("harold"));
    assert!(is_exported_log_target("events::store"));
    test_support::install();
    let pane = unique_pane();
    tracing::warn!(target: HEALTH_TARGET, pane_id = %pane, "health probe");
    tracing::warn!(target: "tonic::transport", pane_id = %pane, "tonic probe");
    tracing::warn!(target: "h2::codec", pane_id = %pane, "h2 probe");
    tracing::warn!(target: "opentelemetry_sdk", pane_id = %pane, "sdk probe");
    assert!(!test_support::exported_text().contains(&pane));
}

#[test]
fn only_harold_and_events_log_records_are_exported() {
    test_support::install();
    let logs = crate::outbound::fake::CapturedLogs::start();
    let canary = format!("TELEMETRY-CANARY-{}", uuid::Uuid::new_v4());
    let pane = unique_pane();
    // Other crates interpolate errors and paths into their messages, as turso_core does.
    tracing::error!(target: "turso_core::statement", "{}", canary);
    tracing::error!(target: "reqwest::blocking::client", "request failed: {}", canary);
    tracing::error!(target: "harold_api", "{}", canary);
    tracing::error!(target: "eventsource", "{}", canary);
    tracing::error!(target: "harold::projector", pane_id = %pane, "harold probe");
    tracing::error!(target: "events::store", pane_id = %pane, "events probe");
    tracing::error!(target: "events", pane_id = %pane, "events root probe");
    tracing::error!(pane_id = %pane, "module target probe");
    // Records from other crates still reach harold.log.
    assert!(logs.text().contains(&canary), "{}", logs.text());
    let exported = test_support::exported_text();
    assert!(!exported.contains(&canary));
    let bodies: Vec<String> = test_support::logs()
        .iter()
        .filter(|l| format!("{:?}", l.record).contains(&pane))
        .map(|l| format!("{:?}", l.record.body().unwrap()))
        .collect();
    assert_eq!(bodies.len(), 4, "{bodies:#?}");
    for probe in [
        "harold probe",
        "events probe",
        "events root probe",
        "module target probe",
    ] {
        assert!(
            bodies.iter().any(|body| body.contains(probe)),
            "{probe}: {bodies:#?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Traces and harold.log
// ---------------------------------------------------------------------------

#[test]
fn a_log_inside_a_harold_span_carries_its_trace_and_no_span_prefix_in_harold_log() {
    test_support::install();
    let logs = crate::outbound::fake::CapturedLogs::start();
    let pane = unique_pane();
    let span = tracing::info_span!(target: TRACE_TARGET, "harold.route", pane_id = %pane);
    let trace_id = span.context().span().span_context().trace_id();
    span.in_scope(|| tracing::info!(pane_id = %pane, "routing reply"));
    drop(span);
    assert!(logs.text().contains("routing reply"));
    assert!(!logs.text().contains("harold.route"), "{}", logs.text());
    let exported = test_support::logs();
    let record = exported
        .iter()
        .find(|l| format!("{:?}", l.record).contains(&pane))
        .unwrap();
    assert_eq!(record.record.trace_context().unwrap().trace_id, trace_id);
}

/// A `MakeWriter` that appends to a shared buffer.
#[derive(Clone, Default)]
struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for SharedBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl SharedBuffer {
    fn lines_without_timestamps(&self) -> Vec<String> {
        String::from_utf8(self.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .map(|line| {
                let (timestamp, rest) = line.split_once(' ').unwrap();
                assert!(timestamp.ends_with('Z'), "{line}");
                rest.to_string()
            })
            .collect()
    }
}

fn write_sample_lines() {
    let pane = "%7";
    let outer = tracing::info_span!("listener", pane_id = pane);
    let _outer = outer.enter();
    tracing::info!(pane_id = pane, "reply stored");
    let traced = tracing::info_span!(target: TRACE_TARGET, "harold.route", pane_id = pane);
    traced.in_scope(|| {
        tracing::warn!(reason = "busy", "reply not typed");
        tracing::debug!("not at info");
        tracing::error!(error = "boom", "send failed");
    });
}

#[test]
fn harold_log_lines_are_those_of_the_old_subscriber_without_harold_trace_spans() {
    let old = SharedBuffer::default();
    let old_writer = old.clone();
    let old_subscriber = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("info"))
        .with_ansi(false)
        .with_writer(move || old_writer.clone())
        .finish();
    tracing::subscriber::with_default(old_subscriber, write_sample_lines);

    let new = SharedBuffer::default();
    let new_writer = new.clone();
    let new_subscriber = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(move || new_writer.clone())
            .with_filter(harold_log_filter(tracing_subscriber::EnvFilter::new(
                "info",
            ))),
    );
    tracing::subscriber::with_default(new_subscriber, write_sample_lines);

    let old_lines = old.lines_without_timestamps();
    let new_lines = new.lines_without_timestamps();
    assert_eq!(old_lines.len(), 3, "{old_lines:#?}");
    assert_eq!(new_lines[0], old_lines[0]);
    // The old subscriber prints the `harold.route` span as a prefix; the new one does not.
    for (old_line, new_line) in old_lines.iter().zip(&new_lines).skip(1) {
        let without_route = old_line.replace("harold.route{pane_id=\"%7\"}:", "");
        assert_eq!(*new_line, without_route);
        assert_ne!(*new_line, *old_line);
    }
    assert_eq!(new_lines.len(), old_lines.len());
}

// ---------------------------------------------------------------------------
// Collector health
// ---------------------------------------------------------------------------

#[test]
fn health_reports_one_failing_line_across_failures_and_one_recovered_line() {
    let changes = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&changes);
    let health = ExportHealth::new(Box::new(move |c| sink.lock().unwrap().push(c)));
    for ok in [false, false, false, true, true, false] {
        health.observe(ok);
    }
    assert_eq!(
        *changes.lock().unwrap(),
        vec![
            HealthChange::Failing,
            HealthChange::Recovered,
            HealthChange::Failing
        ]
    );
}

#[test]
fn the_health_lines_are_written_to_harold_log() {
    test_support::install();
    let logs = crate::outbound::fake::CapturedLogs::start();
    let health = ExportHealth::logging("http://127.0.0.1:9");
    health.observe(false);
    health.observe(false);
    health.observe(true);
    let text = logs.text();
    assert_eq!(
        text.matches("WARN harold::telemetry_health: telemetry export failing (collector unreachable at http://127.0.0.1:9)")
            .count(),
        1,
        "{text}"
    );
    assert_eq!(
        text.matches("INFO harold::telemetry_health: telemetry export recovered")
            .count(),
        1,
        "{text}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_closed_collector_port_does_not_slow_harold_and_logs_one_failing_line() {
    // A port nobody listens on: bind, read the port, drop the listener.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let endpoint = format!("http://127.0.0.1:{port}");
    let changes = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&changes);
    let pipeline = Pipeline::otlp(
        &endpoint,
        "test-mac",
        ExportHealth::new(Box::new(move |c| sink.lock().unwrap().push(c))),
    )
    .unwrap();
    let subscriber = pipeline.subscriber_for_test();
    let started = std::time::Instant::now();
    tracing::subscriber::with_default(subscriber, || {
        // Turn-end updates delivered as the event handler delivers them, through the
        // daemon's dispatcher and a fake environment.
        let environment = Arc::new(crate::outbound::fake::FakeEnvironment::new(""));
        for _ in 0..20 {
            let delivering = std::time::Instant::now();
            let delivered = crate::projector::tests::dispatch_turn_end_with_fake_environment(
                &unique_pane(),
                &environment,
            );
            assert!(matches!(
                delivered,
                Ok(crate::outbound::DeliveryOutcome::Delivered)
            ));
            assert!(delivering.elapsed() < Duration::from_secs(1));
        }
        assert_eq!(environment.delivered().len(), 20);
    });
    assert!(started.elapsed() < Duration::from_secs(1));
    for _ in 0..3 {
        let _ = pipeline.force_flush();
    }
    assert_eq!(*changes.lock().unwrap(), vec![HealthChange::Failing]);
    let stopping = std::time::Instant::now();
    pipeline.shutdown_within(Duration::from_secs(2));
    assert!(stopping.elapsed() < Duration::from_millis(2500));
    assert!(started.elapsed() < Duration::from_secs(1 + 2 + 3 * 5 + 1));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_collector_that_never_answers_does_not_hold_up_shutdown() {
    // Accepts connections and never answers on them, so each send waits for its timeout.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let silent = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            held.push(socket);
        }
    });
    let pipeline =
        Pipeline::otlp(&endpoint, "test-mac", ExportHealth::new(Box::new(|_| {}))).unwrap();
    tracing::subscriber::with_default(pipeline.subscriber_for_test(), || {
        for _ in 0..10 {
            let span =
                tracing::info_span!(target: TRACE_TARGET, "harold.delivery", kind = "turn_end");
            span.in_scope(|| tracing::info!("turn-end update sent"));
        }
    });
    let guard = TelemetryGuard {
        pipeline: Some(pipeline),
    };
    let stopping = std::time::Instant::now();
    guard.shutdown().await;
    let took = stopping.elapsed();
    assert!(took < Duration::from_millis(2500), "{took:?}");
    silent.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn telemetry_off_shuts_down_at_once() {
    let started = std::time::Instant::now();
    TelemetryGuard::off().shutdown().await;
    assert!(started.elapsed() < Duration::from_millis(100));
}

// ---------------------------------------------------------------------------
// Local time
// ---------------------------------------------------------------------------

#[test]
fn the_local_offset_is_utc_until_main_sets_it() {
    // The test binary's `main` is the harness's, so nothing sets it.
    assert_eq!(local_offset(), time::UtcOffset::UTC);
}

// ---------------------------------------------------------------------------
// Phone requests
// ---------------------------------------------------------------------------

/// `minute` minutes after 2026-10-14 08:00 UTC. The `time` crate's `macros` feature is
/// off, so this parses rather than using `datetime!`.
fn at(minute: i64) -> OffsetDateTime {
    OffsetDateTime::parse(
        "2026-10-14T08:00:00Z",
        &time::format_description::well_known::Rfc3339,
    )
    .unwrap()
        + time::Duration::minutes(minute)
}

fn typed(pane: &str, tagged: bool) -> Routing {
    Routing::Typed {
        pane_id: pane.into(),
        resolved_by: if tagged {
            ResolvedBy::ExactTag
        } else {
            ResolvedBy::LatestNotifier
        },
        tagged,
        // `judge` takes the typing time as its own argument.
        typed_at: at(0),
    }
}

#[test]
fn resolved_by_has_the_spec_names_and_harold_chose_only_its_own_routes() {
    let all = [
        (ResolvedBy::ExactTag, "exact_tag", false),
        (ResolvedBy::TagSubstring, "tag_substring", false),
        (ResolvedBy::AiModel, "ai_model", true),
        (ResolvedBy::LatestNotifier, "latest_notifier", true),
        (ResolvedBy::MyAgent, "my_agent", true),
        (ResolvedBy::None, "none", false),
    ];
    for (resolved_by, name, chose) in all {
        assert_eq!(
            (resolved_by.as_str(), resolved_by.harold_chose()),
            (name, chose)
        );
    }
}

#[test]
fn a_reply_typed_on_time_is_typed_with_its_latency() {
    let v = PhoneBook::default().judge(
        "imessage",
        at(0),
        at(0) + time::Duration::seconds(8),
        &typed("%1", false),
    );
    assert_eq!(
        (v.outcome, v.latency),
        (Some("typed"), Some(Duration::from_secs(8)))
    );
}

#[test]
fn a_reply_with_no_pane_is_unavailable() {
    assert_eq!(
        PhoneBook::default()
            .judge("imessage", at(0), at(0), &Routing::NoPane)
            .outcome,
        Some("unavailable")
    );
}

#[test]
fn a_reply_typed_eleven_minutes_late_is_unavailable() {
    assert_eq!(
        PhoneBook::default()
            .judge("imessage", at(0), at(11), &typed("%1", false))
            .outcome,
        Some("unavailable")
    );
}

#[test]
fn a_tagged_reply_for_another_pane_within_ten_minutes_adds_one_wrong() {
    let mut book = PhoneBook::default();
    book.judge("imessage", at(0), at(0), &typed("%1", false));
    let v = book.judge("imessage", at(7), at(7), &typed("%2", true));
    assert_eq!(
        v.wrong,
        Some(("imessage".into(), ResolvedBy::LatestNotifier))
    );
    assert_eq!(v.outcome, Some("typed"));
}

#[test]
fn a_tag_for_the_same_pane_or_after_eleven_minutes_adds_no_wrong() {
    let mut book = PhoneBook::default();
    book.judge("imessage", at(0), at(0), &typed("%1", false));
    assert_eq!(
        book.judge("imessage", at(5), at(5), &typed("%1", true))
            .wrong,
        None
    );
    let mut book = PhoneBook::default();
    book.judge("imessage", at(0), at(0), &typed("%1", false));
    assert_eq!(
        book.judge("imessage", at(11), at(11), &typed("%2", true))
            .wrong,
        None
    );
}

#[test]
fn a_tagged_reply_after_a_user_tagged_reply_to_another_pane_adds_no_wrong() {
    // The earlier reply went where the user's tag said; a new tag for another pane is the user moving on.
    let mut book = PhoneBook::default();
    book.judge("imessage", at(0), at(0), &typed("%1", true));
    assert_eq!(
        book.judge("imessage", at(3), at(3), &typed("%2", true))
            .wrong,
        None
    );
    let mut book = PhoneBook::default();
    book.judge(
        "imessage",
        at(0),
        at(0),
        &Routing::Typed {
            pane_id: "%1".into(),
            resolved_by: ResolvedBy::TagSubstring,
            tagged: true,
            typed_at: at(0),
        },
    );
    assert_eq!(
        book.judge("imessage", at(3), at(3), &typed("%2", true))
            .wrong,
        None
    );
}

#[test]
fn a_tagged_reply_after_an_ai_model_or_my_agent_route_adds_one_wrong() {
    for chose in [ResolvedBy::AiModel, ResolvedBy::MyAgent] {
        let mut book = PhoneBook::default();
        book.judge(
            "imessage",
            at(0),
            at(0),
            &Routing::Typed {
                pane_id: "%1".into(),
                resolved_by: chose,
                tagged: false,
                typed_at: at(0),
            },
        );
        assert_eq!(
            book.judge("imessage", at(2), at(2), &typed("%2", true))
                .wrong,
            Some(("imessage".into(), chose))
        );
    }
}

#[test]
fn a_reply_another_mac_owns_or_while_paused_is_not_counted() {
    let mut book = PhoneBook::default();
    assert_eq!(
        book.judge(
            "imessage",
            at(0),
            at(0),
            &Routing::NotOwned { tagged: false }
        )
        .outcome,
        None
    );
    assert_eq!(
        book.judge("imessage", at(0), at(0), &Routing::Paused)
            .outcome,
        None
    );
}

fn harold_chose(pane: &str) -> Routing {
    Routing::Typed {
        pane_id: pane.into(),
        resolved_by: ResolvedBy::LatestNotifier,
        tagged: false,
        typed_at: at(0),
    }
}

#[test]
fn a_tag_after_a_reply_typed_late_adds_no_wrong() {
    // Only an on-time typed request can be counted wrong: typed minus wrong is the
    // right-agent count.
    let mut book = PhoneBook::default();
    let late = book.judge("imessage", at(0), at(11), &harold_chose("%1"));
    assert_eq!(late.outcome, Some("unavailable"));
    assert_eq!(
        book.judge("imessage", at(5), at(5), &typed("%2", true))
            .wrong,
        None
    );
}

#[test]
fn a_tag_after_a_reply_with_no_pane_adds_no_wrong() {
    // The reply after the typed one is the no-pane reply, not the tag.
    let mut book = PhoneBook::default();
    book.judge("imessage", at(0), at(0), &harold_chose("%1"));
    book.judge("imessage", at(1), at(1), &Routing::NoPane);
    assert_eq!(
        book.judge("imessage", at(2), at(2), &typed("%2", true))
            .wrong,
        None
    );
}

#[test]
fn a_reply_another_mac_owns_leaves_the_last_typed_reply_to_be_judged() {
    let mut book = PhoneBook::default();
    book.judge("imessage", at(0), at(0), &harold_chose("%1"));
    book.judge(
        "imessage",
        at(1),
        at(1),
        &Routing::NotOwned { tagged: false },
    );
    assert_eq!(
        book.judge("imessage", at(2), at(2), &typed("%2", true))
            .wrong,
        Some(("imessage".into(), ResolvedBy::LatestNotifier))
    );
}

const OTHER_MAC_TAGGED: Routing = Routing::NotOwned { tagged: true };

#[test]
fn a_reply_tagged_for_another_mac_within_ten_minutes_adds_one_wrong() {
    let mut book = PhoneBook::default();
    book.judge("imessage", at(0), at(0), &harold_chose("%1"));
    let v = book.judge("imessage", at(7), at(7), &OTHER_MAC_TAGGED);
    assert_eq!(
        v,
        PhoneVerdict {
            outcome: None,
            latency: None,
            wrong: Some(("imessage".into(), ResolvedBy::LatestNotifier)),
        }
    );
}

#[test]
fn a_reply_tagged_for_another_mac_after_eleven_minutes_or_a_user_tag_adds_no_wrong() {
    let mut book = PhoneBook::default();
    book.judge("imessage", at(0), at(0), &harold_chose("%1"));
    assert_eq!(
        book.judge("imessage", at(11), at(11), &OTHER_MAC_TAGGED)
            .wrong,
        None
    );
    let mut book = PhoneBook::default();
    book.judge("imessage", at(0), at(0), &typed("%1", true));
    assert_eq!(
        book.judge("imessage", at(3), at(3), &OTHER_MAC_TAGGED)
            .wrong,
        None
    );
}

#[test]
fn a_reply_tagged_for_another_mac_after_a_late_reply_adds_no_wrong() {
    let mut book = PhoneBook::default();
    book.judge("imessage", at(0), at(11), &harold_chose("%1"));
    assert_eq!(
        book.judge("imessage", at(5), at(5), &OTHER_MAC_TAGGED)
            .wrong,
        None
    );
}

#[test]
fn an_untagged_reply_another_mac_owns_adds_no_wrong() {
    let mut book = PhoneBook::default();
    book.judge("imessage", at(0), at(0), &harold_chose("%1"));
    assert_eq!(
        book.judge(
            "imessage",
            at(2),
            at(2),
            &Routing::NotOwned { tagged: false }
        )
        .wrong,
        None
    );
}

#[test]
fn two_replies_tagged_for_another_mac_count_the_earlier_request_wrong_once() {
    let mut book = PhoneBook::default();
    book.judge("imessage", at(0), at(0), &harold_chose("%1"));
    assert_eq!(
        book.judge("imessage", at(1), at(1), &OTHER_MAC_TAGGED)
            .wrong,
        Some(("imessage".into(), ResolvedBy::LatestNotifier))
    );
    assert_eq!(
        book.judge("imessage", at(2), at(2), &OTHER_MAC_TAGGED)
            .wrong,
        None,
        "a request is counted wrong at most once"
    );
    assert_eq!(
        book.last_typed.as_ref().map(|last| last.pane_id.as_str()),
        Some("%1"),
        "a reply another Mac owns does not replace the last typed reply"
    );
}

#[test]
fn a_local_tag_after_a_tag_for_another_mac_does_not_count_the_request_wrong_again() {
    let mut book = PhoneBook::default();
    book.judge("imessage", at(0), at(0), &harold_chose("%1"));
    assert_eq!(
        book.judge("imessage", at(1), at(1), &OTHER_MAC_TAGGED)
            .wrong,
        Some(("imessage".into(), ResolvedBy::LatestNotifier))
    );
    let local = book.judge("imessage", at(2), at(2), &typed("%2", true));
    assert_eq!(local.wrong, None, "a request is counted wrong at most once");
    assert_eq!(local.outcome, Some("typed"));
}

#[test]
fn an_untagged_reply_to_another_pane_adds_no_wrong() {
    let mut book = PhoneBook::default();
    book.judge("imessage", at(0), at(0), &harold_chose("%1"));
    assert_eq!(
        book.judge("imessage", at(2), at(2), &harold_chose("%2"))
            .wrong,
        None
    );
}

// ---------------------------------------------------------------------------
// Turn starts
// ---------------------------------------------------------------------------

#[test]
fn the_turn_starts_at_the_first_known_source() {
    let reached = at(30);
    let s = turn_start(
        Some("2026-10-14T08:05:00Z"),
        Some((at(3), StartedBy::Phone)),
        Some(at(4)),
        reached,
    );
    assert_eq!((s.at, s.started_by), (Some(at(5)), StartedBy::Phone));
    let s = turn_start(
        None,
        Some((at(3), StartedBy::TimedPrompt)),
        Some(at(4)),
        reached,
    );
    assert_eq!((s.at, s.started_by), (Some(at(3)), StartedBy::TimedPrompt));
    let s = turn_start(None, None, Some(at(4)), reached);
    assert_eq!((s.at, s.started_by), (Some(at(4)), StartedBy::Desk));
    let s = turn_start(None, None, None, reached);
    assert_eq!((s.at, s.started_by), (None, StartedBy::Unknown));
}

#[test]
fn voice_started_by_is_voice() {
    assert_eq!(StartedBy::Voice.as_str(), "voice");
    let s = turn_start(None, Some((at(3), StartedBy::Voice)), Some(at(4)), at(30));
    assert_eq!((s.at, s.started_by), (Some(at(3)), StartedBy::Voice));
}

#[test]
fn a_typing_time_after_the_hook_is_ignored_with_what_it_started() {
    let s = turn_start(None, Some((at(31), StartedBy::Phone)), Some(at(4)), at(30));
    assert_eq!((s.at, s.started_by), (Some(at(4)), StartedBy::Desk));
    let s = turn_start(None, Some((at(31), StartedBy::Phone)), None, at(30));
    assert_eq!((s.at, s.started_by), (None, StartedBy::Unknown));
}

#[test]
fn a_malformed_or_future_hook_time_counts_as_not_sent() {
    let reached = at(30);
    for bad in ["yesterday", "", "2026-10-14T09:00:00Z"] {
        let s = turn_start(Some(bad), None, Some(at(4)), reached);
        assert_eq!(s.at, Some(at(4)), "{bad}");
    }
}

// ---------------------------------------------------------------------------
// Instruments
// ---------------------------------------------------------------------------

/// The unit and histogram bounds of `name`, from the export that carries `product`.
/// Counters have no bounds. `None` when no such export exists.
fn exported_metric(name: &str, product: &str) -> Option<(String, Option<Vec<f64>>)> {
    use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
    fn carries<'a>(mut attributes: impl Iterator<Item = &'a KeyValue>, product: &str) -> bool {
        attributes.any(|kv| kv.key.as_str() == "pod.product" && kv.value.as_str() == product)
    }
    for resource in test_support::metrics() {
        for scope in resource.scope_metrics() {
            for metric in scope.metrics().filter(|m| m.name() == name) {
                let found = match metric.data() {
                    AggregatedMetrics::U64(MetricData::Sum(sum)) => sum
                        .data_points()
                        .any(|p| carries(p.attributes(), product))
                        .then_some(None),
                    AggregatedMetrics::F64(MetricData::Histogram(histogram)) => histogram
                        .data_points()
                        .find(|p| carries(p.attributes(), product))
                        .map(|p| Some(p.bounds().collect())),
                    _ => None,
                };
                if let Some(bounds) = found {
                    return Some((metric.unit().to_string(), bounds));
                }
            }
        }
    }
    None
}

#[test]
fn instruments_have_the_spec_names_units_and_buckets() {
    test_support::install();
    let product = format!("p-{}", uuid::Uuid::new_v4());
    let ours = [KeyValue::new("pod.product", product.clone())];
    let i = instruments();
    i.phone_requests.add(1, &ours);
    i.phone_requests_wrong.add(1, &ours);
    i.phone_request_latency.record(3.0, &ours);
    i.turns.add(1, &ours);
    i.turn_duration.record(42.0, &ours);
    i.pod_merges.add(1, &ours);
    i.pod_merged_lines.add(12, &ours);
    i.pod_spec_lead_time.record(
        700.0,
        &[
            KeyValue::new("pod.product", product.clone()),
            KeyValue::new("phase", "build"),
            KeyValue::new("spec.revisions", 0i64),
        ],
    );
    let counter = |unit: &str| Some((unit.to_string(), None));
    let histogram = |bounds: &[f64]| Some(("s".to_string(), Some(bounds.to_vec())));
    let expected = [
        ("harold.phone_requests", counter("{request}")),
        ("harold.phone_requests.wrong", counter("{request}")),
        (
            "harold.phone_request.latency",
            histogram(&[1.0, 2.0, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0]),
        ),
        ("harold.turns", counter("{turn}")),
        (
            "harold.turn.duration",
            histogram(&[10.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1800.0, 3600.0]),
        ),
        ("pod.merges", counter("{merge}")),
        ("pod.merged_lines", counter("{line}")),
        (
            "pod.spec.lead_time",
            histogram(&[
                600.0, 1800.0, 3600.0, 7200.0, 14400.0, 43200.0, 86400.0, 172800.0,
            ]),
        ),
    ];
    for (name, want) in expected {
        assert_eq!(exported_metric(name, &product), want, "{name}");
    }
}

// ---------------------------------------------------------------------------
// Fixed log messages
// ---------------------------------------------------------------------------

#[test]
fn every_log_message_is_fixed_text() {
    const ALLOWED: &[(&str, &str)] = &[
        ("harold/src/outbound/mod.rs", "{reason}"),
        ("harold/src/projector.rs", "{reason}"),
        (
            "harold/src/settings.rs",
            "HOME env var not set; cannot expand tilde in path: {path}",
        ),
        (
            "harold/src/telemetry.rs",
            "telemetry export failing (collector unreachable at {endpoint})",
        ),
    ];
    let offenders = scan_log_macros(
        Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap(),
        ALLOWED,
    );
    assert!(
        offenders.is_empty(),
        "log messages must be fixed text; put values in fields: {offenders:#?}"
    );
}

#[test]
fn the_scan_finds_interpolating_calls_and_skips_the_rest() {
    let source = r#"
        info!("fixed");
        tracing::warn!(pane_id = %pane, "sent to {pane}");
        error!(
            error = %error,
            "nested (parens) and {{escaped}} braces"
        );
        debug!("{}", x);
        otel_info!("{not a log macro}");
        let s = format!("{ignored}");
        // info!("{in a comment}");
        trace!(r"raw {value}", c = ')');
    "#;
    assert_eq!(
        interpolating_literals(source),
        vec![
            "sent to {pane}".to_string(),
            "{}".to_string(),
            "raw {value}".to_string()
        ]
    );
}

/// Each `(file, literal)` of a log macro call in `harold/src` whose string literal
/// interpolates a value and is not in `allowed`. Test files and the test fakes are skipped.
fn scan_log_macros(root: &Path, allowed: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut files = Vec::new();
    collect_rust_files(&root.join("harold/src"), &mut files);
    files.sort();
    let mut offenders = Vec::new();
    for file in files {
        let name = file.file_name().unwrap().to_string_lossy();
        if name.ends_with("_tests.rs") || name == "fake.rs" {
            continue;
        }
        let relative = file
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let source = std::fs::read_to_string(&file).unwrap();
        for literal in interpolating_literals(&source) {
            if !allowed.contains(&(relative.as_str(), literal.as_str())) {
                offenders.push((relative.clone(), literal));
            }
        }
    }
    offenders
}

fn collect_rust_files(dir: &Path, files: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect_rust_files(&path, files);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path);
        }
    }
}

const LOG_MACROS: [&str; 5] = ["info!(", "warn!(", "error!(", "debug!(", "trace!("];

/// The string literals with a `{...}` placeholder inside each log macro call of `source`.
fn interpolating_literals(source: &str) -> Vec<String> {
    let bytes = source.as_bytes();
    let mut found = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        if source[at..].starts_with("//") {
            at += source[at..].find('\n').unwrap_or(source.len() - at);
            continue;
        }
        if bytes[at] == b'"' || source[at..].starts_with("r\"") || source[at..].starts_with("r#\"")
        {
            at = read_string(source, at).0;
            continue;
        }
        if bytes[at] == b'\'' {
            at = skip_char_literal(source, at);
            continue;
        }
        let preceded_by_word =
            at > 0 && (bytes[at - 1].is_ascii_alphanumeric() || bytes[at - 1] == b'_');
        let Some(name) = LOG_MACROS
            .iter()
            .find(|name| source[at..].starts_with(**name))
        else {
            at += 1;
            continue;
        };
        if preceded_by_word {
            at += 1;
            continue;
        }
        let (end, literals) = read_call(source, at + name.len());
        found.extend(
            literals
                .into_iter()
                .filter(|literal| has_placeholder(literal)),
        );
        at = end;
    }
    found
}

/// Reads a macro call's arguments from just after its `(` to its matching `)`. Returns
/// the offset after the `)` and the string literals inside.
fn read_call(source: &str, start: usize) -> (usize, Vec<String>) {
    let bytes = source.as_bytes();
    let mut depth = 1;
    let mut at = start;
    let mut literals = Vec::new();
    while at < bytes.len() && depth > 0 {
        if source[at..].starts_with("//") {
            at += source[at..].find('\n').unwrap_or(source.len() - at);
            continue;
        }
        if bytes[at] == b'"' || source[at..].starts_with("r\"") || source[at..].starts_with("r#\"")
        {
            let (end, literal) = read_string(source, at);
            literals.push(literal);
            at = end;
            continue;
        }
        if bytes[at] == b'\'' {
            at = skip_char_literal(source, at);
            continue;
        }
        match bytes[at] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            _ => {}
        }
        at += 1;
    }
    (at, literals)
}

/// Reads a string literal (plain or raw) starting at `start`. Returns the offset after it
/// and its text, with escapes left as written.
fn read_string(source: &str, start: usize) -> (usize, String) {
    let bytes = source.as_bytes();
    if bytes[start] == b'r' {
        let hashes = source[start + 1..]
            .bytes()
            .take_while(|byte| *byte == b'#')
            .count();
        let open = start + 1 + hashes;
        if bytes.get(open) != Some(&b'"') {
            return (start + 1, String::new());
        }
        let close = format!("\"{}", "#".repeat(hashes));
        let end = source[open + 1..]
            .find(&close)
            .map_or(source.len(), |offset| open + 1 + offset);
        return (
            (end + close.len()).min(source.len()),
            source[open + 1..end].to_string(),
        );
    }
    let mut at = start + 1;
    while at < bytes.len() && bytes[at] != b'"' {
        at += if bytes[at] == b'\\' { 2 } else { 1 };
    }
    let end = at.min(source.len());
    (
        (end + 1).min(source.len()),
        source[start + 1..end].to_string(),
    )
}

/// Skips a character literal such as `'('`, `'"'` or `'\''`; a lifetime is skipped by
/// its quote alone.
fn skip_char_literal(source: &str, start: usize) -> usize {
    let rest = &source[start + 1..];
    let mut chars = rest.char_indices();
    match chars.next() {
        Some((_, '\\')) => {
            let close = rest[1..].find('\'').map_or(rest.len(), |offset| offset + 1);
            start + 1 + close + 1
        }
        Some((_, c)) if rest[c.len_utf8()..].starts_with('\'') => start + 1 + c.len_utf8() + 1,
        _ => start + 1,
    }
}

/// Whether `literal` has a format placeholder: a `{` that is not `{{`, closed by `}`.
fn has_placeholder(literal: &str) -> bool {
    let mut chars = literal.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '{' {
            continue;
        }
        if chars.peek() == Some(&'{') {
            chars.next();
            continue;
        }
        if chars.any(|c| c == '}') {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Open phone requests and prompts
// ---------------------------------------------------------------------------

/// A channel name no other test uses, so this test's metrics are its own.
fn unique_channel() -> &'static str {
    Box::leak(format!("test-{}", uuid::Uuid::new_v4().simple()).into_boxed_str())
}

fn phone_requests_on(channel: &str) -> Vec<SpanData> {
    test_support::spans()
        .into_iter()
        .filter(|span| {
            span.name == "harold.phone_request"
                && test_support::span_attr(span, "channel").as_deref() == Some(channel)
        })
        .collect()
}

#[test]
fn a_tag_for_another_pane_after_harolds_choice_counts_the_earlier_request_wrong() {
    let channel = unique_channel();
    let mut book = PhoneBook::default();
    let now = OffsetDateTime::now_utc();
    let first = open_phone_request(channel, now - time::Duration::seconds(60));
    finish_phone_request_in(
        &mut book,
        &first,
        &typed("%wrong-a", false),
        now - time::Duration::seconds(50),
    );
    let second = open_phone_request(channel, now - time::Duration::seconds(10));
    finish_phone_request_in(&mut book, &second, &typed("%wrong-b", true), now);

    assert_eq!(
        test_support::counter_value(
            "harold.phone_requests.wrong",
            &[("channel", channel), ("resolved_by", "latest_notifier")],
        ),
        Some(1)
    );
    assert_eq!(
        test_support::counter_value(
            "harold.phone_requests",
            &[("channel", channel), ("outcome", "typed")],
        ),
        Some(2)
    );
    let requests = phone_requests_on(channel);
    assert_eq!(requests.len(), 2);
    assert!(
        requests
            .iter()
            .all(|span| test_support::span_attr(span, "outcome").as_deref() == Some("typed"))
    );
}

#[test]
fn a_tag_for_another_mac_after_harolds_choice_counts_the_earlier_request_wrong() {
    let channel = unique_channel();
    let mut book = PhoneBook::default();
    let now = OffsetDateTime::now_utc();
    let first = open_phone_request(channel, now - time::Duration::seconds(60));
    finish_phone_request_in(
        &mut book,
        &first,
        &typed("%wrong-mac", false),
        now - time::Duration::seconds(50),
    );
    let second = open_phone_request(channel, now - time::Duration::seconds(10));
    finish_phone_request_in(&mut book, &second, &Routing::NotOwned { tagged: true }, now);

    assert_eq!(
        test_support::counter_value(
            "harold.phone_requests.wrong",
            &[("channel", channel), ("resolved_by", "latest_notifier")],
        ),
        Some(1)
    );
    // The reply another Mac owns is counted there, not here.
    assert_eq!(
        test_support::counter_value(
            "harold.phone_requests",
            &[("channel", channel), ("outcome", "typed")],
        ),
        Some(1)
    );
    let outcomes: Vec<_> = phone_requests_on(channel)
        .iter()
        .filter_map(|span| test_support::span_attr(span, "outcome"))
        .collect();
    assert_eq!(outcomes.len(), 2);
    assert!(outcomes.iter().any(|outcome| outcome == "not_owned"));
}

#[test]
fn a_typed_request_keeps_its_prompt_for_the_next_turn_end_on_its_pane() {
    let pane = unique_pane();
    let now = OffsetDateTime::now_utc();
    let reply = open_phone_request(unique_channel(), now - time::Duration::seconds(5));
    let routing = Routing::Typed {
        pane_id: pane.clone(),
        resolved_by: ResolvedBy::MyAgent,
        tagged: false,
        typed_at: now,
    };

    // Handled a second after it was typed: the request ends when it was typed.
    finish_phone_request_in(
        &mut PhoneBook::default(),
        &reply,
        &routing,
        now + time::Duration::seconds(1),
    );

    let (typed_at, started_by, cx) = take_prompt(&pane, now).expect("the prompt is kept");
    assert_eq!(typed_at, now);
    assert_eq!(started_by, StartedBy::Phone);
    assert_eq!(
        cx.expect("the request's context")
            .span()
            .span_context()
            .trace_id(),
        reply.cx.span().span_context().trace_id()
    );
    assert!(take_prompt(&pane, now).is_none(), "a prompt is taken once");
}

#[test]
fn a_reply_with_no_pane_keeps_no_prompt() {
    let now = OffsetDateTime::now_utc();
    let channel = unique_channel();
    let reply = open_phone_request(channel, now);
    finish_phone_request_in(&mut PhoneBook::default(), &reply, &Routing::NoPane, now);

    let requests = phone_requests_on(channel);
    assert_eq!(requests.len(), 1);
    assert_eq!(
        test_support::span_attr(&requests[0], "outcome").as_deref(),
        Some("unavailable")
    );
    assert_eq!(test_support::span_attr(&requests[0], "pane_id"), None);
}

#[test]
fn a_prompt_older_than_an_hour_is_not_taken() {
    let pane = unique_pane();
    let now = OffsetDateTime::now_utc();

    prompt_typed(
        &pane,
        StartedBy::TimedPrompt,
        None,
        now - time::Duration::seconds(3601),
    );
    assert!(take_prompt(&pane, now).is_none());

    prompt_typed(
        &pane,
        StartedBy::TimedPrompt,
        None,
        now - time::Duration::seconds(3599),
    );
    let (_, started_by, cx) = take_prompt(&pane, now).expect("kept for an hour");
    assert_eq!(started_by, StartedBy::TimedPrompt);
    assert!(cx.is_none());
}

#[tokio::test]
async fn a_stored_reply_is_kept_open_under_the_trace_the_store_sees() {
    let channel = unique_channel();
    let at = OffsetDateTime::now_utc() - time::Duration::seconds(3);
    let seen = Mutex::new(None);

    let stored = store_phone_reply(channel, at, async {
        let trace = tracing::Span::current()
            .context()
            .span()
            .span_context()
            .trace_id();
        *seen.lock().unwrap() = Some(trace.to_string());
        true
    })
    .await;

    assert!(stored);
    let trace_id = seen.lock().unwrap().clone().unwrap();
    let reply = open_reply_for(&trace_id).expect("kept open");
    assert_eq!(reply.channel, channel);
    assert_eq!(reply.reply_at, at);
    let stored_spans: Vec<_> = test_support::spans()
        .into_iter()
        .filter(|span| {
            span.name == "harold.reply_stored"
                && span.span_context.trace_id().to_string() == trace_id
        })
        .collect();
    assert_eq!(stored_spans.len(), 1);
    assert!(
        phone_requests_on(channel).is_empty(),
        "the request ends when it is finished, not when it is stored"
    );
}

#[tokio::test]
async fn a_reply_that_is_not_stored_ends_its_request_as_ignored() {
    let channel = unique_channel();

    let stored = store_phone_reply(channel, OffsetDateTime::now_utc(), async { false }).await;

    assert!(!stored);
    let requests = phone_requests_on(channel);
    assert_eq!(requests.len(), 1);
    assert_eq!(
        test_support::span_attr(&requests[0], "outcome").as_deref(),
        Some("ignored")
    );
    assert_eq!(
        test_support::counter_value("harold.phone_requests", &[("channel", channel)]),
        None
    );
}

#[test]
fn a_reply_with_no_trace_is_never_kept_and_finishing_it_leaves_the_others() {
    // With telemetry off every request has the same all-zero trace id.
    let untraced = OpenReply {
        cx: Context::new(),
        channel: "imessage",
        reply_at: OffsetDateTime::now_utc(),
    };
    let zero_trace = untraced.trace_id();
    let key = format!("event:{}", uuid::Uuid::new_v4());
    let waiting = open_phone_request(unique_channel(), OffsetDateTime::now_utc());
    keep_open_reply_as(key.clone(), waiting.clone());

    keep_open_reply(untraced.clone());
    keep_open_reply_as(format!("event:{}", uuid::Uuid::new_v4()), untraced.clone());
    assert!(open_reply_for(&zero_trace).is_none());

    finish_phone_request_in(
        &mut PhoneBook::default(),
        &untraced,
        &Routing::NoPane,
        OffsetDateTime::now_utc(),
    );
    assert!(
        open_reply_for(&key).is_some(),
        "a request waiting for its retry is still open"
    );
}

#[tokio::test]
async fn a_reply_is_kept_open_before_it_is_stored_and_dropped_if_it_is_not() {
    let channel = unique_channel();
    let seen = Mutex::new(None);

    let stored = store_phone_reply(channel, OffsetDateTime::now_utc(), async {
        let trace = tracing::Span::current()
            .context()
            .span()
            .span_context()
            .trace_id()
            .to_string();
        let open = open_reply_for(&trace).map(|reply| reply.channel);
        *seen.lock().unwrap() = Some((trace, open));
        false
    })
    .await;

    assert!(!stored);
    let (trace, open_while_storing) = seen.lock().unwrap().clone().unwrap();
    assert_eq!(
        open_while_storing,
        Some(channel),
        "open before the store commits"
    );
    assert!(open_reply_for(&trace).is_none(), "dropped once not stored");
}

// ---------------------------------------------------------------------------
// HyperDX definition files
// ---------------------------------------------------------------------------

/// The spec's Metrics table.
const METRICS: &[&str] = &[
    "pod.slots.busy",
    "pod.slots.free",
    "pod.merges",
    "pod.merged_lines",
    "harold.phone_requests",
    "harold.phone_requests.wrong",
    "harold.phone_request.latency",
    "harold.turn_end_updates",
    "harold.turns",
    "harold.turn.duration",
    "pod.specs",
    "pod.spec.words",
    "pod.spec.lead_time",
    "watch.runs",
    "watch.cost_usd",
];

/// The Metrics table's attributes, plus the two resource attributes every export carries.
const ATTRIBUTES: &[&str] = &[
    "pod.product",
    "spec.id",
    "change",
    "channel",
    "outcome",
    "reason",
    "result",
    "resolved_by",
    "session.id",
    "pane_id",
    "started_by",
    "state",
    "phase",
    "spec.revisions",
    "harold.machine",
    "service.name",
    "folder",
    "cost_usd",
];

/// The spec's span table. The trends panel "One turn, end to end" lists `harold.turn`.
const SPANS: &[&str] = &[
    "harold.phone_request",
    "harold.route",
    "harold.turn",
    "harold.turn_complete",
    "harold.delivery",
    "harold.voice_note",
    "harold.local_model",
    "harold.watch.run",
    "harold.watch.model",
];

/// The spec's cards table: group, card, the metrics or spans its tile reads, and how it
/// reads them. For a builder tile, the first phrase is its `aggFn` and level; for a SQL
/// tile, every phrase is in its SQL.
const CARDS: [(&str, &str, &[&str], &[&str]); 18] = [
    ("Pods", "Free slots, average", &["pod.slots.free"], &["avg"]),
    (
        "Pods",
        "Busy slots, p95",
        &["pod.slots.busy"],
        &["quantile 0.95"],
    ),
    (
        "Pods",
        "Merges to main, last 24 h",
        &["pod.merges"],
        &["SELECT sum(rise)"],
    ),
    (
        "Pods",
        "Lines merged",
        &["pod.merged_lines"],
        &["SELECT sum(rise)"],
    ),
    (
        "Pods",
        "Lines merged, last hour",
        &["pod.merged_lines"],
        &["SELECT sum(rise)"],
    ),
    (
        "Phone requests",
        "Requests",
        &["harold.phone_requests"],
        &["SELECT sum(rise)"],
    ),
    (
        "Phone requests",
        "Availability",
        &["harold.phone_requests"],
        &[
            "sumIf(rise, MetricName = 'harold.phone_requests' AND Attributes['outcome'] = 'typed') / sumIf(rise, MetricName = 'harold.phone_requests')",
        ],
    ),
    (
        "Phone requests",
        "Right agent",
        &["harold.phone_requests", "harold.phone_requests.wrong"],
        &[
            "(sumIf(rise, MetricName = 'harold.phone_requests' AND Attributes['outcome'] = 'typed') - sumIf(rise, MetricName = 'harold.phone_requests.wrong')) / sumIf(rise, MetricName = 'harold.phone_requests')",
        ],
    ),
    (
        "Phone requests",
        "Latency, p95",
        &["harold.phone_request"],
        &[
            "quantile(0.95)(Duration / 1e9)",
            "SpanAttributes['pane_id'] != ''",
        ],
    ),
    ("Turns", "Turns", &["harold.turns"], &["SELECT sum(rise)"]),
    (
        "Turns",
        "Turns per session, median",
        &["harold.turn"],
        &[
            "SELECT median(turns)",
            "count() AS turns",
            "GROUP BY session",
        ],
    ),
    (
        "Specs",
        "Waiting for approval",
        &["pod.specs"],
        &[
            "SELECT sum(specs)",
            "argMax(Value, TimeUnix)",
            "Attributes['state'] = 'waiting_for_approval'",
        ],
    ),
    (
        "Specs",
        "Merged",
        &["pod.spec.words"],
        &[
            "SELECT count()",
            "min(TimeUnix) AS first_merged",
            "Attributes['state'] = 'merged'",
        ],
    ),
    (
        "Specs",
        "Spec size, median words",
        &["pod.spec.words"],
        &[
            "SELECT median(words)",
            "min(TimeUnix) AS first_ready",
            "Attributes['state'] != 'drafted'",
        ],
    ),
    (
        "Specs",
        "Waiting for you, p50",
        &["pod.spec.lead_time"],
        &[
            "quantileExactWeighted(0.5)",
            "Attributes['phase'] = 'waiting_for_user'",
        ],
    ),
    (
        "Specs",
        "Build, p50",
        &["pod.spec.lead_time"],
        &[
            "quantileExactWeighted(0.5)",
            "Attributes['phase'] = 'build'",
        ],
    ),
    (
        "Turn-end updates",
        "Turn-end update, model p95",
        &["harold.local_model"],
        &["quantile(0.95)(Duration / 1e9)", "harold.local_model"],
    ),
    (
        "Turn-end updates",
        FULL_SENTENCES_CARD,
        &["harold.turn_end_updates"],
        &["SELECT if(sumIf(rise"],
    ),
];

/// The card read over the last 7 days (168 hours); its earlier tile is "previous 7 days".
const FULL_SENTENCES_CARD: &str = "Turn-end updates in full sentences";

/// The card read over the last hour; every other card reads the last 24 hours (choice 10).
const LAST_HOUR_CARD: &str = "Lines merged, last hour";

/// The counter rule in the SQL of every tile that counts a running total: each series
/// rises by its value minus its previous point, and a first point or a drop rises by its
/// whole value.
const COUNTER_RULE: [&str; 3] = [
    "lagInFrame(Value, 1, toFloat64(0)) OVER (PARTITION BY cityHash64(MetricName, ResourceAttributes, Attributes) ORDER BY TimeUnix ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) AS before",
    "if(Value >= before, Value - before, Value) AS rise",
    "FROM $__sourceTable(sum)",
];

/// The same rule over a histogram's count and sum, judged by the count.
const HISTOGRAM_RULE: [&str; 4] = [
    "lagInFrame(toFloat64(Count), 1, toFloat64(0)) OVER (PARTITION BY cityHash64(MetricName, ResourceAttributes, Attributes) ORDER BY TimeUnix ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) AS count_before",
    "if(count < count_before, count, count - count_before) AS count_rise",
    "if(count < count_before, total, total - total_before) AS total_rise",
    "FROM $__sourceTable(histogram)",
];

/// The spec's trends table, in order.
const TREND_PANELS: [&str; 10] = [
    "Slots",
    "Merges to main",
    "Phone requests reaching the right agent",
    "Phone request latency",
    "One turn, end to end",
    "Turns per session",
    "Specs by state",
    "Spec size",
    "Start to completion",
    "Watch runs: cost and duration per run",
];

fn definitions() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs/references/telemetry/hyperdx")
}

fn definition(file: &str) -> serde_json::Value {
    let path = definitions().join(file);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path:?}: {error}"));
    serde_json::from_str(&text).unwrap_or_else(|error| panic!("{path:?}: {error}"))
}

/// Every string in `value`, object keys included.
fn strings_in(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(text) => out.push(text.clone()),
        serde_json::Value::Array(items) => items.iter().for_each(|item| strings_in(item, out)),
        serde_json::Value::Object(map) => {
            for (key, item) in map {
                out.push(key.clone());
                strings_in(item, out);
            }
        }
        _ => {}
    }
}

/// The dotted names starting `pod.` or `harold.` in `text`, such as `pod.slots.free` in
/// `MetricName = 'pod.slots.free'`.
fn dotted_names(text: &str) -> Vec<String> {
    let is_name_char = |c: char| c.is_ascii_lowercase() || c == '_' || c == '.';
    let mut names = Vec::new();
    for (start, _) in text.char_indices() {
        let rest = &text[start..];
        if !(rest.starts_with("pod.") || rest.starts_with("harold.") || rest.starts_with("watch."))
        {
            continue;
        }
        let before = text[..start].chars().next_back();
        if before.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-') {
            continue;
        }
        let end = rest.find(|c: char| !is_name_char(c)).unwrap_or(rest.len());
        names.push(rest[..end].trim_end_matches('.').to_owned());
    }
    names
}

/// The keys read from an attribute map, such as `outcome` in `Attributes['outcome']`.
fn attribute_keys(text: &str) -> Vec<String> {
    text.split("Attributes['")
        .skip(1)
        .map(|rest| rest.split('\'').next().unwrap_or_default().to_owned())
        .collect()
}

fn tiles(dashboard: &serde_json::Value) -> &Vec<serde_json::Value> {
    dashboard["tiles"].as_array().expect("tiles")
}

fn tile_names(dashboard: &serde_json::Value) -> Vec<&str> {
    tiles(dashboard)
        .iter()
        .map(|tile| tile["name"].as_str().expect("tile name"))
        .collect()
}

/// The time conditions of a tile: each select item's `where`, or the raw SQL.
fn tile_queries(tile: &serde_json::Value) -> Vec<String> {
    let config = &tile["config"];
    if let Some(sql) = config["sqlTemplate"].as_str() {
        return vec![sql.to_owned()];
    }
    config["select"]
        .as_array()
        .expect("select")
        .iter()
        .map(|item| item["where"].as_str().unwrap_or_default().to_owned())
        .collect()
}

#[test]
fn every_definition_is_valid_json_naming_only_known_metrics_and_attributes() {
    let mut files: Vec<String> = std::fs::read_dir(definitions())
        .expect("the definitions folder")
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    files.sort();
    assert_eq!(
        files,
        [
            "alert-local-model-failing.json",
            "alert-phone-request-slow.json",
            "alert-phone-request-unavailable.json",
            "alert-slot-idle.json",
            "alert-watch-cost-high.json",
            "alert-wrong-agent.json",
            "dashboard.json",
            "trends.json",
            "webhook-local-model-failing.json",
            "webhook-phone-request-slow.json",
            "webhook-phone-request-unavailable.json",
            "webhook-slot-idle.json",
            "webhook-watch-cost-high.json",
            "webhook-wrong-agent.json",
        ]
    );

    for file in &files {
        let mut texts = Vec::new();
        strings_in(&definition(file), &mut texts);
        for text in &texts {
            assert!(
                !text.contains("{{note}}"),
                "{file} uses {{{{note}}}}, which HyperDX 2.40 renders empty in a webhook body"
            );
            for name in dotted_names(text) {
                assert!(
                    METRICS.contains(&name.as_str())
                        || ATTRIBUTES.contains(&name.as_str())
                        || SPANS.contains(&name.as_str()),
                    "{file} names {name:?}, which is not in the spec's Metrics or span tables"
                );
            }
            for key in attribute_keys(text) {
                assert!(
                    ATTRIBUTES.contains(&key.as_str()),
                    "{file} reads attribute {key:?}, which is not in the spec's Metrics table"
                );
            }
        }
    }
}

/// The metrics and spans `value` names.
fn metrics_read(value: &serde_json::Value) -> Vec<String> {
    let mut texts = Vec::new();
    strings_in(value, &mut texts);
    let mut names: Vec<String> = texts
        .iter()
        .flat_map(|text| dotted_names(text))
        .filter(|name| METRICS.contains(&name.as_str()) || SPANS.contains(&name.as_str()))
        .collect();
    names.sort();
    names.dedup();
    names
}

#[test]
fn the_cards_dashboard_shows_each_card_beside_its_earlier_number_and_nothing_else() {
    let dashboard = definition("dashboard.json");
    assert_eq!(dashboard["name"], "Harold");
    let mut keys: Vec<&str> = dashboard
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, ["containers", "filters", "name", "tags", "tiles"]);

    let names: Vec<String> = CARDS
        .iter()
        .flat_map(|(_, card, ..)| {
            let earlier = if *card == LAST_HOUR_CARD {
                format!("{card}, previous hour")
            } else if *card == FULL_SENTENCES_CARD {
                format!("{card}, previous 7 days")
            } else if *card == "Merges to main, last 24 h" {
                "Merges to main, the 24 h before".to_string()
            } else {
                format!("{card}, previous 24 h")
            };
            [card.to_string(), earlier]
        })
        .collect();
    assert_eq!(tile_names(&dashboard), names);

    let containers: BTreeMap<&str, &str> = dashboard["containers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|container| {
            (
                container["id"].as_str().unwrap(),
                container["title"].as_str().unwrap(),
            )
        })
        .collect();
    let mut groups: Vec<&str> = containers.values().copied().collect();
    groups.sort_unstable();
    assert_eq!(
        groups,
        [
            "Phone requests",
            "Pods",
            "Specs",
            "Turn-end updates",
            "Turns"
        ]
    );

    for (pair, (group, card, reads, how)) in tiles(&dashboard).chunks(2).zip(CARDS) {
        let (current, earlier) = (&pair[0], &pair[1]);
        let hours = match card {
            LAST_HOUR_CARD => 1,
            FULL_SENTENCES_CARD => 168,
            _ => 24,
        };
        let other_periods: &[&str] = if hours == 1 {
            &["INTERVAL 24 HOUR", "INTERVAL 48 HOUR", "INTERVAL 72 HOUR"]
        } else {
            &["INTERVAL 1 HOUR", "INTERVAL 2 HOUR", "INTERVAL 3 HOUR"]
        };
        for (tile, previous) in [(current, false), (earlier, true)] {
            let name = tile["name"].as_str().unwrap();
            assert_eq!(tile["config"]["displayType"], "number", "{name}");
            let container = tile["containerId"].as_str().unwrap_or_default();
            assert_eq!(containers.get(container), Some(&group), "{name}");
            assert_eq!(
                metrics_read(&tile["config"]),
                *reads,
                "{name} reads the wrong metric"
            );

            let config = &tile["config"];
            if let Some(sql) = config["sqlTemplate"].as_str() {
                for phrase in how {
                    assert!(sql.contains(phrase), "{name} must read {phrase}: {sql}");
                }
            } else {
                let select = config["select"].as_array().unwrap();
                assert_eq!(select.len(), 1, "{name}");
                let aggregation = match select[0]["level"].as_f64() {
                    Some(level) => format!("{} {level}", select[0]["aggFn"].as_str().unwrap()),
                    None => select[0]["aggFn"].as_str().unwrap().to_owned(),
                };
                assert_eq!(aggregation, how[0], "{name}");
            }

            for query in tile_queries(tile) {
                if previous {
                    let from = format!(">= now() - INTERVAL {} HOUR", 2 * hours);
                    let to = format!("< now() - INTERVAL {hours} HOUR");
                    assert!(query.contains(&from), "{name} must start {from}: {query}");
                    assert!(query.contains(&to), "{name} must end {to}: {query}");
                } else {
                    let from = format!(">= now() - INTERVAL {hours} HOUR");
                    assert!(query.contains(&from), "{name} must start {from}: {query}");
                    assert!(
                        !query.contains("< now() - INTERVAL"),
                        "{name} must end now: {query}"
                    );
                }
                for other in other_periods {
                    assert!(
                        !query.contains(other),
                        "{name} reads another period: {query}"
                    );
                }
            }
        }
        assert_eq!(
            earlier["y"], current["y"],
            "{card}: the earlier number sits beside it"
        );
        assert_eq!(
            earlier["x"].as_i64().unwrap(),
            current["x"].as_i64().unwrap() + current["w"].as_i64().unwrap(),
            "{card}: the earlier number sits beside it"
        );
    }

    let filters = dashboard["filters"].as_array().unwrap();
    assert_eq!(filters.len(), 1, "only the product filter");
    assert_eq!(filters[0]["name"], "Product (pod and spec cards only)");
    assert_eq!(filters[0]["expression"], "Attributes['pod.product']");
}

#[test]
fn the_product_filter_reaches_the_pod_and_spec_tiles_only() {
    let dashboard = definition("dashboard.json");
    let containers = BTreeMap::from([
        ("pods", true),
        ("specs", true),
        ("phone", false),
        ("turns", false),
        ("turn-end", false),
    ]);
    for tile in tiles(&dashboard) {
        let name = tile["name"].as_str().unwrap();
        let filtered = containers[tile["containerId"].as_str().unwrap()];
        let config = &tile["config"];
        match config["sqlTemplate"].as_str() {
            // A SQL tile takes the filter only through `$__filters`.
            Some(sql) => assert_eq!(sql.contains("$__filters"), filtered, "{name}"),
            // A builder tile on the metrics source always takes it.
            None => assert!(
                filtered,
                "{name} is a builder tile, which the filter reaches"
            ),
        }
    }
}

/// The counter rule the dashboards' SQL applies, in Rust: the rise of one series in a
/// window starting at `start`, from its points in time order from the lookback on.
fn rise_in_window(points: &[(i64, f64)], start: i64) -> f64 {
    let mut before = 0.0;
    let mut total = 0.0;
    for &(time, value) in points {
        // `if(Value >= before, Value - before, Value)`, with `before` 0 for a first point.
        let rise = if value >= before {
            value - before
        } else {
            value
        };
        if time >= start {
            total += rise;
        }
        before = value;
    }
    total
}

#[test]
fn the_counter_rule_counts_rises_new_series_and_resets_from_zero() {
    // A series that rose from 5 before the window to 7 in it.
    assert_eq!(rise_in_window(&[(0, 5.0), (10, 7.0)], 5), 2.0);
    // A series whose first point is in the window, such as a merge naming a new spec.id.
    assert_eq!(rise_in_window(&[(10, 1.0)], 5), 1.0);
    // A reset after a restart: never negative, and counted from 0.
    assert_eq!(rise_in_window(&[(0, 10.0), (10, 0.0), (20, 2.0)], 5), 2.0);
    assert_eq!(rise_in_window(&[(0, 10.0), (10, 3.0)], 5), 3.0);
    // Points before the window only set the baseline.
    assert_eq!(rise_in_window(&[(0, 3.0), (2, 4.0)], 5), 0.0);
    // A point written twice adds nothing the second time.
    assert_eq!(rise_in_window(&[(10, 4.0), (10, 4.0)], 5), 4.0);
}

#[test]
fn every_tile_that_counts_a_running_total_uses_the_counter_rule() {
    let mut counted = 0;
    for file in ["dashboard.json", "trends.json"] {
        let dashboard = definition(file);
        for tile in tiles(&dashboard) {
            let name = tile["name"].as_str().unwrap();
            let Some(sql) = tile["config"]["sqlTemplate"].as_str() else {
                continue;
            };
            let rule: &[&str] = if sql.contains("count_rise") {
                &HISTOGRAM_RULE
            } else if sql.contains("rise") {
                &COUNTER_RULE
            } else {
                continue;
            };
            counted += 1;
            for piece in rule {
                assert!(
                    sql.contains(piece),
                    "{file} {name} must have {piece}: {sql}"
                );
            }
        }
    }
    // Eight counter cards and two lead-time cards, each with its earlier number, and
    // three trends panels.
    assert_eq!(counted, 23);
}

#[test]
fn the_trends_dashboard_has_the_ten_panels() {
    let trends = definition("trends.json");
    assert_eq!(trends["name"], "Harold trends");
    assert_eq!(tile_names(&trends), TREND_PANELS);
}

#[test]
fn the_alert_files_carry_the_spec_thresholds() {
    const METRICS_SOURCE: &str = "@METRICS_SOURCE_ID@";
    const TRACES_SOURCE: &str = "@TRACES_SOURCE_ID@";
    // (alert, source, metric or span read, aggregation, threshold, comparison, minutes
    // the condition must hold)
    let expected = [
        (
            "phone-request-unavailable",
            METRICS_SOURCE,
            "harold.phone_requests",
            "sum",
            0.0,
            "above_exclusive",
            5,
        ),
        (
            "phone-request-slow",
            TRACES_SOURCE,
            "harold.phone_request",
            "quantile 0.95",
            60.0,
            "above_exclusive",
            60,
        ),
        (
            "wrong-agent",
            METRICS_SOURCE,
            "harold.phone_requests.wrong",
            "sum",
            0.0,
            "above_exclusive",
            5,
        ),
        (
            "slot-idle",
            METRICS_SOURCE,
            "pod.slots.free",
            "min",
            1.0,
            "above",
            120,
        ),
        (
            "local-model-failing",
            METRICS_SOURCE,
            "harold.turn_end_updates",
            "sum",
            3.0,
            "above_exclusive",
            60,
        ),
        (
            "watch-cost-high",
            METRICS_SOURCE,
            "watch.cost_usd",
            "sum",
            1.0,
            "above_exclusive",
            1440,
        ),
    ];
    assert_eq!(
        expected.map(|(name, ..)| name),
        crate::alerts::ALERT_NAMES,
        "one file per alert Harold accepts"
    );
    for (name, source, reads, aggregation, threshold, comparison, minutes) in expected {
        let alert = definition(&format!("alert-{name}.json"));
        assert_eq!(alert["name"], name);
        assert_eq!(alert["note"], name, "{name}");
        assert_eq!(alert["source"], "inline", "{name}");
        assert_eq!(alert["channel"]["type"], "webhook", "{name}");
        assert_eq!(
            alert["channel"]["webhookId"], "@WEBHOOK_ID@",
            "{name}: the README fills it with harold-alerts-{name}'s id"
        );
        let webhooks: Vec<String> = std::fs::read_dir(definitions())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .filter(|file| file == &format!("webhook-{name}.json"))
            .collect();
        assert_eq!(
            webhooks.len(),
            1,
            "{name}: exactly one webhook file of its name"
        );
        assert_eq!(alert["threshold"].as_f64(), Some(threshold), "{name}");
        assert_eq!(alert["thresholdType"], comparison, "{name}");
        let interval = match alert["interval"].as_str().unwrap() {
            "5m" => 5,
            "15m" => 15,
            "1h" => 60,
            "1d" => 1440,
            other => panic!("{name}: unexpected interval {other}"),
        };
        let windows = alert["numConsecutiveWindows"].as_i64().unwrap_or(1);
        assert_eq!(interval * windows, minutes, "{name}");
        let config = &alert["chartConfig"];
        assert_eq!(config["sourceId"], source, "{name}");
        assert_eq!(metrics_read(config), [reads], "{name}");
        let select = config["select"].as_array().unwrap();
        assert_eq!(select.len(), 1, "{name}");
        let read = match select[0]["level"].as_f64() {
            Some(level) => format!("{} {level}", select[0]["aggFn"].as_str().unwrap()),
            None => select[0]["aggFn"].as_str().unwrap().to_owned(),
        };
        assert_eq!(read, aggregation, "{name}");
    }

    let config = |name: &str| definition(&format!("alert-{name}.json"))["chartConfig"].clone();
    assert_eq!(
        config("phone-request-unavailable")["select"][0]["where"],
        "Attributes['outcome'] = 'unavailable'"
    );
    // The latency of a typed request: its span runs from the reply's own time to Enter.
    let slow = config("phone-request-slow");
    assert_eq!(slow["select"][0]["valueExpression"], "Duration / 1e9");
    assert_eq!(
        slow["select"][0]["where"],
        "ServiceName = 'harold' AND SpanName = 'harold.phone_request' AND SpanAttributes['pane_id'] != ''"
    );
    assert_eq!(
        config("local-model-failing")["select"][0]["where"],
        "Attributes['reason'] = 'model_failed'"
    );
    assert_eq!(config("wrong-agent")["select"][0]["where"], "");
    assert_eq!(config("slot-idle")["groupBy"], "Attributes['pod.product']");
}

#[test]
fn each_alert_has_its_own_webhook_posting_what_the_alert_address_accepts() {
    let state = "{{#if (eq status 'firing')}}firing{{else if (eq status 'resolved')}}cleared{{else}}{{status}}{{/if}}";
    // HyperDX's statuses, and the state the template renders for each.
    let rendered = [
        ("firing", "firing"),
        ("resolved", "cleared"),
        ("no_data", "no_data"),
        ("pending", "pending"),
        ("error", "error"),
    ];
    for name in crate::alerts::ALERT_NAMES {
        let webhook = definition(&format!("webhook-{name}.json"));
        assert_eq!(webhook["name"], format!("harold-alerts-{name}"));
        assert_eq!(webhook["service"], "generic", "{name}");
        assert_eq!(
            webhook["url"], "http://host.containers.internal:4320/alert",
            "{name}"
        );
        let body = webhook["body"].as_str().unwrap();
        // HyperDX 2.40 leaves `note` out of what a webhook body sees, so it renders empty.
        assert!(!body.contains("{{note}}"), "{name}: {body}");
        assert!(body.contains(state), "{name}: {body}");
        for (status, state_sent) in rendered {
            let post = body.replace(state, state_sent);
            let line = crate::alerts::alert_line(post.as_bytes());
            match status {
                "firing" | "resolved" => {
                    assert_eq!(line, Ok(format!("harold alert: {name} {state_sent}")));
                }
                _ => assert_eq!(line, Err(()), "{status} is refused and nothing is typed"),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Counting from 0
// ---------------------------------------------------------------------------

#[test]
fn every_phone_counter_set_the_alerts_read_is_exported_from_zero() {
    register_phone_counters();
    for channel in PHONE_CHANNELS {
        for resolved_by in ResolvedBy::ALL {
            let resolved_by = resolved_by.as_str();
            for outcome in ["typed", "unavailable"] {
                let set = [
                    ("channel", channel),
                    ("outcome", outcome),
                    ("resolved_by", resolved_by),
                ];
                assert!(
                    test_support::exact_counter_value("harold.phone_requests", &set).is_some(),
                    "{set:?}"
                );
            }
            let set = [("channel", channel), ("resolved_by", resolved_by)];
            assert!(
                test_support::exact_counter_value("harold.phone_requests.wrong", &set).is_some(),
                "{set:?}"
            );
        }
    }
}

/// The first `sh` block after the paragraph starting `after` in the README's "Creating the
/// definitions" section.
fn readme_block(after: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs/references/telemetry/README.md");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path:?}: {error}"));
    let section = text
        .split("\n## Creating the definitions\n")
        .nth(1)
        .expect("the Creating the definitions section")
        .split("\n## ")
        .next()
        .unwrap();
    let start = section
        .find(&format!("\n{after}"))
        .unwrap_or_else(|| panic!("no paragraph starting {after:?}"));
    let block = section[start..]
        .split("```sh\n")
        .nth(1)
        .expect("an sh block after it");
    block.split("\n```").next().unwrap().to_owned()
}

/// A stand-in for the README's `hdx`: webhooks, alerts and dashboards are files under
/// `$STORE/<kind>/`, and every call is logged to `$STORE/calls` as `<method> <path>`, with
/// ` empty` after a POST or PUT that sent no body. With `HDX_DOWN` set it answers every call
/// the way HyperDX answers a bad key; with `HDX_UNREACHABLE` set it fails every call the way
/// curl does when it cannot connect, with exit status 7 and no output; with `HDX_EMPTY` set it
/// answers every call with nothing and exit status 0, as curl does for an error page with no body.
const STAND_IN_HDX: &str = r#"
hdx() {
  _method=GET _url=
  while [ $# -gt 0 ]; do
    case $1 in
      -X) _method=$2; shift 2 ;;
      --data-binary|-H) shift 2 ;;
      -*) shift ;;
      *) _url=$1; shift ;;
    esac
  done
  _path=${_url#"$HYPERDX_URL"}
  _body=
  [ "$_method" = GET ] || _body=$(cat)
  if [ "$_method" != GET ] && [ -z "$_body" ]; then
    echo "$_method $_path empty" >> "$STORE/calls"
  else
    echo "$_method $_path" >> "$STORE/calls"
  fi
  [ -z "$HDX_UNREACHABLE" ] || return 7
  [ -z "$HDX_EMPTY" ] || return 0
  [ -z "$HDX_DOWN" ] || { echo '{"message":"Unauthorized"}'; return; }
  _rest=${_path#/api/v2/}
  _kind=${_rest%%/*}
  case $_method/$_rest in
    GET/*) cat "$STORE/$_kind"/*.json 2>/dev/null | jq -s '{data: .}' ;;
    POST/dashboards/validate) echo '{}' ;;
    POST/*)
      _n=$(( $(cat "$STORE/next" 2>/dev/null || echo 0) + 1 ))
      echo "$_n" > "$STORE/next"
      mkdir -p "$STORE/$_kind"
      printf '%s' "$_body" | jq --arg id "$_kind-$_n" '. + {id: $id}' > "$STORE/$_kind/$_kind-$_n.json" \
        && jq '{data: .}' "$STORE/$_kind/$_kind-$_n.json" ;;
    PUT/*)
      _id=${_rest#*/}
      if [ -n "$_id" ] && [ -f "$STORE/$_kind/$_id.json" ]; then
        printf '%s' "$_body" | jq --arg id "$_id" '. + {id: $id}' > "$STORE/$_kind/$_id.new" \
          && mv "$STORE/$_kind/$_id.new" "$STORE/$_kind/$_id.json"
      fi
      echo '{}' ;;
  esac
}
"#;

struct StandIn {
    store: PathBuf,
}

impl StandIn {
    fn new() -> Self {
        let store = std::env::temp_dir().join(format!("harold-hdx-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&store).unwrap();
        Self { store }
    }

    /// Runs `script` under `/bin/sh` from the repository root, after the stand-in and the
    /// README's lookup block. Returns what it wrote to stderr.
    fn run(&self, script: &str) -> String {
        let lookups = readme_block("The files name HyperDX objects");
        let output = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("{STAND_IN_HDX}\n{lookups}\n{script}\n"))
            .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join(".."))
            .env("STORE", &self.store)
            .env("HYPERDX_URL", "http://hyperdx.test")
            .env_remove("HDX_DOWN")
            .env_remove("HDX_UNREACHABLE")
            .env_remove("HDX_EMPTY")
            .output()
            .unwrap();
        String::from_utf8(output.stderr).unwrap()
    }

    fn objects(&self, kind: &str) -> Vec<serde_json::Value> {
        let Ok(entries) = std::fs::read_dir(self.store.join(kind)) else {
            return Vec::new();
        };
        entries
            .map(|entry| {
                let text = std::fs::read_to_string(entry.unwrap().path()).unwrap();
                serde_json::from_str(&text).unwrap()
            })
            .collect()
    }

    /// The one object of `kind` named `name`.
    fn only(&self, kind: &str, name: &str) -> serde_json::Value {
        let mut named: Vec<_> = self
            .objects(kind)
            .into_iter()
            .filter(|object| object["name"] == name)
            .collect();
        assert_eq!(named.len(), 1, "{kind} named {name}");
        named.remove(0)
    }

    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.store.join("calls"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn forget_calls(&self) {
        let _ = std::fs::remove_file(self.store.join("calls"));
    }
}

impl Drop for StandIn {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.store);
    }
}

fn create_alerts() -> String {
    readme_block("Then create each alert's webhook")
}

#[test]
fn the_create_loop_makes_each_alert_with_its_own_webhook() {
    let stand_in = StandIn::new();
    let stderr = stand_in.run(&create_alerts());
    assert_eq!(stand_in.objects("webhooks").len(), 6, "{stderr}");
    assert_eq!(stand_in.objects("alerts").len(), 6, "{stderr}");
    for name in crate::alerts::ALERT_NAMES {
        let webhook = stand_in.only("webhooks", &format!("harold-alerts-{name}"));
        let alert = stand_in.only("alerts", name);
        assert_eq!(alert["channel"]["webhookId"], webhook["id"], "{name}");
    }
}

#[test]
fn the_create_loop_run_twice_makes_nothing_new() {
    let stand_in = StandIn::new();
    stand_in.run(&create_alerts());
    stand_in.forget_calls();
    let stderr = stand_in.run(&create_alerts());
    assert_eq!(stand_in.objects("webhooks").len(), 6, "{stderr}");
    assert_eq!(stand_in.objects("alerts").len(), 6, "{stderr}");
    for name in crate::alerts::ALERT_NAMES {
        assert!(
            stderr.contains(&format!(
                "alert {name} already exists; use the update commands"
            )),
            "{stderr}"
        );
    }
    let calls = stand_in.calls();
    assert!(
        !calls.iter().any(|call| call.starts_with("POST")),
        "{calls:?}"
    );
}

#[test]
fn the_create_loop_stops_at_an_alert_name_used_twice() {
    let stand_in = StandIn::new();
    stand_in.run(&create_alerts());
    stand_in.run(
        r#"fill alert-wrong-agent.json | hdx -X POST "$HYPERDX_URL/api/v2/alerts" --data-binary @- >/dev/null"#,
    );
    stand_in.forget_calls();
    let stderr = stand_in.run(&create_alerts());
    assert!(
        stderr.contains("2 alerts are named wrong-agent; delete all but one"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("alert slot-idle already exists"),
        "the loop stops: {stderr}"
    );
    let calls = stand_in.calls();
    assert!(
        !calls.iter().any(|call| call.starts_with("POST")),
        "{calls:?}"
    );
}

#[test]
fn the_create_loop_stops_when_a_lookup_fails() {
    let stand_in = StandIn::new();
    stand_in.run(&format!("HDX_DOWN=1\n{}", create_alerts()));
    assert!(stand_in.objects("webhooks").is_empty());
    assert!(stand_in.objects("alerts").is_empty());
    let calls = stand_in.calls();
    assert!(
        !calls.iter().any(|call| call.starts_with("POST")),
        "{calls:?}"
    );
}

fn update_alerts() -> String {
    readme_block("Then update each alert's webhook")
}

/// The PUTs in `calls` to the webhook or alert with one of `ids`.
fn puts_to(calls: &[String], ids: &[&serde_json::Value]) -> Vec<String> {
    calls
        .iter()
        .filter(|call| {
            call.starts_with("PUT ")
                && ids
                    .iter()
                    .any(|id| call.ends_with(&format!("/{}", id.as_str().unwrap())))
        })
        .cloned()
        .collect()
}

#[test]
fn the_update_loop_stops_at_a_webhook_name_used_twice() {
    let stand_in = StandIn::new();
    stand_in.run(&create_alerts());
    stand_in.run(
        r#"fill webhook-slot-idle.json | hdx -X POST "$HYPERDX_URL/api/v2/webhooks" --data-binary @- >/dev/null"#,
    );
    let alert = stand_in.only("alerts", "slot-idle");
    let webhooks: Vec<_> = stand_in
        .objects("webhooks")
        .into_iter()
        .filter(|webhook| webhook["name"] == "harold-alerts-slot-idle")
        .collect();
    stand_in.forget_calls();
    let stderr = stand_in.run(&update_alerts());
    assert!(
        stderr.contains("2 webhooks are named harold-alerts-slot-idle; delete all but one"),
        "{stderr}"
    );
    assert!(!stderr.contains("create it first"), "one message: {stderr}");
    let calls = stand_in.calls();
    let mut ids: Vec<_> = webhooks.iter().map(|webhook| &webhook["id"]).collect();
    ids.push(&alert["id"]);
    assert_eq!(puts_to(&calls, &ids), Vec::<String>::new());
    assert!(
        !calls.iter().any(|call| call.ends_with(" empty")),
        "{calls:?}"
    );
}

#[test]
fn the_update_loop_stops_at_a_missing_alert() {
    let stand_in = StandIn::new();
    stand_in.run(&create_alerts());
    let missing = stand_in.only("alerts", "wrong-agent");
    let file = format!("alerts/{}.json", missing["id"].as_str().unwrap());
    std::fs::remove_file(stand_in.store.join(file)).unwrap();
    stand_in.forget_calls();
    let stderr = stand_in.run(&update_alerts());
    assert!(
        stderr.contains("no alert wrong-agent: create it first"),
        "{stderr}"
    );
    let calls = stand_in.calls();
    assert!(
        !calls
            .iter()
            .any(|call| call == "PUT /api/v2/alerts/" || call.starts_with("PUT /api/v2/alerts/ ")),
        "a PUT with no id: {calls:?}"
    );
    for name in ["phone-request-unavailable", "phone-request-slow"] {
        let webhook = stand_in.only("webhooks", &format!("harold-alerts-{name}"));
        let alert = stand_in.only("alerts", name);
        assert_eq!(
            puts_to(&calls, &[&webhook["id"], &alert["id"]]).len(),
            2,
            "{name}: {calls:?}"
        );
    }
    for name in ["wrong-agent", "slot-idle"] {
        let webhook = stand_in.only("webhooks", &format!("harold-alerts-{name}"));
        assert_eq!(
            puts_to(&calls, &[&webhook["id"]]),
            Vec::<String>::new(),
            "{name}"
        );
    }
    let slot_idle = stand_in.only("alerts", "slot-idle");
    assert_eq!(puts_to(&calls, &[&slot_idle["id"]]), Vec::<String>::new());
}

#[test]
fn the_update_loop_stops_at_a_missing_webhook() {
    let stand_in = StandIn::new();
    stand_in.run(&create_alerts());
    let missing = stand_in.only("webhooks", "harold-alerts-phone-request-slow");
    let file = format!("webhooks/{}.json", missing["id"].as_str().unwrap());
    std::fs::remove_file(stand_in.store.join(file)).unwrap();
    stand_in.forget_calls();
    let stderr = stand_in.run(&update_alerts());
    assert!(
        stderr.contains("no webhook harold-alerts-phone-request-slow: create it first"),
        "{stderr}"
    );
    let calls = stand_in.calls();
    let puts = calls.iter().filter(|call| call.starts_with("PUT")).count();
    assert_eq!(
        puts, 2,
        "only phone-request-unavailable's webhook and alert: {calls:?}"
    );
}

#[test]
fn the_old_setup_loop_creates_only_the_missing_webhooks() {
    let stand_in = StandIn::new();
    stand_in.run(
        r#"echo '{"name": "harold-alerts"}' | hdx -X POST "$HYPERDX_URL/api/v2/webhooks" --data-binary @- >/dev/null
for name in phone-request-slow slot-idle; do
  fill "webhook-$name.json" | hdx -X POST "$HYPERDX_URL/api/v2/webhooks" --data-binary @- >/dev/null
done"#,
    );
    let kept = ["phone-request-slow", "slot-idle"]
        .map(|name| stand_in.only("webhooks", &format!("harold-alerts-{name}"))["id"].clone());
    stand_in.forget_calls();
    let stderr = stand_in.run(&readme_block("A HyperDX set up from the earlier"));
    // The old webhook, the two kept, and the four created.
    assert_eq!(stand_in.objects("webhooks").len(), 7, "{stderr}");
    for name in crate::alerts::ALERT_NAMES {
        stand_in.only("webhooks", &format!("harold-alerts-{name}"));
    }
    for (name, id) in ["phone-request-slow", "slot-idle"].iter().zip(&kept) {
        let webhook = stand_in.only("webhooks", &format!("harold-alerts-{name}"));
        assert_eq!(&webhook["id"], id, "{name}");
    }
    let calls = stand_in.calls();
    let posts = calls.iter().filter(|call| call.starts_with("POST")).count();
    assert_eq!(posts, 4, "{calls:?}");
}

#[test]
fn the_create_loop_stops_when_hyperdx_cannot_be_reached() {
    let stand_in = StandIn::new();
    stand_in.run(&format!("HDX_UNREACHABLE=1\n{}", create_alerts()));
    let calls = stand_in.calls();
    assert!(
        !calls.iter().any(|call| call.starts_with("POST")),
        "{calls:?}"
    );
}

#[test]
fn the_create_loop_stops_when_hyperdx_answers_with_nothing() {
    let stand_in = StandIn::new();
    stand_in.run(&format!("HDX_EMPTY=1\n{}", create_alerts()));
    let calls = stand_in.calls();
    assert!(
        !calls.iter().any(|call| call.starts_with("POST")),
        "{calls:?}"
    );
}

#[test]
fn the_update_loop_stops_when_hyperdx_cannot_be_reached() {
    let stand_in = StandIn::new();
    stand_in.run(&create_alerts());
    stand_in.forget_calls();
    let stderr = stand_in.run(&format!("HDX_UNREACHABLE=1\n{}", update_alerts()));
    assert!(!stderr.contains("create it first"), "{stderr}");
    let calls = stand_in.calls();
    assert!(
        !calls.iter().any(|call| call.starts_with("PUT")),
        "{calls:?}"
    );
}

#[test]
fn the_update_loop_stops_at_an_alert_name_used_twice() {
    let stand_in = StandIn::new();
    stand_in.run(&create_alerts());
    stand_in.run(
        r#"fill alert-phone-request-slow.json | hdx -X POST "$HYPERDX_URL/api/v2/alerts" --data-binary @- >/dev/null"#,
    );
    stand_in.forget_calls();
    let stderr = stand_in.run(&update_alerts());
    assert!(
        stderr.contains("2 alerts are named phone-request-slow; delete all but one"),
        "{stderr}"
    );
    assert!(!stderr.contains("create it first"), "one message: {stderr}");
    let calls = stand_in.calls();
    let puts = calls.iter().filter(|call| call.starts_with("PUT")).count();
    assert_eq!(
        puts, 2,
        "only phone-request-unavailable's webhook and alert: {calls:?}"
    );
}

#[test]
fn the_old_setup_loop_stops_at_a_webhook_name_used_twice() {
    let stand_in = StandIn::new();
    stand_in.run(
        r#"for i in 1 2; do
  fill webhook-wrong-agent.json | hdx -X POST "$HYPERDX_URL/api/v2/webhooks" --data-binary @- >/dev/null
done"#,
    );
    stand_in.forget_calls();
    let stderr = stand_in.run(&readme_block("A HyperDX set up from the earlier"));
    assert!(
        stderr.contains("2 webhooks are named harold-alerts-wrong-agent; delete all but one"),
        "{stderr}"
    );
    let calls = stand_in.calls();
    let posts: Vec<_> = calls
        .iter()
        .filter(|call| call.starts_with("POST"))
        .collect();
    assert_eq!(posts.len(), 2, "only the two names before it: {calls:?}");
    let webhooks = stand_in.objects("webhooks");
    assert!(
        !webhooks
            .iter()
            .any(|webhook| webhook["name"] == "harold-alerts-slot-idle"),
        "the loop stops before slot-idle"
    );
}

#[test]
fn the_old_setup_loop_stops_when_hyperdx_cannot_be_reached() {
    let stand_in = StandIn::new();
    stand_in.run(&format!(
        "HDX_UNREACHABLE=1\n{}",
        readme_block("A HyperDX set up from the earlier")
    ));
    let calls = stand_in.calls();
    assert!(
        !calls.iter().any(|call| call.starts_with("POST")),
        "{calls:?}"
    );
}
