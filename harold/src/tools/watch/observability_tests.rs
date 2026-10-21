//! What a watch run emits: its spans, counters and log lines, and that none of them holds
//! the skill, its tools, the facts, the answer or a report.

use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
use opentelemetry_sdk::trace::SpanData;

use super::tests::{harness, pass, settings};
use super::*;
use crate::outbound::fake::CapturedLogs;
use crate::telemetry::test_support;
use crate::tools::watch::test_support::{Step, answer};

fn run_spans(name: &str) -> Vec<SpanData> {
    let mut spans: Vec<SpanData> = test_support::spans()
        .into_iter()
        .filter(|span| {
            span.name == "harold.watch.run"
                && test_support::span_attr(span, "watch").as_deref() == Some(name)
        })
        .collect();
    spans.sort_by_key(|span| {
        test_support::span_attr(span, "run")
            .unwrap()
            .parse::<u32>()
            .unwrap()
    });
    spans
}

fn attr(span: &SpanData, key: &str) -> Option<String> {
    test_support::span_attr(span, key)
}

fn exported_logs_of(
    name: &str,
) -> Vec<opentelemetry_sdk::logs::in_memory_exporter::LogDataWithResource> {
    test_support::logs()
        .into_iter()
        .filter(|log| format!("{:?}", log.record).contains(name))
        .collect()
}

fn body(log: &opentelemetry_sdk::logs::in_memory_exporter::LogDataWithResource) -> String {
    format!("{:?}", log.record.body().unwrap())
}

fn lines<'a>(text: &'a str, message: &str, name: &str) -> Vec<&'a str> {
    text.lines()
        .filter(|line| line.contains(message) && line.contains(&format!("watch=\"{name}\"")))
        .collect()
}

fn cost_total(folder: &str) -> f64 {
    test_support::metrics()
        .iter()
        .rev()
        .find_map(|resource| {
            resource
                .scope_metrics()
                .flat_map(|scope| scope.metrics())
                .find(|metric| metric.name() == "watch.cost_usd")
                .and_then(|metric| match metric.data() {
                    AggregatedMetrics::F64(MetricData::Sum(sum)) => Some(
                        sum.data_points()
                            .filter(|point| {
                                point.attributes().any(|kv| {
                                    kv.key.as_str() == "folder" && kv.value.as_str() == folder
                                })
                            })
                            .map(|point| {
                                assert_eq!(point.attributes().count(), 1, "folder only");
                                point.value()
                            })
                            .sum(),
                    ),
                    _ => None,
                })
        })
        .unwrap_or(0.0)
}

fn unique(prefix: &str) -> String {
    format!(
        "{prefix}-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..10]
    )
}

#[tokio::test(start_paused = true)]
async fn each_run_is_a_trace_with_a_model_child_and_its_log_lines_carry_the_trace_id() {
    test_support::install();
    let logs = CapturedLogs::start();
    let h = harness(
        settings(),
        vec![
            answer("QUIET", 0.012),
            answer("REPORT x", 0.02),
            Step::NoStart,
            answer("All good!", 0.01),
        ],
    )
    .await;
    let name = unique("trace");
    h.start(&name, 2, 120).await;

    pass(Duration::from_secs(6 * 60 + 5)).await;

    let spans = run_spans(&name);
    assert!(spans.len() >= 4, "{}", spans.len());
    let outcomes: Vec<_> = spans
        .iter()
        .take(4)
        .map(|span| attr(span, "outcome").unwrap())
        .collect();
    assert_eq!(
        outcomes,
        ["quiet", "report", "process_failed", "unreadable_answer"]
    );
    assert_eq!(attr(&spans[0], "cost_usd").as_deref(), Some("0.012"));
    assert_eq!(attr(&spans[2], "reason").as_deref(), Some("process_failed"));
    assert_eq!(
        attr(&spans[3], "reason").as_deref(),
        Some("unreadable_answer")
    );
    assert_eq!(attr(&spans[0], "reason"), None, "a good run has no reason");

    let all = test_support::spans();
    for span in spans.iter().take(4) {
        let children: Vec<&SpanData> = all
            .iter()
            .filter(|child| {
                child.parent_span_id == span.span_context.span_id()
                    && child.span_context.trace_id() == span.span_context.trace_id()
            })
            .collect();
        let names: Vec<&str> = children.iter().map(|child| child.name.as_ref()).collect();
        assert_eq!(names, ["harold.watch.model"], "the one child span");
    }
    let model_results: Vec<_> = spans
        .iter()
        .take(4)
        .map(|run| {
            let child = all
                .iter()
                .find(|child| child.parent_span_id == run.span_context.span_id())
                .unwrap();
            attr(child, "result").unwrap()
        })
        .collect();
    assert_eq!(
        model_results,
        ["exited", "exited", "failed_to_start", "exited"]
    );
    assert!(
        !all.iter()
            .any(|span| span.name == "harold.watch.facts" || span.name == "harold.watch.report"),
        "those spans were trimmed from the spec"
    );

    // The run's lines are written inside its span, so they carry its trace id.
    let exported = exported_logs_of(&name);
    let finished = exported
        .iter()
        .find(|l| body(l).contains("watch run finished"))
        .unwrap();
    assert_eq!(
        finished.record.trace_context().unwrap().trace_id,
        spans[0].span_context.trace_id()
    );
    let failed = exported
        .iter()
        .find(|l| body(l).contains("watch run failed"))
        .unwrap();
    assert_eq!(
        failed.record.trace_context().unwrap().trace_id,
        spans[2].span_context.trace_id()
    );
    // And the harold.log lines have no span prefix.
    assert!(!logs.text().contains("harold.watch.run"), "{}", logs.text());
}

