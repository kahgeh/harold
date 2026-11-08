//! Tests of the factory flow page: the named query, the read, the levers and the page.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Mutex;
use std::time::Duration;

use super::flow::{
    FACTORY_FLOW_SQL, FactoryFlow, FlowOutcome, Lever, LeversRead, format_wait, read_factory_flow,
    read_levers,
};
use super::{AppState, router};
use crate::clickhouse_read::{
    ClickhouseRequest as FlowRequest, ClickhouseSource as FlowSource, ReqwestSource, SourceFailure,
};
use crate::outbound::fake::CapturedLogs;
use crate::settings::FactoryFlowSettings;
use crate::store::HaroldStore;
use crate::telemetry::test_support;
use axum::Router;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::sync::watch;

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
    assert!(
        FACTORY_FLOW_SQL
            .contains("-- Columns: stage, items, median_wait_s, has_data, evidence_missing.")
    );
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
        ("idea", "4", "21600", 1),
        ("spec", "2", "7200", 1),
        ("architect review", "3", "10800", 1),
        ("approval", "7", "68400", 1),
        ("build", "4", "181440.5", 1),
        ("merge", "1", "3600", 1),
        ("release", "17", "7200", 1),
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
        (Some(17.0), Some(7200.0), true)
    );
    let idea = &read.rows[0];
    assert_eq!(
        (idea.items, idea.median_wait_s, idea.has_data),
        (Some(4.0), Some(21600.0), true)
    );
    let installed = &read.rows[7];
    assert!(!installed.has_data && installed.items.is_none());
    // A row with no column `evidence_missing` reads as 0.
    assert!(read.rows.iter().all(|row| row.evidence_missing == 0.0));
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

// ---------------------------------------------------------------------------
// The page. No test opens a port: the router serves one connection over an in-memory pipe.
// ---------------------------------------------------------------------------

const PAGE_LOGIN: &str = "me@example.com";
const LOGIN_LINE: &str = "tailscale-user-login: me@example.com\r\n";

struct OneConnection(Option<DuplexStream>);

impl axum::serve::Listener for OneConnection {
    type Io = DuplexStream;
    type Addr = ();

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.0.take() {
            Some(io) => (io, ()),
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        Ok(())
    }
}

/// Sends one GET and returns the status and the body.
async fn get(app: Router, path: &str, headers: &str) -> (u16, String) {
    let (mut client, server) = tokio::io::duplex(256 * 1024);
    let serving = tokio::spawn(async move {
        let _ = axum::serve(OneConnection(Some(server)), app).await;
    });
    let request =
        format!("GET {path} HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n{headers}\r\n");
    client.write_all(request.as_bytes()).await.unwrap();
    let mut reply = Vec::new();
    client.read_to_end(&mut reply).await.unwrap();
    serving.abort();
    let text = String::from_utf8_lossy(&reply).into_owned();
    let status = text.split(' ').nth(1).unwrap().parse().unwrap();
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    (status, body)
}

async fn page_app(flow: FactoryFlow) -> (Router, PathBuf) {
    let directory =
        std::env::temp_dir().join(format!("harold-factory-page-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let store = Arc::new(HaroldStore::open(directory.join("store")).await.unwrap());
    let (_shutdown, shutdown_rx) = watch::channel(());
    let state = AppState::for_tests(
        store,
        PAGE_LOGIN,
        "BPublicKeyForTheTestPage",
        Arc::from(vec!["localhost".to_owned()]),
        shutdown_rx,
        None,
        Default::default(),
    )
    .with_factory_flow(flow);
    (router(state), directory)
}

/// The page for one answer of the source, with the given levers text.
async fn page_for(answer: Result<String, SourceFailure>, levers: Option<&str>) -> String {
    let dir = TestDirectory::new();
    let mut flow_settings = settings(Some(dir.file("users.sql", &users_file_text(CANARY))));
    flow_settings.levers_file = levers.map(|text| dir.file("levers.md", text));
    let flow = FactoryFlow::new(flow_settings, Arc::new(FakeSource::answering(answer)));
    let (app, directory) = page_app(flow).await;
    let (status, body) = get(app, "/flow", LOGIN_LINE).await;
    let _ = std::fs::remove_dir_all(directory);
    assert_eq!(status, 200, "{body}");
    body
}

fn body_with(rows: &[(&str, &str, &str, u8)]) -> String {
    rows.iter()
        .map(|(stage, items, median, has)| {
            format!("{{\"stage\":\"{stage}\",\"items\":{items},\"median_wait_s\":{median},\"has_data\":{has}}}\n")
        })
        .collect()
}

/// The text of the page from the stage `name` to the end of its list item.
fn stage_block<'a>(page: &'a str, name: &str) -> &'a str {
    let marker = format!("data-stage=\"{name}\"");
    let from = page
        .find(&marker)
        .unwrap_or_else(|| panic!("no stage {name}: {page}"));
    let rest = &page[from + marker.len()..];
    &rest[..rest.find("</li>").unwrap_or(rest.len())]
}

