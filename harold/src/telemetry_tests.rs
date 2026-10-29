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
fn the_web_app_push_counter_is_counted_from_0_at_start() {
    test_support::install();
    register_phone_counters();
    for outcome in ["accepted", "gone", "refused", "unreachable"] {
        let counted =
            test_support::exact_counter_value("harold.web_app.pushes", &[("outcome", outcome)]);
        assert!(counted.is_some(), "{outcome}");
    }
}

#[test]
fn seconds_and_event_id_are_exported_on_a_log_record() {
    test_support::install();
    let event_id = format!("event-{}", uuid::Uuid::new_v4());
    tracing::info!(event_id = %event_id, seconds = 7_u64, "app page disconnected");
    tracing::warn!(event_id = %event_id, "app reply state not recorded");
    let logs = test_support::logs();
    let records: Vec<_> = logs
        .iter()
        .filter(|l| format!("{:?}", l.record).contains(&event_id))
        .collect();
    assert_eq!(records.len(), 2);
    for record in &records {
        let keys: Vec<&str> = record
            .record
            .attributes_iter()
            .map(|(key, _)| key.as_str())
            .collect();
        assert!(keys.contains(&"event_id"), "{keys:?}");
    }
    assert!(records.iter().any(|r| {
        r.record
            .attributes_iter()
            .any(|(k, _)| k.as_str() == "seconds")
    }));
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
fn text_cut_leaves_on_the_delivery_span() {
    test_support::install();
    let pane = unique_pane();
    let span = tracing::info_span!(target: TRACE_TARGET, "harold.delivery",
        pane_id = %pane, text_cut = tracing::field::Empty);
    span.in_scope(|| crate::channels::record_text_cut(true));
    drop(span);
    let spans = test_support::spans();
    let span = spans
        .iter()
        .find(|s| test_support::span_attr(s, "pane_id").as_deref() == Some(pane.as_str()))
        .unwrap();
    assert_eq!(
        test_support::span_attr(span, "text_cut").as_deref(),
        Some("true")
    );
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
fn the_pod_file_and_stage_fields_leave_a_log_record() {
    test_support::install();
    let product = format!("pod-test-{}", uuid::Uuid::new_v4());
    tracing::warn!(
        pod.product = %product,
        spec.id = "spec-a",
        spec.stage = "1b",
        file = "merged/spec-a.md",
        "pod file unreadable"
    );
    let logs = test_support::logs();
    let record = logs
        .iter()
        .find(|l| format!("{:?}", l.record).contains(&product))
        .unwrap();
    let keys: Vec<&str> = record
        .record
        .attributes_iter()
        .map(|(key, _)| key.as_str())
        .collect();
    assert_eq!(keys, vec!["pod.product", "spec.id", "spec.stage", "file"]);
}

#[test]
fn an_agent_monitor_degraded_record_leaves_with_component_and_reason() {
    test_support::install();
    let component = format!("degraded-{}", uuid::Uuid::new_v4());
    tracing::warn!(component = %component, reason = "timeout", reason_code = "timeout", "agent monitor degraded");
    let logs = test_support::logs();
    let record = logs
        .iter()
        .find(|l| format!("{:?}", l.record).contains(&component))
        .unwrap();
    let keys: Vec<&str> = record
        .record
        .attributes_iter()
        .map(|(key, _)| key.as_str())
        .collect();
    assert_eq!(keys, vec!["component", "reason"]);
}

#[test]
fn an_agent_monitor_read_retried_record_leaves_with_elapsed_ms() {
    test_support::install();
    let pane = unique_pane();
    tracing::info!(component = "screen", pane_id = %pane, elapsed_ms = 634_u64, "agent monitor read retried");
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
    assert_eq!(keys, vec!["component", "pane_id", "elapsed_ms"]);
}

#[test]
fn the_watch_run_time_fields_leave_a_log_record() {
    test_support::install();
    let watch = format!("run-ms-{}", uuid::Uuid::new_v4());
    tracing::info!(watch = %watch, wait_ms = 12_u64, run_ms = 34_u64, "watch run finished");
    let logs = test_support::logs();
    let record = logs
        .iter()
        .find(|l| format!("{:?}", l.record).contains(&watch))
        .unwrap();
    let keys: Vec<&str> = record
        .record
        .attributes_iter()
        .map(|(key, _)| key.as_str())
        .collect();
    assert_eq!(keys, vec!["watch", "wait_ms", "run_ms"]);
}

#[test]
fn the_call_sound_and_busy_fields_leave_a_log_record_and_text_still_does_not() {
    test_support::install();
    let pane = unique_pane();
    tracing::info!(pane_id = %pane, busy_reason = "agent_busy", heard_ms = 2000_u64,
        first_heard_ms = 40_u64, backlog_ms = 12_u64, background_start = 35_u64, call_from = "desk",
        background = 30_u64, loudest = 5657_u64, loud_frames = 50_u64,
        speech_cut = 1_u64, text = "TELEMETRY-CANARY-6641", "call ended");
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
    assert_eq!(
        keys,
        vec![
            "pane_id",
            "busy_reason",
            "heard_ms",
            "first_heard_ms",
            "backlog_ms",
            "background_start",
            "call_from",
            "background",
            "loudest",
            "loud_frames",
            "speech_cut"
        ]
    );
    assert!(!test_support::exported_text().contains("TELEMETRY-CANARY-6641"));
}

#[test]
fn the_echo_test_fields_leave_a_log_record_and_text_still_does_not() {
    test_support::install();
    let pane = unique_pane();
    tracing::info!(pane_id = %pane, setup = "speaker", kind = "silent", verdict = "pass",
        share_over_bar = 0.02_f64, voice_found = false, echo_cancellation = true,
        standalone = true, user_agent = "TELEMETRY-CANARY-3318", "echo test run");
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
    assert_eq!(
        keys,
        vec![
            "pane_id",
            "setup",
            "kind",
            "verdict",
            "share_over_bar",
            "voice_found",
            "echo_cancellation",
            "standalone"
        ]
    );
    assert!(!test_support::exported_text().contains("TELEMETRY-CANARY-3318"));
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
    let expected = if cfg!(target_os = "macos") {
        "darwin"
    } else {
        "linux"
    };
    assert_eq!(attribute("os.type").as_deref(), Some(expected));
}

#[test]
fn the_machine_stats_fields_are_allowed() {
    for field in [
        "metric",
        "value",
        "normal_top",
        "days",
        "top_processes",
        "minutes",
        "reason",
    ] {
        assert!(ALLOWED_FIELDS.contains(&field), "{field}");
    }
}

#[test]
fn insert_pressed_is_an_allowed_field() {
    assert!(ALLOWED_FIELDS.contains(&"insert_pressed"));
}

#[test]
fn every_allowed_field_is_in_a_docs_table() {
    // Does the docs name `doc` match the field? `<n>` is one or more digits and a closing
    // `…` is any rest.
    fn names(doc: &str, field: &str) -> bool {
        let (doc, open) = match doc.strip_suffix('…') {
            Some(doc) => (doc, true),
            None => (doc, false),
        };
        let mut rest = field;
        let mut parts = doc.split("<n>");
        let first = parts.next().unwrap_or_default();
        let Some(after) = rest.strip_prefix(first) else {
            return false;
        };
        rest = after;
        for part in parts {
            let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
            if digits == 0 {
                return false;
            }
            let Some(after) = rest[digits..].strip_prefix(part) else {
                return false;
            };
            rest = after;
        }
        open || rest.is_empty()
    }

    fn docs_files(dir: &Path, found: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("read docs dir") {
            let path = entry.expect("read docs entry").path();
            if path.is_dir() {
                docs_files(&path, found);
            } else if path.extension().is_some_and(|ext| ext == "md") {
                found.push(path);
            }
        }
    }

    // The field names a table row holds in backticks: the leading name of each span, so
    // `name = value` and `name=value` name `name`.
    fn row_names(row: &str, found: &mut Vec<String>) {
        for (i, span) in row.split('`').enumerate() {
            if i % 2 == 0 {
                continue;
            }
            let name: String = span
                .chars()
                .take_while(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | '<' | '>' | '…'))
                .collect();
            if !name.is_empty() {
                found.push(name);
            }
        }
    }

    let docs = Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs/references");
    let mut files = Vec::new();
    docs_files(&docs, &mut files);
    assert!(!files.is_empty(), "no docs found under {}", docs.display());

    let mut named = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file).expect("read docs file");
        let mut in_fields_table = false;
        let mut in_table = false;
        for line in text.lines() {
            if !line.trim_start().starts_with('|') {
                in_table = false;
                in_fields_table = false;
                continue;
            }
            if !in_table {
                // The first row of a table is its header row.
                in_table = true;
                in_fields_table = line.split('|').any(|cell| {
                    let cell = cell.trim();
                    cell.starts_with("Fields") || cell.starts_with("Attributes")
                });
                continue;
            }
            if in_fields_table {
                row_names(line, &mut named);
            }
        }
    }

    let missing: Vec<&str> = ALLOWED_FIELDS
        .iter()
        .copied()
        .filter(|field| !named.iter().any(|doc| names(doc, field)))
        .collect();
    assert!(
        missing.is_empty(),
        "these fields on ALLOWED_FIELDS are in no fields table under docs/references:\n{}",
        missing.join("\n")
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
    tracing::error!(target: "buckwal", pane_id = %pane, "buckwal probe");
    tracing::error!(target: "buckwal::uploader", pane_id = %pane, "buckwal uploader probe");
    tracing::error!(target: "buckwalish", "{}", canary);
    // Records from other crates still reach harold.log.
    assert!(logs.text().contains(&canary), "{}", logs.text());
    let exported = test_support::exported_text();
    assert!(!exported.contains(&canary));
    let bodies: Vec<String> = test_support::logs()
        .iter()
        .filter(|l| format!("{:?}", l.record).contains(&pane))
        .map(|l| format!("{:?}", l.record.body().unwrap()))
        .collect();
    assert_eq!(bodies.len(), 6, "{bodies:#?}");
    for probe in [
        "harold probe",
        "events probe",
        "events root probe",
        "module target probe",
        "buckwal probe",
        "buckwal uploader probe",
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

#[cfg(feature = "imessage")]
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
        let environment = Arc::new(crate::outbound::fake::FakeEnvironment::locked(""));
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
        ("harold/src/outbound/left_out.rs", "{}"),
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

/// How many `web_app` requests `outcome` has counted, with `resolved_by = none`. Other
/// tests in this process also finish web app requests, so a test checks for a rise of at
/// least what it finished.
fn web_app_requests(outcome: &str) -> u64 {
    test_support::exact_counter_value(
        "harold.phone_requests",
        &[
            ("channel", WEB_APP_CHANNEL),
            ("outcome", outcome),
            ("resolved_by", "none"),
        ],
    )
    .unwrap_or(0)
}

fn web_app_latency_sum() -> f64 {
    test_support::histogram_sum(
        "harold.phone_request.latency",
        &[("channel", WEB_APP_CHANNEL)],
    )
    .unwrap_or(0.0)
}

fn web_app_typed(typed_at: OffsetDateTime) -> Routing {
    Routing::Typed {
        pane_id: unique_pane(),
        resolved_by: ResolvedBy::MyAgent,
        tagged: false,
        typed_at,
    }
}

#[test]
fn a_web_app_request_typed_on_time_is_counted_typed_with_its_latency() {
    test_support::install();
    let now = OffsetDateTime::now_utc();
    let (typed_before, latency_before) = (web_app_requests("typed"), web_app_latency_sum());
    let reply = open_phone_request(WEB_APP_CHANNEL, now - time::Duration::seconds(30));

    finish_phone_request_in(&mut PhoneBook::default(), &reply, &web_app_typed(now), now);

    assert!(web_app_requests("typed") > typed_before);
    assert!(web_app_latency_sum() >= latency_before + 29.0);
    assert_eq!(
        test_support::exact_counter_value(
            "harold.phone_requests.wrong",
            &[("channel", WEB_APP_CHANNEL), ("resolved_by", "none")],
        ),
        None,
        "a web app reply is never judged wrong"
    );
}

#[test]
fn a_web_app_request_with_no_pane_is_counted_unavailable() {
    test_support::install();
    let now = OffsetDateTime::now_utc();
    let before = web_app_requests("unavailable");
    let reply = open_phone_request(WEB_APP_CHANNEL, now);

    finish_phone_request_in(&mut PhoneBook::default(), &reply, &Routing::NoPane, now);

    assert!(web_app_requests("unavailable") > before);
}

#[test]
fn a_web_app_request_typed_eleven_minutes_late_is_counted_unavailable() {
    test_support::install();
    let now = OffsetDateTime::now_utc();
    let before = web_app_requests("unavailable");
    let reply = open_phone_request(WEB_APP_CHANNEL, now - time::Duration::minutes(11));

    finish_phone_request_in(&mut PhoneBook::default(), &reply, &web_app_typed(now), now);

    assert!(web_app_requests("unavailable") > before);
}

#[test]
fn paused_and_not_owned_web_app_requests_add_nothing() {
    test_support::install();
    let now = OffsetDateTime::now_utc();
    let book = &mut PhoneBook::default();
    for routing in [Routing::Paused, Routing::NotOwned { tagged: true }] {
        let verdict = book.judge(WEB_APP_CHANNEL, now, now, &routing);
        assert_eq!(
            verdict,
            PhoneVerdict {
                outcome: None,
                latency: None,
                wrong: None
            }
        );
    }
}

#[test]
fn a_web_app_reply_leaves_the_wrong_agent_look_back_on_the_imessage_reply() {
    let channel = unique_channel();
    let mut book = PhoneBook::default();
    let now = OffsetDateTime::now_utc();
    let first = open_phone_request(channel, now - time::Duration::seconds(60));
    finish_phone_request_in(
        &mut book,
        &first,
        &typed("%look-back-a", false),
        now - time::Duration::seconds(50),
    );
    // Typed, unavailable and not-owned web app replies in between.
    for routing in [
        web_app_typed(now - time::Duration::seconds(30)),
        Routing::NoPane,
        Routing::NotOwned { tagged: true },
    ] {
        let web = open_phone_request(WEB_APP_CHANNEL, now - time::Duration::seconds(30));
        finish_phone_request_in(&mut book, &web, &routing, now - time::Duration::seconds(30));
    }
    let tagged = open_phone_request(channel, now - time::Duration::seconds(10));
    finish_phone_request_in(&mut book, &tagged, &typed("%look-back-b", true), now);

    assert_eq!(
        test_support::counter_value(
            "harold.phone_requests.wrong",
            &[("channel", channel), ("resolved_by", "latest_notifier")],
        ),
        Some(1),
        "the first iMessage reply is still the one looked back at"
    );
    assert_eq!(
        test_support::exact_counter_value(
            "harold.phone_requests.wrong",
            &[("channel", WEB_APP_CHANNEL), ("resolved_by", "none")],
        ),
        None
    );
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
    "system.filesystem.usage",
    "system.filesystem.utilization",
    "system.memory.usage",
    "system.memory.utilization",
    "system.paging.usage",
    "system.cpu.utilization",
    "system.cpu.load_average.1m",
    "process.memory.usage",
    "process.cpu.utilization",
    "harold.machine.normal_top",
    "harold.machine.past_normal",
    "harold.app_spans.not_forwarded",
    "pod.slots.busy",
    "pod.slots.free",
    "pod.merges",
    "pod.merged_lines",
    "harold.phone_requests",
    "harold.phone_requests.wrong",
    "harold.phone_request.latency",
    "harold.turns",
    "harold.turn.duration",
    "pod.specs",
    "pod.spec.words",
    "pod.spec.effort",
    "pod.pods",
    "pod.agents",
    "pod.spec.lead_time",
    "watch.runs",
    "watch.cost_usd",
    "harold.notifications",
    "harold.knowledge.graph_waiting",
    "harold.knowledge.typed_relations",
];

/// The Metrics table's attributes, plus the two resource attributes every export carries.
const ATTRIBUTES: &[&str] = &[
    "first_audio_ms",
    "loaded",
    "device",
    "caller",
    "route",
    "hidden_reported",
    "metric",
    "exception.type",
    "code.filepath",
    "harold.opened_at",
    "load_waited_ms",
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
    "measure",
    "role",
    "withdrawn.reason",
    "withdrawn.reached",
    "harold.machine",
    "service.name",
    "folder",
    "cost_usd",
    "agent",
    "call_from",
    "first_sound_ms",
    "answer_ms",
    "answered_by",
    "cost_micros",
    "source",
    "results",
    "embed_ms",
    "search_ms",
    "rss_mb",
    "embed_ms_per_passage",
    "passages",
    "buckwal.outcome",
];

/// The spec's span table. The trends panel "One turn, end to end" lists `harold.turn`.
const SPANS: &[&str] = &[
    "harold.tts.speak",
    "harold.knowledge.graph_rebuild",
    "harold.machine_stats",
    "browser.error",
    "harold.phone_request",
    "harold.route",
    "harold.turn",
    "harold.turn_complete",
    "harold.delivery",
    "harold.watch.run",
    "harold.watch.model",
    "harold.call_turn",
    "harold.call_answer",
    "harold.call_transcribe",
    "harold.knowledge.search",
];

/// The spec's cards table: group, card, the metrics or spans its tile reads, and how it
/// reads them. For a builder tile, the first phrase is its `aggFn` and level; for a SQL
/// tile, every phrase is in its SQL.
const CARDS: [(&str, &str, &[&str], &[&str]); 16] = [
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
            "Attributes['state'] = 'open'",
        ],
    ),
    (
        "Specs",
        "Merged",
        &["pod.spec.words"],
        &[
            "SELECT count()",
            "min(TimeUnix) AS first_merged",
            "Attributes['state'] = 'completed'",
        ],
    ),
    (
        "Specs",
        "Spec size, median words",
        &["pod.spec.words"],
        &[
            "SELECT median(words)",
            "min(TimeUnix) AS first_ready",
            "Attributes['state'] != 'withdrawn'",
        ],
    ),
    (
        "Specs",
        "Waiting for you, p50",
        &["pod.spec.lead_time"],
        &[
            "quantileExactWeighted(0.5)",
            "Attributes['phase'] = 'open_to_approved'",
        ],
    ),
    (
        "Specs",
        "Build, p50",
        &["pod.spec.lead_time"],
        &[
            "quantileExactWeighted(0.5)",
            "Attributes['phase'] = 'started_to_completed'",
        ],
    ),
];

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
const TREND_PANELS: [&str; 12] = [
    "Slots",
    "Merges to main",
    "Phone requests reaching the right agent",
    "Phone request latency",
    "One turn, end to end",
    "Turns per session",
    "Specs by state",
    "Spec size",
    "Withdrawn and wasted",
    "Pods and agents",
    "Time per step",
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
    let is_name_char =
        |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.';
    let mut names = Vec::new();
    for (start, _) in text.char_indices() {
        let rest = &text[start..];
        if !["pod.", "harold.", "watch.", "system.", "process."]
            .iter()
            .any(|prefix| rest.starts_with(prefix))
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
            "alert-disk-low.json",
            "alert-machine-past-normal.json",
            "alert-phone-request-slow.json",
            "alert-phone-request-unavailable.json",
            "alert-slot-idle.json",
            "alert-watch-cost-high.json",
            "alert-wrong-agent.json",
            "dashboard.json",
            "machine.json",
            "trends.json",
            "webhook-disk-low.json",
            "webhook-machine-past-normal.json",
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
            } else if *card == "Merges to main, last 24 h" {
                "Merges to main, the 24 h before".to_string()
            } else {
                format!("{card}, previous 24 h")
            };
            [card.to_string(), earlier]
        })
        .chain([
            NOTIFICATIONS_CARD.to_string(),
            SPOKEN_LINES_DROPPED_CARD.to_string(),
            CALL_TURN_CARD.to_string(),
            CALL_ANSWERS_FAILED_CARD.to_string(),
            CALL_ANSWERER_SPEND_CARD.to_string(),
        ])
        .chain(WEB_APP_CARDS.map(str::to_string))
        .chain(KNOWLEDGE_CARDS.map(str::to_string))
        .chain(TTS_CARDS.map(str::to_string))
        .chain([INVENTORY_FULL_READS_CARD.to_string()])
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
            "Knowledge index",
            "Notifications (leave the product filter empty)",
            "Phone requests",
            "Pods",
            "Specs",
            "TTS server",
            "Turns",
            "Web app (the phone's browser)"
        ]
    );

    for (pair, (group, card, reads, how)) in tiles(&dashboard).chunks(2).zip(CARDS) {
        let (current, earlier) = (&pair[0], &pair[1]);
        let hours = match card {
            LAST_HOUR_CARD => 1,
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
    let filter_names: Vec<_> = filters
        .iter()
        .map(|filter| {
            (
                filter["name"].as_str().unwrap(),
                filter["expression"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        filter_names,
        [
            (
                "Product (pod and spec cards only)",
                "Attributes['pod.product']"
            ),
            ("Agent (notifications card only)", "Attributes['agent']"),
            ("Channel (notifications card only)", "Attributes['channel']"),
        ]
    );
    assert_eq!(filters[1]["sourceMetricType"], "sum");
    assert_eq!(filters[2]["sourceMetricType"], "sum");
}

const NOTIFICATIONS_CARD: &str = "Notifications by agent and channel";
const SPOKEN_LINES_DROPPED_CARD: &str = "Spoken lines dropped while locked, by pane, per day";
const CALL_TURN_CARD: &str = "Call turn, time to first sound and to the answer, last 7 days";
/// The knowledge index's cards, in the order the dashboard lists them.
const KNOWLEDGE_CARDS: [&str; 6] = [
    "Knowledge index: hours since the last good pass (info; the line is 2)",
    "Notes searches by source: median ms, searches, searches with 0 results",
    "Knowledge worker, last 7 days: highest rss_mb, mean ms a passage, passages",
    "buckwal commits that failed, per hour",
    "Graph records waiting (info; the line is 0)",
    "Typed relations, 30 days (info)",
];

/// The web app's cards, in the order the dashboard lists them.
const WEB_APP_CARDS: [&str; 8] = [
    "Web app errors per day",
    "Phone page load (p50, p95)",
    "Browser spans not forwarded per day",
    "Phone turns in one trace",
    "Browser spans stored per day",
    "Thread opened at the oldest row, 7 days",
    "Web app posts refused, by route, per day",
    "Pages closed without a hidden report, share per day",
];
/// The TTS server's cards, in the order the dashboard lists them.
const TTS_CARDS: [&str; 2] = [
    "TTS time to first audio, p50 and p95 by machine, loaded and device, last 7 days",
    "TTS failures per day, by machine, caller and reason",
];
const INVENTORY_FULL_READS_CARD: &str = "Agent inventory full reads per day, by reason";
const CALL_ANSWERS_FAILED_CARD: &str = "Call answers failed, by day";
const CALL_ANSWERER_SPEND_CARD: &str = "Call answerer spend, by day";

#[test]
fn the_notifications_card_is_a_table_of_sent_and_kept_over_the_picked_range() {
    let dashboard = definition("dashboard.json");
    let card = tiles(&dashboard)
        .iter()
        .find(|tile| tile["name"] == NOTIFICATIONS_CARD)
        .expect("the notifications card");
    let config = &card["config"];
    assert_eq!(config["displayType"], "table");
    assert_eq!(config["sourceId"], "@METRICS_SOURCE_ID@");
    let sql = config["sqlTemplate"].as_str().unwrap();
    for piece in [
        "Attributes['agent'] AS agent",
        "Attributes['channel'] AS channel",
        "AS sent",
        "AS kept",
        "Attributes['outcome'] = 'sent'",
        "Attributes['outcome'] = 'kept'",
        "MetricName IN ('harold.notifications')",
        "$__timeFilter(TimeUnix)",
        "$__fromTime - INTERVAL 1 DAY",
        "$__filters",
        "GROUP BY agent, channel",
    ] {
        assert!(sql.contains(piece), "{piece} in {sql}");
    }
    // The range is the dashboard's, not a fixed period.
    assert!(!sql.contains("now() - INTERVAL"), "{sql}");
}

#[test]
fn the_spoken_lines_dropped_card_counts_the_locked_drops_by_pane_and_day() {
    let dashboard = definition("dashboard.json");
    let card = tiles(&dashboard)
        .iter()
        .find(|tile| tile["name"] == SPOKEN_LINES_DROPPED_CARD)
        .expect("the spoken lines dropped card");
    let config = &card["config"];
    assert_eq!(config["displayType"], "table");
    let sql = config["sqlTemplate"].as_str().unwrap();
    for piece in [
        "otel.otel_logs",
        "Body = 'spoken line dropped (screen locked)'",
        "LogAttributes['pane_id'] AS pane_id",
        "INTERVAL 7 DAY",
        "GROUP BY day, pane_id",
    ] {
        assert!(sql.contains(piece), "{piece} in {sql}");
    }
}

#[test]
fn the_call_turn_card_is_a_table_of_the_two_sound_times_by_where_the_call_is() {
    let dashboard = definition("dashboard.json");
    let card = tiles(&dashboard)
        .iter()
        .find(|tile| tile["name"] == CALL_TURN_CARD)
        .expect("the call turn card");
    let config = &card["config"];
    assert_eq!(config["displayType"], "table");
    assert_eq!(config["sourceId"], "@TRACES_SOURCE_ID@");
    let sql = config["sqlTemplate"].as_str().unwrap();
    for piece in [
        "SpanName = 'harold.call_turn'",
        "SpanAttributes['call_from']",
        "SpanAttributes['first_sound_ms']",
        "SpanAttributes['answer_ms']",
        "SpanAttributes['answered_by'] AS answered_by",
        "quantileIf(0.5)",
        "quantileIf(0.95)",
        "INTERVAL 7 DAY",
        "GROUP BY call_from, answered_by",
        // The column "waited for the load": turns whose transcription waited for the worker.
        "SpanName = 'harold.call_transcribe'",
        "toUInt64OrZero(SpanAttributes['load_waited_ms']) > 0",
        "AS waited_for_the_load",
    ] {
        assert!(sql.contains(piece), "{piece} in {sql}");
    }
}

#[test]
fn the_call_answer_cards_count_failed_runs_and_sum_their_cost_for_each_day() {
    let dashboard = definition("dashboard.json");
    let sql_of = |name: &str| {
        let card = tiles(&dashboard)
            .iter()
            .find(|tile| tile["name"] == name)
            .expect("the call answer card");
        assert_eq!(card["config"]["displayType"], "table");
        assert_eq!(card["config"]["sourceId"], "@TRACES_SOURCE_ID@");
        card["config"]["sqlTemplate"].as_str().unwrap().to_owned()
    };
    let failed = sql_of(CALL_ANSWERS_FAILED_CARD);
    for piece in [
        "SpanName = 'harold.call_answer'",
        "SpanAttributes['result'] = 'failed'",
        "INTERVAL 7 DAY",
        "GROUP BY day",
    ] {
        assert!(failed.contains(piece), "{piece} in {failed}");
    }
    let spend = sql_of(CALL_ANSWERER_SPEND_CARD);
    for piece in [
        "SpanName = 'harold.call_answer'",
        "sum(toUInt64OrZero(SpanAttributes['cost_micros']))",
        "GROUP BY day",
    ] {
        assert!(spend.contains(piece), "{piece} in {spend}");
    }
}

#[test]
fn the_product_filter_reaches_the_pod_and_spec_tiles_only() {
    let dashboard = definition("dashboard.json");
    let containers = BTreeMap::from([
        ("pods", true),
        ("specs", true),
        ("phone", false),
        ("turns", false),
        ("web-app", false),
        ("knowledge", false),
        ("tts", false),
        // The card takes `$__filters`, so the product filter empties it: its container
        // title says to leave that filter empty.
        ("notifications", true),
    ]);
    for tile in tiles(&dashboard) {
        let name = tile["name"].as_str().unwrap();
        // A log card reads `otel.otel_logs`, which the product filter does not reach.
        let filtered =
            containers[tile["containerId"].as_str().unwrap()] && name != SPOKEN_LINES_DROPPED_CARD;
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
    // Seven counter cards and two lead-time cards, each with its earlier number, the
    // notifications card, the browser spans not forwarded card and three trends panels.
    assert_eq!(counted, 23);
}

#[test]
fn the_trends_dashboard_has_the_twelve_panels() {
    let trends = definition("trends.json");
    assert_eq!(trends["name"], "Harold trends");
    assert_eq!(tile_names(&trends), TREND_PANELS);
}

/// The SQL of the trends panel named `name`.
fn trends_sql(name: &str) -> String {
    let trends = definition("trends.json");
    let tile = tiles(&trends)
        .iter()
        .find(|tile| tile["name"] == name)
        .unwrap_or_else(|| panic!("no panel {name}"));
    tile["config"]["sqlTemplate"].as_str().unwrap().to_owned()
}

#[test]
fn the_spec_pod_and_agent_panels_read_the_new_gauges_and_steps() {
    let trends = definition("trends.json");
    let reads = |name: &str| {
        let tile = tiles(&trends)
            .iter()
            .find(|tile| tile["name"] == name)
            .unwrap();
        metrics_read(&tile["config"])
    };

    // The latest value of each product and state, not a sum over time.
    let by_state = trends_sql("Specs by state");
    assert_eq!(reads("Specs by state"), ["pod.specs"]);
    assert!(
        by_state.contains("argMax(Value, TimeUnix) AS specs"),
        "{by_state}"
    );
    for state in ["open", "approved", "in_progress", "completed", "withdrawn"] {
        assert!(by_state.contains(&format!("state = '{state}'")), "{state}");
    }

    let wasted = trends_sql("Withdrawn and wasted");
    assert_eq!(reads("Withdrawn and wasted"), ["pod.spec.effort"]);
    for phrase in [
        "withdrawn.reason",
        "withdrawn.reached",
        "'revisions'",
        "'direction_revisions'",
        "'architect_rounds'",
        "'build_worker_hours'",
        "direction_share",
    ] {
        assert!(wasted.contains(phrase), "{phrase}: {wasted}");
    }

    let pods = trends_sql("Pods and agents");
    assert_eq!(
        reads("Pods and agents"),
        ["pod.agents", "pod.pods", "pod.specs"]
    );
    for phrase in [
        "running_pods",
        "registered_pods",
        "stopped_pods_with_waiting_work",
        "'pod_coordination'",
        "'pnem'",
        "'build_worker'",
        "'chief'",
        "'other'",
    ] {
        assert!(pods.contains(phrase), "{phrase}: {pods}");
    }

    let steps = trends_sql("Time per step");
    assert_eq!(reads("Time per step"), ["pod.spec.lead_time"]);
    for phrase in [
        "median_7d",
        "slowest_7d",
        "median_30d",
        "slowest_30d",
        "INTERVAL 7 DAY",
        "INTERVAL 30 DAY",
    ] {
        assert!(steps.contains(phrase), "{phrase}: {steps}");
    }
    // The three phases are the ones the metric records.
    assert!(!steps.contains("waiting_for_user"));
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
            "watch-cost-high",
            METRICS_SOURCE,
            "watch.cost_usd",
            "sum",
            1.0,
            "above_exclusive",
            1440,
        ),
        (
            "machine-past-normal",
            METRICS_SOURCE,
            "harold.machine.past_normal",
            "max",
            1.0,
            "above",
            30,
        ),
        (
            "disk-low",
            METRICS_SOURCE,
            "system.filesystem.utilization",
            "max",
            0.9,
            "above",
            30,
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
    assert_eq!(config("wrong-agent")["select"][0]["where"], "");
    assert_eq!(config("slot-idle")["groupBy"], "Attributes['pod.product']");
}

#[test]
fn the_machine_dashboard_shows_each_machine_and_its_normal_range() {
    let dashboard = definition("machine.json");
    assert_eq!(dashboard["name"], "Machine resources");
    assert_eq!(dashboard["tags"], serde_json::json!(["harold"]));
    assert_eq!(
        tile_names(&dashboard),
        [
            "Disk used and free (home disk)",
            "Disk growth per day, last 7 days",
            "Memory use and its normal range top",
            "Swap in use and its normal range top",
            "CPU use and its normal range top",
            "Load average, 1 minute",
            "Past the normal range, by metric",
            "Harold's own memory and CPU",
            "Machine stats reading p95",
        ]
    );
    for tile in tiles(&dashboard) {
        let name = tile["name"].as_str().unwrap();
        let config = &tile["config"];
        assert_eq!(config["configType"], "sql", "{name}");
        assert_eq!(
            config["connectionId"], "@CLICKHOUSE_CONNECTION_ID@",
            "{name}"
        );
        let sql = config["sqlTemplate"].as_str().unwrap();
        // Every tile groups by machine, and none uses the counter rule.
        assert!(
            sql.contains("ResourceAttributes['harold.machine']"),
            "{name}: {sql}"
        );
        assert!(!sql.contains("rise"), "{name}: {sql}");
    }
    let sql_of = |name: &str| {
        let tile = tiles(&dashboard)
            .iter()
            .find(|tile| tile["name"] == name)
            .unwrap();
        tile["config"]["sqlTemplate"].as_str().unwrap().to_owned()
    };
    for (card, use_metric, metric) in [
        (
            "Memory use and its normal range top",
            "system.memory.utilization",
            "memory",
        ),
        (
            "Swap in use and its normal range top",
            "system.paging.usage",
            "swap",
        ),
        (
            "CPU use and its normal range top",
            "system.cpu.utilization",
            "cpu",
        ),
    ] {
        let sql = sql_of(card);
        for piece in [
            format!("MetricName = '{use_metric}'"),
            "MetricName = 'harold.machine.normal_top'".to_owned(),
            format!("Attributes['metric'] = '{metric}'"),
            "FROM $__sourceTable(gauge)".to_owned(),
        ] {
            assert!(sql.contains(&piece), "{card} must have {piece}: {sql}");
        }
    }
    let disk = sql_of("Disk used and free (home disk)");
    assert!(disk.contains("system.filesystem.usage"), "{disk}");
    assert!(disk.contains("Attributes['state'] = 'used'"), "{disk}");
    assert!(disk.contains("Attributes['state'] = 'free'"), "{disk}");
    let growth = sql_of("Disk growth per day, last 7 days");
    assert!(growth.contains("INTERVAL 7 DAY"), "{growth}");
    assert!(growth.contains("growth_bytes_per_day"), "{growth}");
    let past = sql_of("Past the normal range, by metric");
    assert!(past.contains("harold.machine.past_normal"), "{past}");
    assert!(past.contains("Attributes['metric']"), "{past}");
    let own = sql_of("Harold's own memory and CPU");
    assert!(own.contains("process.memory.usage"), "{own}");
    assert!(own.contains("process.cpu.utilization"), "{own}");
    let reading = tiles(&dashboard).last().unwrap();
    assert_eq!(reading["config"]["sourceId"], "@TRACES_SOURCE_ID@");
    let sql = reading["config"]["sqlTemplate"].as_str().unwrap();
    assert!(sql.contains("SpanName = 'harold.machine_stats'"), "{sql}");
    assert!(sql.contains("quantile(0.95)(Duration)"), "{sql}");
    // Only the placeholders the README fills.
    let mut texts = Vec::new();
    strings_in(&dashboard, &mut texts);
    for text in texts.iter().filter(|text| text.contains('@')) {
        assert!(
            [
                "@CLICKHOUSE_CONNECTION_ID@",
                "@METRICS_SOURCE_ID@",
                "@TRACES_SOURCE_ID@"
            ]
            .contains(&text.as_str()),
            "{text}"
        );
    }
}

#[test]
fn the_two_machine_alerts_have_no_group_by() {
    // Harold's alert address keeps one state for each alert name, so a grouped alert could
    // clear for one machine while another is still past.
    for name in ["machine-past-normal", "disk-low"] {
        let alert = definition(&format!("alert-{name}.json"));
        let config = &alert["chartConfig"];
        assert!(
            config
                .get("groupBy")
                .is_none_or(|group| group.is_null() || group == ""),
            "{name} is grouped"
        );
        assert_eq!(alert["interval"], "15m", "{name}");
        assert_eq!(alert["numConsecutiveWindows"], 2, "{name}");
        assert_eq!(config["select"][0]["metricType"], "gauge", "{name}");
        assert_eq!(config["select"][0]["where"], "", "{name}");
    }
    // The other alerts keep what they had.
    assert!(definition("alert-slot-idle.json")["chartConfig"]["groupBy"].is_string());
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

#[cfg(feature = "imessage")]
#[test]
fn judged_phone_requests_carry_imessage_or_unknown_as_their_channel() {
    assert_eq!(PHONE_CHANNELS, ["imessage", "unknown"]);
}

#[cfg(not(feature = "imessage"))]
#[test]
fn a_build_without_imessage_judges_phone_requests_as_unknown_only() {
    assert_eq!(PHONE_CHANNELS, ["unknown"]);
}

#[test]
fn the_web_app_phone_request_series_are_exported_from_zero_but_never_wrong() {
    test_support::install();
    register_phone_counters();
    for outcome in ["typed", "unavailable"] {
        let set = [
            ("channel", WEB_APP_CHANNEL),
            ("outcome", outcome),
            ("resolved_by", "none"),
        ];
        assert!(
            test_support::exact_counter_value("harold.phone_requests", &set).is_some(),
            "{set:?}"
        );
    }
    assert_eq!(
        test_support::counter_value(
            "harold.phone_requests.wrong",
            &[("channel", WEB_APP_CHANNEL)]
        ),
        None
    );
}

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
    assert_eq!(stand_in.objects("webhooks").len(), 7, "{stderr}");
    assert_eq!(stand_in.objects("alerts").len(), 7, "{stderr}");
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
    assert_eq!(stand_in.objects("webhooks").len(), 7, "{stderr}");
    assert_eq!(stand_in.objects("alerts").len(), 7, "{stderr}");
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
    // The old webhook, the two kept, and the five created.
    assert_eq!(stand_in.objects("webhooks").len(), 8, "{stderr}");
    for name in crate::alerts::ALERT_NAMES {
        stand_in.only("webhooks", &format!("harold-alerts-{name}"));
    }
    for (name, id) in ["phone-request-slow", "slot-idle"].iter().zip(&kept) {
        let webhook = stand_in.only("webhooks", &format!("harold-alerts-{name}"));
        assert_eq!(&webhook["id"], id, "{name}");
    }
    let calls = stand_in.calls();
    let posts = calls.iter().filter(|call| call.starts_with("POST")).count();
    assert_eq!(posts, 5, "{calls:?}");
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

#[test]
fn the_web_app_cards_are_tables_over_the_browser_spans_and_the_phone_turns() {
    let dashboard = definition("dashboard.json");
    let sql_of = |name: &str, source: &str| {
        let card = tiles(&dashboard)
            .iter()
            .find(|tile| tile["name"] == name)
            .unwrap_or_else(|| panic!("the {name} card"));
        assert_eq!(card["containerId"], "web-app", "{name}");
        assert_eq!(card["config"]["displayType"], "table", "{name}");
        assert_eq!(card["config"]["sourceId"], source, "{name}");
        card["config"]["sqlTemplate"].as_str().unwrap().to_owned()
    };
    for (name, source, pieces) in [
        (
            "Web app errors per day",
            "@TRACES_SOURCE_ID@",
            &[
                "ServiceName = 'harold-web-app'",
                "SpanName = 'browser.error'",
                "SpanAttributes['exception.type']",
                "SpanAttributes['code.filepath']",
            ][..],
        ),
        (
            "Phone page load (p50, p95)",
            "@TRACES_SOURCE_ID@",
            &[
                "SpanName = 'documentLoad'",
                "quantile(0.5)",
                "quantile(0.95)",
            ][..],
        ),
        (
            "Browser spans not forwarded per day",
            "@METRICS_SOURCE_ID@",
            &["harold.app_spans.not_forwarded", "Attributes['reason']"][..],
        ),
        (
            "Phone turns in one trace",
            "@TRACES_SOURCE_ID@",
            &[
                "SpanName = 'harold.turn'",
                "SpanAttributes['started_by'] = 'phone'",
                "ParentSpanId != ''",
            ][..],
        ),
        (
            "Browser spans stored per day",
            "@TRACES_SOURCE_ID@",
            &["ServiceName = 'harold-web-app'", "GROUP BY day"][..],
        ),
    ] {
        let sql = sql_of(name, source);
        for piece in pieces {
            assert!(sql.contains(piece), "{name}: {piece} in {sql}");
        }
        assert!(sql.contains("INTERVAL 7 DAY"), "{name}: {sql}");
    }
}

#[test]
fn a_buckwal_span_is_exported_under_harold_with_its_allowed_fields() {
    test_support::install();
    let marker = format!("m{}", uuid::Uuid::new_v4().simple());
    let span = tracing::info_span!(
        target: "buckwal",
        "buckwal.commit",
        buckwal.outcome = tracing::field::Empty,
        buckwal.bytes = 7_u64,
        text = %marker,
    );
    span.record("buckwal.outcome", "error");
    drop(span.entered());
    let found: Vec<_> = test_support::spans()
        .into_iter()
        .filter(|s| s.name == "buckwal.commit")
        .collect();
    let found = found
        .iter()
        .find(|s| test_support::span_attr(s, "buckwal.bytes").as_deref() == Some("7"))
        .expect("the buckwal span reached the exporter");
    assert_eq!(
        test_support::span_attr(found, "buckwal.outcome").as_deref(),
        Some("error")
    );
    assert_eq!(test_support::span_attr(found, "text"), None);
    assert!(!test_support::exported_text().contains(&marker));
}

#[test]
fn a_real_buckwal_commit_reaches_the_exporter() {
    test_support::install();
    let dir = std::env::temp_dir().join(format!("harold-buckwal-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let config = buckwal::Config::new(&dir, "http://127.0.0.1:1", "test", "ak", "sk");
    let db = buckwal::open_with_store(config, Arc::new(buckwal::store::MemStore::new())).unwrap();
    let conn = db.connect().unwrap();
    conn.execute("CREATE TABLE t(a)", &[]).unwrap();
    conn.execute("INSERT INTO t VALUES (1)", &[]).unwrap();
    assert!(
        test_support::spans()
            .iter()
            .any(|s| s.name == "buckwal.commit"),
        "a buckwal.commit span was exported"
    );
    drop(conn);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_knowledge_lines_export_their_counts_and_no_text() {
    test_support::install();
    let marker = format!("m{}", uuid::Uuid::new_v4().simple());
    tracing::info!(
        target: "harold::tools::knowledge",
        notes = 3_u64,
        count = 9_u64,
        removed = 1_u64,
        embed_ms = 40_u64,
        pending = 0_u64,
        last_ok_age_s = 0_u64,
        query = %marker,
        "knowledge indexed"
    );
    let logs = test_support::logs();
    let record = logs
        .iter()
        .find(|l| format!("{:?}", l.record).contains("knowledge indexed"))
        .expect("the line was exported");
    let keys: Vec<&str> = record
        .record
        .attributes_iter()
        .map(|(k, _)| k.as_str())
        .collect();
    for key in [
        "notes",
        "count",
        "removed",
        "embed_ms",
        "pending",
        "last_ok_age_s",
    ] {
        assert!(keys.contains(&key), "{key} in {keys:?}");
    }
    assert!(!keys.contains(&"query"));
    assert!(!test_support::exported_text().contains(&marker));
}

/// A stand-in for `curl` that answers `scripts/check_hyperdx_alerts.sh` from the same store as
/// `STAND_IN_HDX`. It reads the key from the `-K -` config on stdin and keeps it in
/// `$STORE/key_seen`, and keeps its command line in `$STORE/curl_args`, so a test can see that the
/// key arrived on stdin and not on the command line.
const STAND_IN_CURL: &str = r#"#!/bin/sh
cat > "$STORE/key_seen"
echo "$@" > "$STORE/curl_args"
for url; do :; done
[ -z "$HDX_UNREACHABLE" ] || { echo "curl: (7) Failed to connect to hyperdx.test" >&2; exit 7; }
[ -z "$HDX_DOWN" ] || { echo '{"message":"Unauthorized"}'; exit 0; }
kind=${url##*/}
cat "$STORE/$kind"/*.json 2>/dev/null | jq -s '{data: .}'
"#;

const CHECK_KEY: &str = "known-key-0123456789";

struct CheckResult {
    code: i32,
    stdout: String,
    stderr: String,
}

impl StandIn {
    /// Runs `scripts/check_hyperdx_alerts.sh` with the stand-in `curl` first on `PATH`, and with
    /// `CHECK_KEY` in a 0600 key file in the store.
    fn check(&self, env: &[(&str, &str)]) -> CheckResult {
        self.check_with_key(CHECK_KEY, env)
    }

    fn check_with_key(&self, key: &str, env: &[(&str, &str)]) -> CheckResult {
        use std::os::unix::fs::PermissionsExt;
        let bin = self.store.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let curl = bin.join("curl");
        std::fs::write(&curl, STAND_IN_CURL).unwrap();
        std::fs::set_permissions(&curl, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
        let key_file = self.store.join("hyperdx-access-key");
        std::fs::write(&key_file, format!("{key}\n")).unwrap();
        std::fs::set_permissions(&key_file, std::fs::Permissions::from_mode(0o600)).unwrap();
        let mut command = std::process::Command::new("bash");
        command
            .arg("scripts/check_hyperdx_alerts.sh")
            .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join(".."))
            .env("PATH", path)
            .env("STORE", &self.store)
            .env("HYPERDX_URL", "http://hyperdx.test")
            .env("HYPERDX_API_KEY_FILE", &key_file)
            .env_remove("HYPERDX_API_KEY")
            .env_remove("HDX_DOWN")
            .env_remove("HDX_UNREACHABLE");
        for (name, value) in env {
            command.env(name, value);
        }
        let output = command.output().unwrap();
        let result = CheckResult {
            code: output.status.code().unwrap(),
            stdout: String::from_utf8(output.stdout).unwrap(),
            stderr: String::from_utf8(output.stderr).unwrap(),
        };
        assert!(
            key.is_empty() || !result.stdout.contains(key) && !result.stderr.contains(key),
            "the key is in the output: {} {}",
            result.stdout,
            result.stderr
        );
        result
    }

    /// Rewrites the one object of `kind` named `name`.
    fn edit(&self, kind: &str, name: &str, change: impl FnOnce(&mut serde_json::Value)) {
        let id = self.only(kind, name)["id"].as_str().unwrap().to_owned();
        let path = self.store.join(kind).join(format!("{id}.json"));
        let mut object: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        change(&mut object);
        std::fs::write(&path, object.to_string()).unwrap();
    }

    fn remove(&self, kind: &str, name: &str) {
        let id = self.only(kind, name)["id"].as_str().unwrap().to_owned();
        std::fs::remove_file(self.store.join(kind).join(format!("{id}.json"))).unwrap();
    }
}

fn applied() -> StandIn {
    let stand_in = StandIn::new();
    let stderr = stand_in.run(&create_alerts());
    assert_eq!(stand_in.objects("alerts").len(), 7, "{stderr}");
    stand_in
}

#[test]
fn the_check_passes_after_the_create_loop() {
    let stand_in = applied();
    let result = stand_in.check(&[]);
    assert_eq!(result.code, 0, "{} {}", result.stdout, result.stderr);
    assert_eq!(result.stdout, "check_hyperdx_alerts: ok, 7 alerts\n");
    let key_seen = std::fs::read_to_string(stand_in.store.join("key_seen")).unwrap();
    assert!(
        key_seen.contains(CHECK_KEY),
        "the key reaches curl on stdin"
    );
}

const CHECK_REFUSED: &str = "check_hyperdx_alerts: cannot check, ";

#[test]
fn the_check_refuses_a_key_file_others_can_read() {
    use std::os::unix::fs::PermissionsExt;
    let stand_in = applied();
    let key_file = stand_in.store.join("loose-key");
    std::fs::write(&key_file, "known-key\n").unwrap();
    std::fs::set_permissions(&key_file, std::fs::Permissions::from_mode(0o644)).unwrap();
    let result = stand_in.check(&[("HYPERDX_API_KEY_FILE", key_file.to_str().unwrap())]);
    assert_eq!(result.code, 2);
    assert!(result.stdout.is_empty(), "{}", result.stdout);
    assert!(
        result.stderr.starts_with(&format!(
            "{CHECK_REFUSED}{}: readable by group or others",
            key_file.display()
        )),
        "{}",
        result.stderr
    );
    assert_eq!(result.stderr.lines().count(), 1);
}

#[test]
fn the_check_refuses_a_missing_or_empty_key_file() {
    let stand_in = applied();
    let missing = stand_in.store.join("no-such-key");
    let result = stand_in.check(&[("HYPERDX_API_KEY_FILE", missing.to_str().unwrap())]);
    assert_eq!(result.code, 2);
    assert_eq!(
        result.stderr,
        format!("{CHECK_REFUSED}{}: missing\n", missing.display())
    );
    let result = stand_in.check_with_key("", &[]);
    assert_eq!(result.code, 2);
    assert!(result.stderr.contains(": empty"), "{}", result.stderr);
}

#[test]
fn the_check_refuses_a_key_file_inside_a_git_checkout() {
    let stand_in = applied();
    let checkout = stand_in.store.join("checkout");
    std::fs::create_dir_all(&checkout).unwrap();
    let init = std::process::Command::new("git")
        .args(["init", "-q"])
        .arg(&checkout)
        .status()
        .unwrap();
    assert!(init.success());
    let key_file = checkout.join("key");
    std::fs::write(&key_file, "known-key\n").unwrap();
    std::fs::set_permissions(
        &key_file,
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )
    .unwrap();
    let result = stand_in.check(&[("HYPERDX_API_KEY_FILE", key_file.to_str().unwrap())]);
    assert_eq!(result.code, 2);
    assert_eq!(
        result.stderr,
        format!(
            "{CHECK_REFUSED}{}: inside a git checkout\n",
            key_file.display()
        )
    );
}

#[test]
fn the_check_without_hyperdx_url_cannot_check() {
    let stand_in = applied();
    let result = stand_in.check(&[("HYPERDX_URL", "")]);
    assert_eq!(result.code, 2);
    assert_eq!(result.stderr, format!("{CHECK_REFUSED}set HYPERDX_URL\n"));
}

#[test]
fn a_key_with_a_quote_and_a_backslash_reaches_curl_escaped() {
    let stand_in = applied();
    let key = r#"ab"cd\ef"#;
    let result = stand_in.check_with_key(key, &[]);
    assert_eq!(result.code, 0, "{} {}", result.stdout, result.stderr);
    let key_seen = std::fs::read_to_string(stand_in.store.join("key_seen")).unwrap();
    assert_eq!(
        key_seen,
        "header = \"Authorization: Bearer ab\\\"cd\\\\ef\"\n"
    );
    let args = std::fs::read_to_string(stand_in.store.join("curl_args")).unwrap();
    assert!(
        !args.contains("cd"),
        "the key is on curl's command line: {args}"
    );
}

#[test]
fn the_check_names_an_alert_that_is_missing() {
    let stand_in = applied();
    stand_in.remove("alerts", "wrong-agent");
    let result = stand_in.check(&[]);
    assert_eq!(result.code, 1);
    assert_eq!(
        result.stdout,
        "alert wrong-agent: missing in HyperDX\ncheck_hyperdx_alerts: 1 problems\n"
    );
}

#[test]
fn the_check_names_a_changed_threshold_with_both_values() {
    let stand_in = applied();
    stand_in.edit("alerts", "phone-request-slow", |alert| {
        alert["threshold"] = 61.into();
    });
    let result = stand_in.check(&[]);
    assert_eq!(result.code, 1);
    assert!(
        result
            .stdout
            .contains("alert phone-request-slow: threshold is 61 in HyperDX, 60 in the file\n"),
        "{}",
        result.stdout
    );
}

#[test]
fn the_check_names_a_changed_comparison_and_interval() {
    let stand_in = applied();
    stand_in.edit("alerts", "wrong-agent", |alert| {
        alert["thresholdType"] = "below".into();
        alert["interval"] = "1h".into();
    });
    let result = stand_in.check(&[]);
    assert_eq!(result.code, 1);
    assert!(
        result.stdout.contains(
            "alert wrong-agent: thresholdType is below in HyperDX, above_exclusive in the file\n"
        ),
        "{}",
        result.stdout
    );
    assert!(
        result
            .stdout
            .contains("alert wrong-agent: interval is 1h in HyperDX, 5m in the file\n"),
        "{}",
        result.stdout
    );
}

#[test]
fn the_check_names_a_changed_number_of_windows() {
    let stand_in = applied();
    stand_in.edit("alerts", "slot-idle", |alert| {
        alert["numConsecutiveWindows"] = 4.into();
    });
    let result = stand_in.check(&[]);
    assert_eq!(result.code, 1);
    assert!(
        result
            .stdout
            .contains("alert slot-idle: numConsecutiveWindows is 4 in HyperDX, 8 in the file\n"),
        "{}",
        result.stdout
    );
}

#[test]
fn the_check_accepts_only_the_recorded_default_for_a_file_without_windows() {
    let stand_in = applied();
    let alert = stand_in.only("alerts", "wrong-agent");
    assert!(
        alert["numConsecutiveWindows"].is_null(),
        "the default HyperDX returns when the file has none is null"
    );
    stand_in.edit("alerts", "wrong-agent", |alert| {
        alert["numConsecutiveWindows"] = 3.into();
    });
    let result = stand_in.check(&[]);
    assert_eq!(result.code, 1);
    assert!(
        result.stdout.contains(
            "alert wrong-agent: numConsecutiveWindows is 3 in HyperDX, none in the file\n"
        ),
        "{}",
        result.stdout
    );
}

#[test]
fn the_check_names_a_second_alert_of_one_name() {
    let stand_in = applied();
    stand_in.run(
        r#"fill alert-wrong-agent.json | hdx -X POST "$HYPERDX_URL/api/v2/alerts" --data-binary @- >/dev/null"#,
    );
    let result = stand_in.check(&[]);
    assert_eq!(result.code, 1);
    assert!(
        result
            .stdout
            .contains("alert wrong-agent: HyperDX has 2 of that name\n"),
        "{}",
        result.stdout
    );
}

#[test]
fn the_check_names_a_harold_alert_that_has_no_file_and_ignores_others() {
    let stand_in = applied();
    stand_in.run(
        r#"echo '{"name": "stray", "tags": ["harold"]}' | hdx -X POST "$HYPERDX_URL/api/v2/alerts" --data-binary @- >/dev/null
echo '{"name": "Alert: other", "tags": ["sre"]}' | hdx -X POST "$HYPERDX_URL/api/v2/alerts" --data-binary @- >/dev/null
echo '{"name": "alert-sink"}' | hdx -X POST "$HYPERDX_URL/api/v2/webhooks" --data-binary @- >/dev/null"#,
    );
    let result = stand_in.check(&[]);
    assert_eq!(result.code, 1);
    assert_eq!(
        result.stdout,
        "alert stray: tagged harold in HyperDX, has no file\ncheck_hyperdx_alerts: 1 problems\n"
    );
}

#[test]
fn the_check_names_a_missing_webhook_and_an_alert_pointing_elsewhere() {
    let stand_in = applied();
    stand_in.remove("webhooks", "harold-alerts-slot-idle");
    stand_in.edit("alerts", "wrong-agent", |alert| {
        alert["channel"]["webhookId"] = "somewhere-else".into();
    });
    let result = stand_in.check(&[]);
    assert_eq!(result.code, 1);
    assert!(
        result
            .stdout
            .contains("webhook harold-alerts-slot-idle: missing in HyperDX\n"),
        "{}",
        result.stdout
    );
    assert!(
        result
            .stdout
            .contains("alert wrong-agent: does not point at webhook harold-alerts-wrong-agent\n"),
        "{}",
        result.stdout
    );
}

#[test]
fn the_check_exits_2_when_hyperdx_cannot_be_checked() {
    let stand_in = applied();
    let down = stand_in.check(&[("HDX_UNREACHABLE", "1")]);
    assert_eq!(down.code, 2, "{} {}", down.stdout, down.stderr);
    assert!(down.stderr.contains("cannot check"), "{}", down.stderr);
    let refused = stand_in.check(&[("HDX_DOWN", "1")]);
    assert_eq!(refused.code, 2, "{} {}", refused.stdout, refused.stderr);
    assert!(
        refused.stderr.contains("did not answer with a list"),
        "{}",
        refused.stderr
    );
}

#[test]
fn the_check_names_a_missing_alert_and_its_missing_webhook_together() {
    let stand_in = applied();
    stand_in.remove("alerts", "watch-cost-high");
    stand_in.remove("webhooks", "harold-alerts-watch-cost-high");
    let result = stand_in.check(&[]);
    assert_eq!(result.code, 1);
    assert_eq!(
        result.stdout,
        "alert watch-cost-high: missing in HyperDX\nwebhook harold-alerts-watch-cost-high: missing in HyperDX\ncheck_hyperdx_alerts: 2 problems\n"
    );
}

#[test]
fn the_tts_cards_read_first_audio_by_machine_loaded_and_device_and_the_failures_by_reason() {
    let dashboard = definition("dashboard.json");
    let sql_of = |name: &str| {
        let card = tiles(&dashboard)
            .iter()
            .find(|tile| tile["name"] == name)
            .expect("the TTS card");
        assert_eq!(card["config"]["displayType"], "table");
        assert_eq!(card["containerId"], "tts");
        card["config"]["sqlTemplate"].as_str().unwrap().to_owned()
    };

    let speed = sql_of(TTS_CARDS[0]);
    for piece in [
        "SpanName = 'harold.tts.speak'",
        "SpanAttributes['result'] = 'spoke'",
        "ResourceAttributes['harold.machine'] AS machine",
        "SpanAttributes['loaded'] AS loaded",
        "SpanAttributes['device'] AS device",
        "quantile(0.5)(toFloat64OrZero(SpanAttributes['first_audio_ms']))",
        "quantile(0.95)(toFloat64OrZero(SpanAttributes['first_audio_ms']))",
        "INTERVAL 7 DAY",
        "GROUP BY day, machine, loaded, device",
    ] {
        assert!(speed.contains(piece), "{piece} in {speed}");
    }

    let failures = sql_of(TTS_CARDS[1]);
    for piece in [
        "otel.otel_logs",
        "Body = 'tts failed'",
        "LogAttributes['caller'] AS caller",
        "LogAttributes['reason'] AS reason",
        "ResourceAttributes['harold.machine'] AS machine",
        "GROUP BY day, machine, caller, reason",
    ] {
        assert!(failures.contains(piece), "{piece} in {failures}");
    }
    // Nothing in either card reads the text of a line.
    assert!(!speed.contains("text") && !failures.contains("text"));
}

/// Checks the ids of one dashboard value. `tiles` must hold at least one item; `containers` is optional.
fn check_dashboard_ids(file: &str, dashboard: &serde_json::Value) {
    const MAX_ID_LENGTH: usize = 36;
    let tiles = dashboard["tiles"]
        .as_array()
        .unwrap_or_else(|| panic!("{file} has no tiles list"));
    assert!(!tiles.is_empty(), "{file} has an empty tiles list");
    for list in ["tiles", "containers"] {
        let mut seen = std::collections::HashSet::new();
        for item in dashboard[list].as_array().into_iter().flatten() {
            let id = item["id"]
                .as_str()
                .unwrap_or_else(|| panic!("{file} has a {list} entry without an id"));
            let length = id.chars().count();
            assert!(
                (1..=MAX_ID_LENGTH).contains(&length),
                "{file} {list} id {id:?} has {length} characters, HyperDX accepts 1 to {MAX_ID_LENGTH}"
            );
            assert!(seen.insert(id), "{file} has the {list} id {id:?} twice");
        }
    }
}

/// HyperDX refuses a tile or container id of more than 36 characters.
#[test]
fn every_dashboard_tile_and_container_id_fits_hyperdx() {
    for file in ["dashboard.json", "trends.json", "machine.json"] {
        check_dashboard_ids(file, &definition(file));
    }
}

#[test]
#[should_panic(expected = "has no tiles list")]
fn the_dashboard_id_check_fails_when_tiles_is_missing() {
    check_dashboard_ids("x.json", &serde_json::json!({ "containers": [] }));
}

#[test]
#[should_panic(expected = "has an empty tiles list")]
fn the_dashboard_id_check_fails_when_tiles_is_empty() {
    check_dashboard_ids("x.json", &serde_json::json!({ "tiles": [] }));
}