#[tokio::test(start_paused = true)]
async fn the_counters_carry_the_folder_and_the_outcome_and_never_the_watch_name() {
    test_support::install();
    let h = harness(
        settings(),
        vec![
            answer("QUIET", 0.01),
            answer("REPORT x", 0.02),
            Step::NoStart,
            Step::Hang,
        ],
    )
    .await;
    let name = unique("counters");
    let folder = h.folder.0.to_string_lossy().into_owned();
    h.start(&name, 10, 120).await;
    // Counted from 0 at start for the failures and skipped runs.
    for outcome in [
        "timeout",
        "process_failed",
        "run_budget_reached",
        "unreadable_answer",
        "skill_missing",
        "max_turns_reached",
        "skipped",
    ] {
        assert_eq!(
            test_support::exact_counter_value(
                "watch.runs",
                &[("folder", &folder), ("outcome", outcome)]
            ),
            Some(0),
            "{outcome}"
        );
    }

    pass(Duration::from_secs(35 * 60)).await;

    let runs = |outcome: &str| {
        test_support::exact_counter_value(
            "watch.runs",
            &[("folder", &folder), ("outcome", outcome)],
        )
    };
    assert_eq!(runs("quiet"), Some(1));
    assert_eq!(runs("report"), Some(1));
    assert_eq!(runs("process_failed"), Some(1));
    assert_eq!(runs("timeout"), Some(1));
    assert!(
        (cost_total(&folder) - 0.03).abs() < 1e-9,
        "{}",
        cost_total(&folder)
    );
    // The metric points never carry a `watch` attribute.
    for resource in test_support::metrics() {
        for metric in resource.scope_metrics().flat_map(|scope| scope.metrics()) {
            if !metric.name().starts_with("watch.") {
                continue;
            }
            let dump = format!("{:?}", metric.data());
            assert!(!dump.contains("\"watch\""), "{}: {dump}", metric.name());
            assert!(!dump.contains(&name), "{}", metric.name());
        }
    }
}