#[test]
fn a_wait_shows_in_hours_below_two_days_and_in_days_from_two_days() {
    assert_eq!(format_wait(68_400.0), "19 h");
    assert_eq!(format_wait(3_600.0), "1 h");
    assert_eq!(format_wait(172_799.0), "48 h");
    assert_eq!(format_wait(172_800.0), "2.0 d");
    assert_eq!(format_wait(181_440.5), "2.1 d");
    assert_eq!(format_wait(0.0), "0 h");
}

/// The eight rows with the column `evidence_missing` of the installed row: 3 specs released
/// and not installed, and a median install lag of 90000 seconds (25 h).
fn rows_with_installed(count: &str) -> String {
    let mut text = body_with(&[
        ("idea", "4", "21600", 1),
        ("spec", "2", "7200", 1),
        ("architect review", "3", "10800", 1),
        ("approval", "7", "68400", 1),
        ("build", "4", "181440.5", 1),
        ("merge", "1", "3600", 1),
        ("release", "17", "7200", 1),
    ]);
    text.push_str(&format!(
        "{{\"stage\":\"installed\",\"items\":3,\"median_wait_s\":90000,\"has_data\":1,\"evidence_missing\":{count}}}\n"
    ));
    text
}

#[tokio::test]
async fn the_installed_stage_shows_the_specs_not_installed_and_the_median_install_lag() {
    let page = page_for(Ok(rows_with_installed("0")), None).await;
    let block = stage_block(&page, "installed");
    assert!(block.contains("3 waiting"), "{block}");
    assert!(block.contains("median 25 h"), "{block}");
    assert!(
        !block.contains("no data yet") && !block.contains("flow-grey"),
        "{block}"
    );
}

#[tokio::test]
async fn the_installed_stage_keeps_the_evidence_line_beside_the_install_numbers() {
    let page = page_for(Ok(rows_with_installed("2")), None).await;
    let block = stage_block(&page, "installed");
    assert!(block.contains("3 waiting"), "{block}");
    assert!(block.contains("2 installed without evidence"), "{block}");
}

#[tokio::test]
async fn the_installed_stage_shows_a_count_of_specs_without_evidence_and_no_line_for_0() {
    let page = page_for(Ok(rows_with_installed("2")), None).await;
    assert!(stage_block(&page, "installed").contains("2 installed without evidence"));
    // The line is one count. Only the installed stage has it.
    assert_eq!(page.matches("installed without evidence").count(), 1);
    let none = page_for(Ok(rows_with_installed("0")), None).await;
    assert!(!none.contains("without evidence"), "{none}");
    let unknown = page_for(Ok(rows_with_installed("null")), None).await;
    assert!(!unknown.contains("without evidence"), "{unknown}");
}

#[tokio::test]
async fn a_count_on_another_stage_row_shows_no_evidence_line() {
    let page = page_for(Ok(body()), None).await;
    assert!(!page.contains("without evidence"), "{page}");
}

#[test]
fn the_query_counts_the_evidence_gauge_of_the_last_10_minutes_by_its_newest_point() {
    for piece in [
        "'pod.evidence.missing'",
        "INTERVAL 10 MINUTE",
        "argMax(Value, TimeUnix)",
    ] {
        assert!(FACTORY_FLOW_SQL.contains(piece), "{piece}");
    }
}

