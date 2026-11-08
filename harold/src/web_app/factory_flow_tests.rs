//! Tests of the factory flow page: the named query, the read, the levers and the page.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Mutex;
use std::time::Duration;

use super::flow::{
    FACTORY_FLOW_SQL, FlowOutcome, FlowRequest, FlowSource, Lever, LeversRead, ReqwestSource,
    SourceFailure, read_factory_flow, read_levers,
};
use crate::outbound::fake::CapturedLogs;
use crate::settings::FactoryFlowSettings;
use crate::telemetry::test_support;

const STAGES: [&str; 8] = [
    "idea",
    "spec",
    "architect review",
    "approval",
    "build",
    "merge",
    "release",
    "installed",
];

fn queries_file(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../docs/references/telemetry/queries")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

#[test]
fn the_build_embeds_the_named_query_file() {
    assert_eq!(FACTORY_FLOW_SQL, queries_file("factory-flow.sql"));
}

#[test]
fn the_query_names_the_eight_stages_in_order() {
    let mut at = 0;
    for stage in STAGES {
        let literal = format!("'{stage}'");
        let found = FACTORY_FLOW_SQL[at..]
            .find(&literal)
            .unwrap_or_else(|| panic!("stage {stage} is missing or out of order"));
        at += found + literal.len();
    }
    assert!(FACTORY_FLOW_SQL.contains("ORDER BY ord"));
    assert!(FACTORY_FLOW_SQL.contains("-- Columns: stage, items, median_wait_s, has_data."));
}

#[test]
fn the_query_header_names_the_two_files_that_its_expressions_come_from() {
    let header: String = FACTORY_FLOW_SQL
        .lines()
        .take_while(|line| line.starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(header.contains("work-waiting.sql"));
    assert!(header.contains("delivery-lead-time.sql"));
}

#[test]
fn the_approval_and_build_expressions_match_the_rows_they_come_from() {
    let waiting = queries_file("work-waiting.sql");
    let lead = queries_file("delivery-lead-time.sql");
    // work-waiting.sql: the gauge, its name and its lists.
    assert!(waiting.contains("pod.inventory.items"));
    for list in [
        "awaiting_approval",
        "approved_not_started",
        "in_progress",
        "merged_not_released",
    ] {
        assert!(FACTORY_FLOW_SQL.contains(&format!("'{list}'")), "{list}");
    }
    assert!(FACTORY_FLOW_SQL.contains("MetricName = 'pod.inventory.items'"));
    assert!(FACTORY_FLOW_SQL.contains("TimeUnix > now() - INTERVAL 7 DAY"));
    // delivery-lead-time.sql: the whole rise rule of the "spec:" rows, line by line.
    let rise = [
        "if(cnt >= before_cnt, toInt64(cnt) - toInt64(before_cnt), toInt64(cnt)) AS rise_count,",
        "if(cnt >= before_cnt, total - before_total, total) AS rise_sum",
        "lagInFrame(Count, 1, toUInt64(0)) OVER (",
        "lagInFrame(Sum, 1, toFloat64(0)) OVER (",
        "PARTITION BY cityHash64(MetricName, ResourceAttributes, Attributes)",
        "ORDER BY TimeUnix ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) AS before_cnt,",
        "WHERE MetricName = 'pod.spec.lead_time'",
    ];
    for line in rise {
        assert!(lead.contains(line), "delivery-lead-time.sql lost: {line}");
        assert!(
            FACTORY_FLOW_SQL.contains(line),
            "factory-flow.sql lost: {line}"
        );
    }
    for phase in [
        "open_to_approved",
        "approved_to_started",
        "started_to_completed",
    ] {
        assert!(FACTORY_FLOW_SQL.contains(&format!("'{phase}'")), "{phase}");
    }
    assert!(FACTORY_FLOW_SQL.contains("Attributes['spec.id']"));
    assert!(FACTORY_FLOW_SQL.contains("INTERVAL 30 DAY"));
}

#[test]
fn the_query_has_no_product_filter_and_no_format_clause() {
    assert!(!FACTORY_FLOW_SQL.contains("$__filters"));
    assert!(!FACTORY_FLOW_SQL.contains("FORMAT"));
}

const CANARY: &str = "FLOW-PASSWORD-CANARY-4417";

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("harold-factory-flow-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("create test directory");
        Self(path)
    }

    fn file(&self, name: &str, text: &str) -> String {
        let path = self.0.join(name);
        std::fs::write(&path, text).expect("write test file");
        path.to_string_lossy().into_owned()
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A users file as `bin/logq` reads it, with a fake password.
fn users_file_text(password: &str) -> String {
    format!(
        "CREATE USER IF NOT EXISTS other IDENTIFIED BY 'not-this-one';\nCREATE USER IF NOT EXISTS hyperdx_ro IDENTIFIED BY '{password}' SETTINGS readonly = 1;\n"
    )
}

fn settings(users_file: Option<String>) -> FactoryFlowSettings {
    FactoryFlowSettings {
        clickhouse_url: "http://localhost:8123/".to_string(),
        users_file,
        levers_file: None,
    }
}

/// The eight rows as ClickHouse prints them in `JSONEachRow`.
fn body() -> String {
    let rows = [
        ("idea", "null", "null", 0),
        ("spec", "null", "null", 0),
        ("architect review", "null", "null", 0),
        ("approval", "7", "68400", 1),
        ("build", "4", "181440.5", 1),
        ("merge", "null", "null", 0),
        ("release", "17", "null", 1),
        ("installed", "null", "null", 0),
    ];
    rows.iter()
        .map(|(stage, items, median, has)| {
            format!(
                "{{\"stage\":\"{stage}\",\"items\":{items},\"median_wait_s\":{median},\"has_data\":{has}}}\n"
            )
        })
        .collect()
}

/// What the fake source saw, and what it answers.
struct FakeSource {
    answer: Mutex<Option<Result<String, SourceFailure>>>,
    seen: Mutex<Vec<(String, String, String, String)>>,
}

impl FakeSource {
    fn answering(answer: Result<String, SourceFailure>) -> Self {
        Self {
            answer: Mutex::new(Some(answer)),
            seen: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

impl FlowSource for FakeSource {
    fn query<'a>(
        &'a self,
        request: &'a FlowRequest,
    ) -> Pin<Box<dyn Future<Output = Result<String, SourceFailure>> + Send + 'a>> {
        self.seen.lock().unwrap().push((
            request.url().to_string(),
            request.user().to_string(),
            request.password().to_string(),
            request.sql().to_string(),
        ));
        let answer = self.answer.lock().unwrap().take().expect("one answer");
        Box::pin(async move { answer })
    }
}

/// A source that never answers.
struct Silent;

impl FlowSource for Silent {
    fn query<'a>(
        &'a self,
        _request: &'a FlowRequest,
    ) -> Pin<Box<dyn Future<Output = Result<String, SourceFailure>> + Send + 'a>> {
        Box::pin(std::future::pending())
    }
}

#[tokio::test]
async fn a_good_answer_gives_eight_rows_and_sends_the_user_the_url_and_the_query() {
    let dir = TestDirectory::new();
    let users = dir.file("users.sql", &users_file_text(CANARY));
    let source = FakeSource::answering(Ok(body()));
    let read = read_factory_flow(&settings(Some(users)), &source).await;
    assert_eq!(read.outcome, FlowOutcome::Ok);
    let names: Vec<&str> = read.rows.iter().map(|row| row.stage.as_str()).collect();
    assert_eq!(names, STAGES);
    let approval = &read.rows[3];
    assert_eq!(
        (approval.items, approval.median_wait_s, approval.has_data),
        (Some(7.0), Some(68400.0), true)
    );
    let release = &read.rows[6];
    assert_eq!(
        (release.items, release.median_wait_s, release.has_data),
        (Some(17.0), None, true)
    );
    assert!(!read.rows[0].has_data && read.rows[0].items.is_none());
    let seen = source.seen.lock().unwrap();
    assert_eq!(
        seen[0],
        (
            "http://localhost:8123/".to_string(),
            "hyperdx_ro".to_string(),
            CANARY.to_string(),
            FACTORY_FLOW_SQL.to_string()
        )
    );
}

#[tokio::test]
async fn no_users_file_setting_is_not_set_up_and_asks_nobody() {
    let source = FakeSource::answering(Ok(body()));
    let read = read_factory_flow(&settings(None), &source).await;
    assert_eq!(read.outcome, FlowOutcome::NotSetUp);
    assert!(read.rows.is_empty());
    assert_eq!(source.calls(), 0);
}

#[tokio::test]
async fn a_missing_users_file_is_refused_and_asks_nobody() {
    let dir = TestDirectory::new();
    let missing = dir.0.join("none.sql").to_string_lossy().into_owned();
    let source = FakeSource::answering(Ok(body()));
    let read = read_factory_flow(&settings(Some(missing)), &source).await;
    assert_eq!(read.outcome, FlowOutcome::Refused);
    assert_eq!(source.calls(), 0);
}

#[tokio::test]
async fn a_users_file_with_no_hyperdx_ro_password_is_refused() {
    let dir = TestDirectory::new();
    for text in [
        "CREATE USER other IDENTIFIED BY 'x';\n",
        "CREATE USER hyperdx_ro IDENTIFIED BY '';\n",
        "",
    ] {
        let users = dir.file("users.sql", text);
        let source = FakeSource::answering(Ok(body()));
        let read = read_factory_flow(&settings(Some(users)), &source).await;
        assert_eq!(read.outcome, FlowOutcome::Refused, "{text:?}");
        assert_eq!(source.calls(), 0);
    }
}

#[tokio::test]
async fn a_users_file_with_crlf_lines_still_gives_the_password() {
    let dir = TestDirectory::new();
    let users = dir.file("users.sql", &users_file_text(CANARY).replace('\n', "\r\n"));
    let source = FakeSource::answering(Ok(body()));
    read_factory_flow(&settings(Some(users)), &source).await;
    assert_eq!(source.seen.lock().unwrap()[0].2, CANARY);
}

#[tokio::test]
async fn each_source_failure_gives_its_word() {
    let dir = TestDirectory::new();
    let users = dir.file("users.sql", &users_file_text(CANARY));
    for (failure, word) in [
        (SourceFailure::Refused, FlowOutcome::Refused),
        (SourceFailure::Unreachable, FlowOutcome::Unreachable),
        (SourceFailure::Timeout, FlowOutcome::Timeout),
    ] {
        let source = FakeSource::answering(Err(failure));
        let read = read_factory_flow(&settings(Some(users.clone())), &source).await;
        assert_eq!(read.outcome, word);
        assert!(read.rows.is_empty());
    }
}

#[tokio::test]
async fn an_answer_that_is_not_the_eight_rows_in_order_is_refused() {
    let dir = TestDirectory::new();
    let users = dir.file("users.sql", &users_file_text(CANARY));
    let swapped = body().replacen("\"stage\":\"idea\"", "\"stage\":\"spec\"", 1);
    let few: String = body()
        .lines()
        .take(7)
        .map(|line| format!("{line}\n"))
        .collect();
    for answer in [
        "Code: 497. DB::Exception: not enough privileges".to_string(),
        swapped,
        few,
        String::new(),
    ] {
        let source = FakeSource::answering(Ok(answer.clone()));
        let read = read_factory_flow(&settings(Some(users.clone())), &source).await;
        assert_eq!(read.outcome, FlowOutcome::Refused, "{answer}");
    }
}

#[tokio::test(start_paused = true)]
async fn no_answer_in_five_seconds_is_a_timeout() {
    let dir = TestDirectory::new();
    let users = dir.file("users.sql", &users_file_text(CANARY));
    let started = tokio::time::Instant::now();
    let read = read_factory_flow(&settings(Some(users)), &Silent).await;
    assert_eq!(read.outcome, FlowOutcome::Timeout);
    assert_eq!(started.elapsed(), Duration::from_secs(5));
    assert_eq!(read.elapsed_ms, 5000);
}

#[tokio::test]
async fn the_password_is_in_no_log_line_and_no_span_for_any_outcome() {
    test_support::install();
    let logs = CapturedLogs::start();
    let dir = TestDirectory::new();
    let users = dir.file("users.sql", &users_file_text(CANARY));
    let answers = [
        Ok(body()),
        Err(SourceFailure::Refused),
        Err(SourceFailure::Unreachable),
        Err(SourceFailure::Timeout),
        Ok(format!("{CANARY} rejected")),
    ];
    for answer in answers {
        read_factory_flow(
            &settings(Some(users.clone())),
            &FakeSource::answering(answer),
        )
        .await;
    }
    read_factory_flow(&settings(None), &FakeSource::answering(Ok(body()))).await;
    read_factory_flow(
        &settings(Some("/none/users.sql".into())),
        &FakeSource::answering(Ok(body())),
    )
    .await;
    let text = logs.text();
    assert!(!text.contains(CANARY), "{text}");
    assert!(!text.contains("hyperdx_ro IDENTIFIED"));
    assert!(!text.contains("SELECT"));
    for outcome in ["ok", "refused", "unreachable", "timeout", "not_set_up"] {
        assert!(
            text.contains("factory flow read") && text.contains(&format!("outcome=\"{outcome}\""))
                || text.contains(&format!("outcome={outcome}")),
            "{outcome}: {text}"
        );
    }
    assert!(text.contains("elapsed_ms="));
    let spans: Vec<_> = test_support::spans()
        .into_iter()
        .filter(|span| span.name == "factory_flow.read")
        .collect();
    assert!(spans.len() >= 7, "{}", spans.len());
    for span in &spans {
        assert!(test_support::span_attr(span, "outcome").is_some());
        assert!(test_support::span_attr(span, "elapsed_ms").is_some());
        for attribute in &span.attributes {
            assert!(!attribute.value.as_str().contains(CANARY));
        }
    }
}

#[tokio::test]
async fn a_read_counts_once_for_its_outcome() {
    test_support::install();
    crate::telemetry::register_factory_flow_counters();
    let before = test_support::exact_counter_value(
        "harold.factory_flow.reads",
        &[("outcome", "not_set_up")],
    )
    .unwrap_or(0);
    read_factory_flow(&settings(None), &FakeSource::answering(Ok(body()))).await;
    let after = test_support::exact_counter_value(
        "harold.factory_flow.reads",
        &[("outcome", "not_set_up")],
    )
    .unwrap();
    assert!(after > before, "{before} {after}");
}

#[test]
fn the_real_request_holds_the_user_the_limits_the_query_and_only_the_configured_url() {
    let request = FlowRequest::new(
        "http://127.0.0.1:8123/".to_string(),
        CANARY.to_string(),
        FACTORY_FLOW_SQL,
    );
    let built = ReqwestSource::build(&reqwest::Client::new(), &request).expect("build");
    assert_eq!(built.method(), reqwest::Method::POST);
    assert_eq!(built.url().host_str(), Some("127.0.0.1"));
    assert_eq!(built.url().port(), Some(8123));
    let query: std::collections::BTreeMap<String, String> = built
        .url()
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    assert_eq!(query.get("readonly").map(String::as_str), Some("1"));
    assert_eq!(
        query.get("max_execution_time").map(String::as_str),
        Some("5")
    );
    assert_eq!(
        query.get("default_format").map(String::as_str),
        Some("JSONEachRow")
    );
    assert!(
        !built.url().as_str().contains(CANARY),
        "the password is not in the URL"
    );
    assert_eq!(
        built.headers().get("x-clickhouse-user").unwrap(),
        "hyperdx_ro"
    );
    let key = built.headers().get("x-clickhouse-key").unwrap();
    assert_eq!(key, CANARY);
    assert!(key.is_sensitive());
    assert_eq!(
        built.body().and_then(|b| b.as_bytes()),
        Some(FACTORY_FLOW_SQL.as_bytes())
    );
    assert_eq!(built.timeout(), Some(&Duration::from_secs(5)));
}

#[test]
fn a_request_does_not_show_its_password_in_debug_output() {
    let request = FlowRequest::new("http://localhost:8123/".into(), CANARY.into(), "SELECT 1");
    assert!(!format!("{request:?}").contains(CANARY));
}

const LEVERS: &str = "---\nname: levers\n---\n\nText before.\n\n| Flow stage | Lever | What it changes | Owner |\n|---|---|---|---|\n| idea | Backlog of five | A free slot always has ranked work | PnEM |\n| spec | Spec quality | Fewer rounds | PnEM |\n| spec | Slicing and sizing | Smaller pieces | PnEM |\n| approval | Standing waiver | Time to start | chief |\n| release | Release speed and tooling | Time to release | chief |\n| installed | Release watch | Faults after install | chief |\n\nText after.\n";

fn lever(name: &str, owner: &str) -> Lever {
    Lever {
        name: name.to_string(),
        owner: owner.to_string(),
    }
}

async fn levers_from(text: &str) -> LeversRead {
    let dir = TestDirectory::new();
    let path = dir.file("levers.md", text);
    read_levers(Some(&path)).await
}

#[tokio::test]
async fn the_levers_table_gives_each_stage_its_levers_in_the_order_of_the_file() {
    let LeversRead::Read { per_stage, unread } = levers_from(LEVERS).await else {
        panic!("the file is read");
    };
    assert_eq!(per_stage.len(), 8);
    assert_eq!(per_stage[0], vec![lever("Backlog of five", "PnEM")]);
    assert_eq!(
        per_stage[1],
        vec![
            lever("Spec quality", "PnEM"),
            lever("Slicing and sizing", "PnEM")
        ]
    );
    assert!(per_stage[2].is_empty());
    assert_eq!(per_stage[3], vec![lever("Standing waiver", "chief")]);
    assert_eq!(
        per_stage[6],
        vec![lever("Release speed and tooling", "chief")]
    );
    assert_eq!(per_stage[7], vec![lever("Release watch", "chief")]);
    assert!(unread.is_empty());
}

#[tokio::test]
async fn a_row_with_an_unknown_flow_stage_goes_in_rows_not_read() {
    let text = LEVERS.replace("| installed | Release watch", "| deploy | Release watch");
    let LeversRead::Read { per_stage, unread } = levers_from(&text).await else {
        panic!("the file is read");
    };
    assert!(per_stage[7].is_empty());
    assert_eq!(unread, vec!["deploy: Release watch".to_string()]);
}

#[tokio::test]
async fn a_short_row_goes_in_rows_not_read() {
    let text = LEVERS.replace(
        "| spec | Slicing and sizing | Smaller pieces | PnEM |",
        "| spec | Slicing and sizing |",
    );
    let LeversRead::Read { per_stage, unread } = levers_from(&text).await else {
        panic!("the file is read");
    };
    assert_eq!(per_stage[1], vec![lever("Spec quality", "PnEM")]);
    assert_eq!(unread, vec!["spec: Slicing and sizing".to_string()]);
}

#[tokio::test]
async fn only_the_first_table_with_the_four_columns_is_read() {
    let other = "| Name | Value |\n|---|---|\n| a | b |\n\n";
    let second = "\n| Flow stage | Lever | What it changes | Owner |\n|---|---|---|---|\n| idea | Second table | x | chief |\n";
    let text = format!("{other}{LEVERS}{second}");
    let LeversRead::Read { per_stage, .. } = levers_from(&text).await else {
        panic!("the file is read");
    };
    assert_eq!(per_stage[0], vec![lever("Backlog of five", "PnEM")]);
}

#[tokio::test]
async fn the_columns_may_come_in_another_order_and_the_stage_in_any_case() {
    let text = "| Owner | Lever | Flow Stage | What it changes |\r\n|---|---|---|---|\r\n| chief | Release watch | Installed | x |\r\n";
    let LeversRead::Read { per_stage, .. } = levers_from(text).await else {
        panic!("the file is read");
    };
    assert_eq!(per_stage[7], vec![lever("Release watch", "chief")]);
}

#[tokio::test]
async fn no_setting_or_no_file_is_no_levers_file() {
    assert!(matches!(read_levers(None).await, LeversRead::NoFile));
    let dir = TestDirectory::new();
    let missing = dir.0.join("none.md").to_string_lossy().into_owned();
    assert!(matches!(
        read_levers(Some(&missing)).await,
        LeversRead::NoFile
    ));
}

#[tokio::test]
async fn a_file_with_no_levers_table_gives_empty_stages() {
    let LeversRead::Read { per_stage, unread } = levers_from("Only text.\n").await else {
        panic!("the file is read");
    };
    assert!(per_stage.iter().all(Vec::is_empty));
    assert!(unread.is_empty());
}