#[tokio::test(start_paused = true)]
async fn every_log_line_of_the_table_is_written_with_its_fields() {
    test_support::install();
    let logs = CapturedLogs::start();
    let h = harness(
        settings(),
        vec![
            answer("REPORT first", 0.01),
            answer("REPORT second", 0.01),
            Step::NoStart,
            Step::NoStart,
            Step::NoStart,
        ],
    )
    .await;
    h.agents
        .set_state("%1", crate::agent::domain::EffectiveAgentState::Busy);
    let name = unique("lines");
    h.start(&name, 10, 120).await;
    pass(Duration::from_secs(15 * 60)).await;
    h.agents
        .set_state("%1", crate::agent::domain::EffectiveAgentState::Idle);
    pass(Duration::from_secs(10)).await;
    // The replacement is logged too.
    h.start(&name, 10, 120).await;
    pass(Duration::from_secs(60 * 60)).await;

    let text = logs.text();
    let started = lines(&text, "watch started", &name);
    assert_eq!(started.len(), 2, "{text}");
    for field in [
        "folder=",
        "skill=\"pod-watch\"",
        "ends_at=",
        "every_minutes=10",
        "every_run=false",
    ] {
        assert!(started[0].contains(field), "{field}: {}", started[0]);
    }
    assert!(started[0].contains("how=\"started\"") && started[1].contains("how=\"replaced\""));

    let finished = lines(&text, "watch run finished", &name);
    assert!(finished[0].contains("run=1") && finished[0].contains("outcome=\"report\""));
    assert!(finished[0].contains("cost_usd=0.01") && finished[0].contains("duration_ms="));
    let failed = lines(&text, "watch run failed", &name);
    assert!(failed[0].contains("reason=\"process_failed\"") && failed[0].contains("run="));

    let waiting = lines(&text, "watch report waiting", &name);
    // Once per report while its reason stays the same: the first report, then the second.
    assert_eq!(
        waiting.len(),
        2,
        "logged when the reason changes: {waiting:?}"
    );
    assert!(waiting[1].contains("run=2"));
    assert!(waiting[0].contains("reason=\"agent_busy\"") && waiting[0].contains("run=1"));
    let replaced = lines(&text, "watch report replaced", &name);
    assert!(replaced[0].contains("why=\"newer_report\"") && replaced[0].contains("run=1"));
    let typed = lines(&text, "watch report typed", &name);
    assert!(!typed.is_empty());
    assert!(typed[0].contains("pane=\"%1\""));
    let stopped = lines(&text, "watch stopped", &name);
    assert_eq!(stopped.len(), 1, "{stopped:?}");
    assert!(stopped[0].contains("reason=\"runs_failing\"") && stopped[0].contains("runs=3"));
    assert!(stopped[0].contains("cost_usd="));
}

#[tokio::test(start_paused = true)]
async fn a_yaml_list_of_tools_is_logged_once_per_run_and_the_run_gets_no_tools() {
    test_support::install();
    let logs = CapturedLogs::start();
    let h = harness(settings(), vec![answer("QUIET", 0.01)]).await;
    h.folder.write_skill(
        "pod-watch",
        "---\nname: pod-watch\nallowed-tools:\n  - Bash(bin/pod slots:*)\n---\nCheck.\n",
    );
    let name = unique("yaml");
    h.start(&name, 10, 120).await;

    pass(Duration::from_secs(11 * 60)).await;

    assert_eq!(h.runner.calls(), 2);
    let text = logs.text();
    let found = lines(&text, "watch skill tools not read", &name);
    assert_eq!(found.len(), 2, "once per run");
    assert!(
        found
            .iter()
            .all(|line| line.contains("reason=\"yaml_list\""))
    );
    assert!(
        h.runner
            .jobs
            .lock()
            .unwrap()
            .iter()
            .all(|job| job.allowed_tools.is_none())
    );
}

#[tokio::test(start_paused = true)]
async fn a_skipped_run_is_logged_with_its_reason() {
    test_support::install();
    let logs = CapturedLogs::start();
    let h = harness(
        settings(),
        vec![Step::Answer {
            result: "QUIET".into(),
            cost: 0.01,
            after: Duration::from_secs(100),
        }],
    )
    .await;
    let (first, second) = (unique("skipa"), unique("skipb"));
    h.start(&first, 10, 120).await;
    h.start(&second, 10, 120).await;

    pass(Duration::from_secs(70)).await;

    let text = logs.text();
    let skipped = lines(&text, "watch run skipped", &second);
    assert_eq!(skipped.len(), 1);
    assert!(skipped[0].contains("reason=\"model_busy\"") && skipped[0].contains("run=1"));
    let spans = run_spans(&second);
    assert_eq!(attr(&spans[0], "outcome").as_deref(), Some("skipped"));
    let folder = h.folder.0.to_string_lossy().into_owned();
    assert!(
        test_support::exact_counter_value(
            "watch.runs",
            &[("folder", &folder), ("outcome", "skipped")]
        )
        .unwrap()
            >= 1
    );
}