#[tokio::test]
async fn the_page_shows_the_eight_stages_in_order_with_the_numbers_of_the_query() {
    let page = page_for(Ok(body()), Some(LEVERS)).await;
    let mut at = 0;
    for stage in STAGES {
        let found = page[at..]
            .find(&format!("data-stage=\"{stage}\""))
            .unwrap_or_else(|| panic!("{stage}: {page}"));
        at += found + 1;
    }
    assert!(stage_block(&page, "approval").contains("7 waiting"));
    assert!(stage_block(&page, "approval").contains("median 19 h"));
    assert!(stage_block(&page, "spec").contains("2 waiting"));
    assert!(stage_block(&page, "spec").contains("median 2 h"));
    assert!(stage_block(&page, "architect review").contains("3 waiting"));
    assert!(stage_block(&page, "architect review").contains("median 3 h"));
    assert!(stage_block(&page, "build").contains("4 waiting"));
    assert!(stage_block(&page, "build").contains("median 2.1 d"));
    assert!(stage_block(&page, "release").contains("17 waiting"));
    assert!(stage_block(&page, "release").contains("median 2 h"));
    assert!(!stage_block(&page, "release").contains("wait not recorded yet"));
    assert!(stage_block(&page, "merge").contains("1 waiting"));
    assert!(stage_block(&page, "merge").contains("median 1 h"));
    assert!(!stage_block(&page, "merge").contains("no data yet"));
    assert!(stage_block(&page, "idea").contains("4 waiting"));
    assert!(stage_block(&page, "idea").contains("median 6 h"));
    assert!(!stage_block(&page, "idea").contains("no data yet"));
    assert!(stage_block(&page, "installed").contains("no data yet"));
    assert!(stage_block(&page, "installed").contains("flow-grey"));
    for stage in ["idea", "spec", "architect review", "approval"] {
        assert!(!stage_block(&page, stage).contains("flow-grey"), "{stage}");
    }
    assert!(page.contains("<title>Factory flow</title>"));
    assert!(page.contains(r#"data-back="/c""#));
    assert!(page.contains("/static/back-swipe.js"));
    assert!(!page.contains("Rows not read"));
    assert!(
        !page.contains("@post") && !page.contains("<form"),
        "the page has no POST"
    );
}

#[tokio::test]
async fn only_the_stage_with_the_highest_median_is_red() {
    let page = page_for(Ok(body()), None).await;
    assert!(stage_block(&page, "build").contains("flow-red"));
    for stage in STAGES.iter().filter(|stage| **stage != "build") {
        assert!(!stage_block(&page, stage).contains("flow-red"), "{stage}");
    }
}

#[tokio::test]
async fn two_stages_with_the_same_highest_median_are_both_red() {
    let rows = body_with(&[
        ("idea", "null", "null", 0),
        ("spec", "null", "null", 0),
        ("architect review", "null", "null", 0),
        ("approval", "3", "90000", 1),
        ("build", "2", "90000", 1),
        ("merge", "null", "null", 0),
        ("release", "1", "null", 1),
        ("installed", "null", "null", 0),
    ]);
    let page = page_for(Ok(rows), None).await;
    assert!(stage_block(&page, "approval").contains("flow-red"));
    assert!(stage_block(&page, "build").contains("flow-red"));
}

#[tokio::test]
async fn with_no_median_nothing_is_red_and_a_stage_with_zero_items_is_not_no_data() {
    let rows = body_with(&[
        ("idea", "null", "null", 0),
        ("spec", "null", "null", 0),
        ("architect review", "null", "null", 0),
        ("approval", "0", "null", 1),
        ("build", "0", "null", 1),
        ("merge", "null", "null", 0),
        ("release", "0", "null", 1),
        ("installed", "null", "null", 0),
    ]);
    let page = page_for(Ok(rows), None).await;
    assert!(!page.contains("flow-red"), "{page}");
    assert!(stage_block(&page, "approval").contains("0 waiting"));
    assert!(!stage_block(&page, "approval").contains("no data yet"));
}

#[tokio::test]
async fn a_failed_read_shows_one_reason_line_and_no_data_in_every_stage() {
    for (answer, reason) in [
        (Err(SourceFailure::Refused), "ClickHouse refused the read"),
        (
            Err(SourceFailure::Unreachable),
            "ClickHouse does not answer",
        ),
        (
            Err(SourceFailure::Timeout),
            "ClickHouse did not answer in 5 seconds",
        ),
    ] {
        let page = page_for(answer, Some(LEVERS)).await;
        assert_eq!(page.matches("class=\"flow-reason\"").count(), 1, "{page}");
        assert!(page.contains(reason), "{reason}: {page}");
        for stage in STAGES {
            assert!(stage_block(&page, stage).contains("no data yet"), "{stage}");
        }
        assert!(!page.contains("flow-red"));
        // The levers still show.
        assert!(stage_block(&page, "approval").contains("Standing waiver"));
        assert!(!page.contains(CANARY));
    }
}

#[tokio::test]
async fn no_users_file_setting_shows_that_the_read_is_not_set_up() {
    let flow = FactoryFlow::new(settings(None), Arc::new(FakeSource::answering(Ok(body()))));
    let (app, directory) = page_app(flow).await;
    let (status, page) = get(app, "/flow", LOGIN_LINE).await;
    let _ = std::fs::remove_dir_all(directory);
    assert_eq!(status, 200);
    assert!(
        page.contains("The factory flow read is not set up"),
        "{page}"
    );
    assert_eq!(page.matches("class=\"flow-reason\"").count(), 1);
}

#[tokio::test]
async fn the_levers_show_under_their_stage_and_unknown_rows_go_in_rows_not_read() {
    let text = LEVERS.replace("| installed | Release watch", "| deploy | Release watch");
    let page = page_for(Ok(body()), Some(&text)).await;
    let spec = stage_block(&page, "spec");
    assert!(spec.contains("Spec quality") && spec.contains("Slicing and sizing"));
    assert!(spec.contains("PnEM"));
    assert!(stage_block(&page, "release").contains("Release speed and tooling"));
    assert!(page.contains("Rows not read"));
    assert!(page.contains("deploy: Release watch"));
    assert!(!stage_block(&page, "installed").contains("Release watch"));
}

#[tokio::test]
async fn with_no_levers_file_each_stage_says_so() {
    let page = page_for(Ok(body()), None).await;
    for stage in STAGES {
        assert!(
            stage_block(&page, stage).contains("no levers file"),
            "{stage}"
        );
    }
}

#[tokio::test]
async fn markup_in_a_lever_is_escaped_and_the_page_shows_no_spec_id_or_message_text() {
    let text = LEVERS.replace("Spec quality", "<script>alert(1)</script>");
    let page = page_for(Ok(body()), Some(&text)).await;
    assert!(!page.contains("<script>alert(1)"), "{page}");
    assert!(page.contains("&#60;script&#62;alert(1)"));
}

#[tokio::test]
async fn the_page_answers_only_after_the_login_and_device_checks() {
    let flow = FactoryFlow::new(settings(None), Arc::new(FakeSource::answering(Ok(body()))));
    let (app, directory) = page_app(flow).await;
    // No login header: refused, as `/specs` is.
    let (flow_status, _) = get(app.clone(), "/flow", "").await;
    let (specs_status, _) = get(app.clone(), "/specs", "").await;
    assert_eq!(flow_status, specs_status);
    assert_ne!(flow_status, 200);
    // Another login: refused too.
    let (other, _) = get(app, "/flow", "tailscale-user-login: other@example.com\r\n").await;
    assert_ne!(other, 200);
    let _ = std::fs::remove_dir_all(directory);
}

#[tokio::test]
async fn a_load_that_shows_the_stages_counts_one_use_of_factory_flow_page() {
    crate::telemetry::take_noted_work();
    page_for(Ok(body()), None).await;
    assert_eq!(
        crate::telemetry::take_noted_work(),
        vec![crate::telemetry::WorkNoted::FeatureUsed(
            "factory-flow-page".to_owned()
        )]
    );
}

#[tokio::test]
async fn a_load_that_shows_no_stage_counts_no_use() {
    crate::telemetry::take_noted_work();
    page_for(Err(SourceFailure::Unreachable), None).await;
    assert_eq!(crate::telemetry::take_noted_work(), vec![]);
}

#[tokio::test]
async fn a_load_does_not_ask_the_source_for_any_other_url_than_the_configured_one() {
    let dir = TestDirectory::new();
    let mut flow_settings = settings(Some(dir.file("users.sql", &users_file_text(CANARY))));
    flow_settings.clickhouse_url = "http://127.0.0.1:18123/".to_string();
    let source = Arc::new(FakeSource::answering(Ok(body())));
    let flow = FactoryFlow::new(flow_settings, source.clone());
    let (app, directory) = page_app(flow).await;
    let (status, _) = get(app, "/flow", LOGIN_LINE).await;
    let _ = std::fs::remove_dir_all(directory);
    assert_eq!(status, 200);
    let seen = source.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, "http://127.0.0.1:18123/");
}

#[tokio::test]
async fn the_pages_index_has_the_row_factory_flow_after_specs() {
    let flow = FactoryFlow::new(settings(None), Arc::new(FakeSource::answering(Ok(body()))));
    let (app, directory) = page_app(flow).await;
    let (status, page) = get(app, "/c", LOGIN_LINE).await;
    let _ = std::fs::remove_dir_all(directory);
    assert_eq!(status, 200);
    let specs = page.find("href=\"/specs\"").expect("the Specs row");
    let flow = page.find("href=\"/flow\"").expect("the Factory flow row");
    assert!(flow > specs);
    assert!(page[flow..].contains("Factory flow"));
}

// ---------------------------------------------------------------------------
// Port tests: a fake ClickHouse on a loopback port. They run only in the container test
// set and in CI, through `scripts/test-run factory-flow-page <name> --container port-tests`.
// ---------------------------------------------------------------------------

/// What the fake ClickHouse saw: the whole request, as text.
type Seen = Arc<Mutex<Vec<String>>>;

/// A fake ClickHouse that answers each request with `status` and `body`, or never answers
/// when `body` is none. It returns the base URL and what it saw.
async fn fake_clickhouse(status: &'static str, body: Option<String>) -> (String, Seen) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let seen: Seen = Arc::default();
    let record = Arc::clone(&seen);
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let record = Arc::clone(&record);
            let body = body.clone();
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let Ok(read) = stream.read(&mut chunk).await else {
                        return;
                    };
                    if read == 0 {
                        return;
                    }
                    request.extend_from_slice(&chunk[..read]);
                    let text = String::from_utf8_lossy(&request).into_owned();
                    let Some((head, rest)) = text.split_once("\r\n\r\n") else {
                        continue;
                    };
                    let length = head
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .and_then(|n| n.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if rest.len() >= length {
                        record.lock().unwrap().push(text);
                        break;
                    }
                }
                let Some(body) = body else {
                    std::future::pending::<()>().await;
                    return;
                };
                let reply = format!(
                    "HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(reply.as_bytes()).await;
            });
        }
    });
    (url, seen)
}