#[tokio::test(start_paused = true)]
async fn run_lines_carry_the_number_of_add_dirs_and_never_a_path() {
    test_support::install();
    let logs = CapturedLogs::start();
    let h = harness(
        settings(),
        vec![answer("REPORT found", 0.01), Step::NoStart],
    )
    .await;
    h.folder.write_skill(
        "pod-watch",
        "---\nadd-dirs: /ADDDIR-CANARY-6643/a, ~/ADDDIR-CANARY-6643/b\n---\nRules\n",
    );
    let name = unique("adddirs");
    h.start(&name, 10, 120).await;
    pass(Duration::from_secs(15 * 60)).await;
    // The skill goes: the run fails before it can read any folder.
    std::fs::remove_file(h.folder.0.join(".claude/skills/pod-watch/SKILL.md")).unwrap();
    pass(Duration::from_secs(10 * 60)).await;

    let jobs = h.runner.jobs.lock().unwrap().clone();
    assert_eq!(jobs[0].add_dirs.len(), 2);
    let text = logs.text();
    let finished = lines(&text, "watch run finished", &name);
    assert!(finished[0].contains("add_dirs=2"), "{}", finished[0]);
    let failed = lines(&text, "watch run failed", &name);
    assert!(failed[0].contains("process_failed") && failed[0].contains("add_dirs=2"));
    assert!(failed[1].contains("skill_missing") && failed[1].contains("add_dirs=0"));
    let exported = test_support::exported_text();
    assert!(!text.contains("ADDDIR-CANARY-6643"), "path in harold.log");
    assert!(
        !exported.contains("ADDDIR-CANARY-6643"),
        "path in an export"
    );
    assert!(!exported.contains("add_dirs"), "add_dirs is exported");
}

#[tokio::test(start_paused = true)]
async fn the_skill_its_tools_the_facts_the_answer_and_the_report_reach_no_log_or_export() {
    test_support::install();
    let logs = CapturedLogs::start();
    let h = harness(
        settings(),
        vec![
            answer("REPORT ANSWER-CANARY-5521 found", 0.01),
            Step::Raw {
                stdout: "RAW-CANARY-7710 not json".into(),
                ok: true,
            },
            answer("garbage ANSWER-CANARY-5522", 0.01),
        ],
    )
    .await;
    h.folder.write_skill(
        "pod-watch",
        "---\nallowed-tools: Bash(TOOL-CANARY-3391:*)\nadd-dirs: /DIRS-CANARY-4418/x\n---\nSKILL-CANARY-2207 rules\n",
    );
    h.agents.agents.lock().unwrap()[0].work_summary = Some("FACT-CANARY-8812".into());
    let name = unique("canary");
    h.start(&name, 2, 120).await;

    pass(Duration::from_secs(5 * 60)).await;

    // The canaries really reached the run and the typing...
    let jobs = h.runner.jobs.lock().unwrap().clone();
    assert!(jobs[0].system_prompt.contains("SKILL-CANARY-2207"));
    assert_eq!(
        jobs[0].allowed_tools.as_deref(),
        Some("Bash(TOOL-CANARY-3391:*)")
    );
    assert_eq!(jobs[0].add_dirs, ["/DIRS-CANARY-4418/x"]);
    assert!(String::from_utf8_lossy(&jobs[0].facts_json).contains("FACT-CANARY-8812"));
    assert!(
        h.typing
            .typed()
            .iter()
            .any(|line| line.contains("ANSWER-CANARY-5521"))
    );
    // ...and nowhere else.
    let exported = test_support::exported_text();
    let text = logs.text();
    for canary in [
        "SKILL-CANARY-2207",
        "TOOL-CANARY-3391",
        "DIRS-CANARY-4418",
        "FACT-CANARY-8812",
        "ANSWER-CANARY-5521",
        "ANSWER-CANARY-5522",
        "RAW-CANARY-7710",
    ] {
        assert!(!text.contains(canary), "{canary} in harold.log: {text}");
        assert!(!exported.contains(canary), "{canary} in an export");
    }
}

const CAUSE_FIELDS: [&str; 5] = [
    "exit_code=",
    "signal=",
    "subtype=",
    "is_error=",
    "num_turns=",
];

#[tokio::test(start_paused = true)]
async fn a_process_failed_run_logs_how_it_ended_and_what_claude_code_reported_without_text() {
    test_support::install();
    let logs = CapturedLogs::start();
    let h = harness(
        settings(),
        vec![
            Step::Exit {
                stdout: r#"{"subtype":"error_max_turns","is_error":true,"num_turns":8,"result":"CAUSE-CANARY-1"}"#.into(),
                exit: Exit {
                    code: Some(3),
                    signal: None,
                },
            },
            Step::Exit {
                stdout: "CAUSE-CANARY-2 not json".into(),
                exit: Exit {
                    code: None,
                    signal: Some(15),
                },
            },
            Step::Raw {
                stdout: r#"{"subtype":"Bad CAUSE-CANARY-3","is_error":true}"#.into(),
                ok: false,
            },
        ],
    )
    .await;
    let name = unique("cause");
    h.start(&name, 2, 120).await;
    pass(Duration::from_secs(5 * 60)).await;

    let text = logs.text();
    let failed = lines(&text, "watch run failed", &name);
    assert_eq!(failed.len(), 3, "{text}");
    for field in [
        "exit_code=3",
        "subtype=\"error_max_turns\"",
        "is_error=true",
        "num_turns=8",
    ] {
        assert!(failed[0].contains(field), "{field}: {}", failed[0]);
    }
    assert!(!failed[0].contains("signal="), "{}", failed[0]);
    assert!(
        failed[0].contains("reason=\"max_turns_reached\""),
        "{}",
        failed[0]
    );
    assert!(
        failed[1].contains("reason=\"process_failed\""),
        "{}",
        failed[1]
    );
    let folder = h.folder.0.to_string_lossy().into_owned();
    let runs = |outcome: &str| {
        test_support::exact_counter_value(
            "watch.runs",
            &[("folder", &folder), ("outcome", outcome)],
        )
    };
    assert_eq!(runs("max_turns_reached"), Some(1));
    assert!(failed[1].contains("signal=15") && failed[1].contains("subtype=\"unreadable_json\""));
    assert!(!failed[1].contains("exit_code="), "{}", failed[1]);
    assert!(failed[2].contains("subtype=\"other\"") && failed[2].contains("is_error=true"));
    assert!(!failed[2].contains("num_turns="), "{}", failed[2]);
    let exported = test_support::exported_text();
    for canary in ["CAUSE-CANARY-1", "CAUSE-CANARY-2", "CAUSE-CANARY-3"] {
        assert!(!text.contains(canary), "{canary} in harold.log");
        assert!(!exported.contains(canary), "{canary} in an export");
    }
    // The export filter keeps all five.
    for field in CAUSE_FIELDS {
        let key = field.trim_end_matches('=');
        assert!(crate::telemetry::ALLOWED_FIELDS.contains(&key), "{key}");
    }
    // They reach the export on the line itself.
    let logged = exported_logs_of(&name)
        .iter()
        .map(|log| format!("{:?}", log.record))
        .find(|record| record.contains("watch run failed") && record.contains("error_max_turns"))
        .expect("the failed line is exported");
    assert!(
        logged.contains("exit_code") && logged.contains("num_turns"),
        "{logged}"
    );
}

#[tokio::test(start_paused = true)]
async fn other_failures_log_none_of_the_cause_fields() {
    test_support::install();
    let logs = CapturedLogs::start();
    let h = harness(
        settings(),
        vec![
            Step::Raw {
                stdout:
                    r#"{"subtype":"error_max_budget_usd","is_error":true,"total_cost_usd":0.05}"#
                        .into(),
                ok: false,
            },
            Step::Hang,
            Step::NoStart,
        ],
    )
    .await;
    let name = unique("nocause");
    h.start(&name, 2, 120).await;
    pass(Duration::from_secs(20 * 60)).await;

    let text = logs.text();
    let failed = lines(&text, "watch run failed", &name);
    assert_eq!(failed.len(), 3, "{text}");
    // NoStart is a process_failed run too, but it never ran, so it has nothing to say.
    for line in &failed {
        for field in CAUSE_FIELDS {
            assert!(!line.contains(field), "{field} in {line}");
        }
    }
}