async fn read_from(url: String) -> super::flow::FlowRead {
    let dir = TestDirectory::new();
    let mut flow_settings = settings(Some(dir.file("users.sql", &users_file_text(CANARY))));
    flow_settings.clickhouse_url = url;
    read_factory_flow(&flow_settings, &ReqwestSource::new()).await
}

#[tokio::test]
#[ignore = "opens a port: runs in the container test set and in CI"]
async fn the_real_source_sends_the_user_the_key_the_limits_and_the_query() {
    let (url, seen) = fake_clickhouse("200 OK", Some(body())).await;
    let read = read_from(url).await;
    assert_eq!(read.outcome, FlowOutcome::Ok);
    assert_eq!(read.rows.len(), 8);
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    let request = seen[0].to_ascii_lowercase();
    assert!(request.starts_with("post /?"), "{request}");
    for piece in [
        "default_format=jsoneachrow",
        "readonly=1",
        "max_execution_time=5",
        "x-clickhouse-user: hyperdx_ro",
    ] {
        assert!(request.contains(piece), "{piece}");
    }
    assert!(request.contains(&format!(
        "x-clickhouse-key: {}",
        CANARY.to_ascii_lowercase()
    )));
    assert!(seen[0].ends_with(FACTORY_FLOW_SQL));
    assert!(
        !seen[0].lines().next().unwrap().contains(CANARY),
        "the password is not in the URL"
    );
}

#[tokio::test]
#[ignore = "opens a port: runs in the container test set and in CI"]
async fn a_refusal_from_clickhouse_is_refused() {
    let (url, _) = fake_clickhouse("403 Forbidden", Some("Code: 497".to_string())).await;
    assert_eq!(read_from(url).await.outcome, FlowOutcome::Refused);
}

#[tokio::test]
#[ignore = "opens a port: runs in the container test set and in CI"]
async fn a_port_with_no_listener_is_unreachable() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    drop(listener);
    assert_eq!(read_from(url).await.outcome, FlowOutcome::Unreachable);
}

#[tokio::test(start_paused = true)]
#[ignore = "opens a port: runs in the container test set and in CI"]
async fn a_server_that_never_answers_is_a_timeout() {
    let (url, _) = fake_clickhouse("200 OK", None).await;
    assert_eq!(read_from(url).await.outcome, FlowOutcome::Timeout);
}

#[test]
fn the_real_client_follows_no_redirect() {
    // A redirect could carry the password to another host. The client's Debug output does not
    // list its proxies, so `no_proxy()` in `ReqwestSource::client` is checked by reading the code.
    let shown = format!("{:?}", ReqwestSource::client().expect("the client builds"));
    assert!(
        shown.contains(r#"redirect_policy: "Policy(None)""#),
        "{shown}"
    );
}

#[tokio::test]
async fn a_client_that_failed_to_build_gives_unreachable_and_sends_nothing() {
    let dir = TestDirectory::new();
    let users = dir.file("users.sql", &users_file_text(CANARY));
    let read = read_factory_flow(&settings(Some(users)), &ReqwestSource::from_client(None)).await;
    assert_eq!(read.outcome, FlowOutcome::Unreachable);
    assert!(read.rows.is_empty());
}
