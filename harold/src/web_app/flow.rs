//! The factory flow page: eight flow stages, each with its items waiting, its median wait
//! and its levers. The numbers come from one named query that Harold reads from ClickHouse.

/// The named query, the same file that `bin/logq --named factory-flow` runs.
pub(crate) const FACTORY_FLOW_SQL: &str =
    include_str!("../../../docs/references/telemetry/queries/factory-flow.sql");

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use tokio::time::Instant;
use tracing::{Instrument, info, warn};

use crate::settings::{FactoryFlowSettings, expand_tilde};
use crate::telemetry::{self, TRACE_TARGET};

/// The user that reads ClickHouse. It may only `SELECT` from `otel.*`.
const CLICKHOUSE_USER: &str = "hyperdx_ro";
/// The limit of the whole read, and of the query inside ClickHouse, in seconds.
const LIMIT_SECONDS: u64 = 5;
/// The flow stages, in the order of the query.
pub(crate) const STAGES: [&str; 8] = [
    "idea",
    "spec",
    "architect review",
    "approval",
    "build",
    "merge",
    "release",
    "installed",
];

/// How a read of the factory flow ended. `as_str` is the `outcome` of the counter, the log
/// line and the span.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FlowOutcome {
    Ok,
    /// No answer.
    Unreachable,
    /// The users file is missing, unreadable or holds no password, or ClickHouse refuses.
    Refused,
    /// The 5-second limit passed.
    Timeout,
    /// No `users_file` setting.
    NotSetUp,
}

impl FlowOutcome {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Unreachable => "unreachable",
            Self::Refused => "refused",
            Self::Timeout => "timeout",
            Self::NotSetUp => "not_set_up",
        }
    }
}

/// How a source failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SourceFailure {
    Unreachable,
    Refused,
    Timeout,
}

/// One flow stage as the query gives it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct StageRow {
    pub stage: String,
    pub items: Option<f64>,
    pub median_wait_s: Option<f64>,
    pub has_data: bool,
}

/// What one read gives. `rows` is empty unless `outcome` is `Ok`.
#[derive(Debug)]
pub(crate) struct FlowRead {
    pub outcome: FlowOutcome,
    pub rows: Vec<StageRow>,
    pub elapsed_ms: u64,
}

/// One query to send. It holds the password until it drops, and `Debug` never shows it.
pub(crate) struct FlowRequest {
    url: String,
    password: String,
    sql: &'static str,
}

impl FlowRequest {
    pub(crate) fn new(url: String, password: String, sql: &'static str) -> Self {
        Self { url, password, sql }
    }

    pub(crate) fn url(&self) -> &str {
        &self.url
    }

    pub(crate) fn user(&self) -> &'static str {
        CLICKHOUSE_USER
    }

    pub(crate) fn password(&self) -> &str {
        &self.password
    }

    pub(crate) fn sql(&self) -> &'static str {
        self.sql
    }
}

impl fmt::Debug for FlowRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FlowRequest")
            .field("url", &self.url)
            .field("password", &"<hidden>")
            .finish_non_exhaustive()
    }
}

/// Where the rows come from: ClickHouse in Harold, a fake in most tests. It answers with the
/// body in `JSONEachRow`.
pub(crate) trait FlowSource: Send + Sync {
    fn query<'a>(
        &'a self,
        request: &'a FlowRequest,
    ) -> Pin<Box<dyn Future<Output = Result<String, SourceFailure>> + Send + 'a>>;
}

/// ClickHouse over HTTP, with `reqwest`.
pub(crate) struct ReqwestSource {
    client: reqwest::Client,
}

impl ReqwestSource {
    pub(crate) fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }

    /// The HTTP request for `request`: the user and the password in headers, the limits and
    /// the format in the query string, and the SQL as the body. The password is not in the URL.
    pub(crate) fn build(
        client: &reqwest::Client,
        request: &FlowRequest,
    ) -> Result<reqwest::Request, reqwest::Error> {
        let mut key = reqwest::header::HeaderValue::from_str(request.password())
            .unwrap_or_else(|_| reqwest::header::HeaderValue::from_static(""));
        key.set_sensitive(true);
        client
            .post(request.url())
            .query(&[
                ("default_format", "JSONEachRow"),
                ("readonly", "1"),
                ("max_execution_time", "5"),
            ])
            .header("X-ClickHouse-User", request.user())
            .header("X-ClickHouse-Key", key)
            .timeout(Duration::from_secs(LIMIT_SECONDS))
            .body(request.sql())
            .build()
    }
}

impl FlowSource for ReqwestSource {
    fn query<'a>(
        &'a self,
        request: &'a FlowRequest,
    ) -> Pin<Box<dyn Future<Output = Result<String, SourceFailure>> + Send + 'a>> {
        Box::pin(async move {
            let built = Self::build(&self.client, request).map_err(|_| SourceFailure::Refused)?;
            let response = self.client.execute(built).await.map_err(failure_of)?;
            if !response.status().is_success() {
                return Err(SourceFailure::Refused);
            }
            response.text().await.map_err(failure_of)
        })
    }
}

fn failure_of(error: reqwest::Error) -> SourceFailure {
    if error.is_timeout() {
        SourceFailure::Timeout
    } else {
        SourceFailure::Unreachable
    }
}

/// The `hyperdx_ro` password in the users file, by the rule of `bin/logq`: the text between the
/// quotes after `hyperdx_ro IDENTIFIED BY '`. None when it is missing or empty.
fn password_in(text: &str) -> Option<String> {
    const MARK: &str = "hyperdx_ro IDENTIFIED BY '";
    text.lines().find_map(|line| {
        let rest = &line[line.find(MARK)? + MARK.len()..];
        let password = &rest[..rest.find('\'')?];
        (!password.is_empty()).then(|| password.to_string())
    })
}

/// The eight rows of `body`, or none when it is not exactly the eight flow stages in order.
fn rows_in(body: &str) -> Option<Vec<StageRow>> {
    let rows: Vec<StageRow> = body
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let value: serde_json::Value = serde_json::from_str(line).ok()?;
            Some(StageRow {
                stage: value.get("stage")?.as_str()?.to_string(),
                items: number(value.get("items")?),
                median_wait_s: number(value.get("median_wait_s")?),
                has_data: number(value.get("has_data")?)? > 0.0,
            })
        })
        .collect::<Option<_>>()?;
    let in_order = rows.len() == STAGES.len()
        && rows
            .iter()
            .zip(STAGES)
            .all(|(row, stage)| row.stage == stage);
    in_order.then_some(rows)
}

/// A JSON number, or a number in a string, as ClickHouse prints a 64-bit value.
fn number(value: &serde_json::Value) -> Option<f64> {
    match value {
        serde_json::Value::Number(number) => number.as_f64(),
        serde_json::Value::String(text) => text.parse().ok(),
        _ => None,
    }
}

/// Reads the factory flow once. It reads the password at this call, drops it at the end,
/// counts the outcome, and logs `factory flow read` with the `outcome` and `elapsed_ms` only.
pub(crate) async fn read_factory_flow(
    settings: &FactoryFlowSettings,
    source: &dyn FlowSource,
) -> FlowRead {
    let span = tracing::info_span!(
        target: TRACE_TARGET,
        "factory_flow.read",
        outcome = tracing::field::Empty,
        elapsed_ms = tracing::field::Empty,
    );
    let started = Instant::now();
    let (outcome, rows) = attempt(settings, source).instrument(span.clone()).await;
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let word = outcome.as_str();
    span.record("outcome", word);
    span.record("elapsed_ms", elapsed_ms);
    telemetry::count_factory_flow_read(word);
    let _inside = span.enter();
    if outcome == FlowOutcome::Ok {
        info!(outcome = word, elapsed_ms, "factory flow read");
    } else {
        warn!(outcome = word, elapsed_ms, "factory flow read");
    }
    FlowRead {
        outcome,
        rows,
        elapsed_ms,
    }
}

async fn attempt(
    settings: &FactoryFlowSettings,
    source: &dyn FlowSource,
) -> (FlowOutcome, Vec<StageRow>) {
    let Some(users_file) = settings.users_file.as_deref() else {
        return (FlowOutcome::NotSetUp, Vec::new());
    };
    let Ok(text) = tokio::fs::read_to_string(expand_tilde(users_file)).await else {
        return (FlowOutcome::Refused, Vec::new());
    };
    let Some(password) = password_in(&text) else {
        return (FlowOutcome::Refused, Vec::new());
    };
    drop(text);
    let request = FlowRequest::new(settings.clickhouse_url.clone(), password, FACTORY_FLOW_SQL);
    let limit = Duration::from_secs(LIMIT_SECONDS);
    let answer = match tokio::time::timeout(limit, source.query(&request)).await {
        Err(_) => return (FlowOutcome::Timeout, Vec::new()),
        Ok(answer) => answer,
    };
    drop(request);
    match answer {
        Err(SourceFailure::Unreachable) => (FlowOutcome::Unreachable, Vec::new()),
        Err(SourceFailure::Refused) => (FlowOutcome::Refused, Vec::new()),
        Err(SourceFailure::Timeout) => (FlowOutcome::Timeout, Vec::new()),
        Ok(body) => match rows_in(&body) {
            Some(rows) => (FlowOutcome::Ok, rows),
            None => (FlowOutcome::Refused, Vec::new()),
        },
    }
}

/// One lever of a flow stage, as `levers.md` lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Lever {
    pub name: String,
    pub owner: String,
}

/// What the levers file gave.
#[derive(Debug)]
pub(crate) enum LeversRead {
    /// No setting, or the file cannot be read.
    NoFile,
    /// `per_stage` has one list for each flow stage, in the order of `STAGES`. `unread` names
    /// each row that no stage owns, as `<flow stage>: <lever>`.
    Read {
        per_stage: Vec<Vec<Lever>>,
        unread: Vec<String>,
    },
}

/// Reads `levers_file` (read only) at each call.
pub(crate) async fn read_levers(path: Option<&str>) -> LeversRead {
    let Some(path) = path else {
        return LeversRead::NoFile;
    };
    match tokio::fs::read_to_string(expand_tilde(path)).await {
        Ok(text) => parse_levers(&text),
        Err(_) => LeversRead::NoFile,
    }
}

/// The cells of one table line, trimmed, without the outer bars.
fn cells(line: &str) -> Vec<&str> {
    line.trim()
        .trim_start_matches('|')
        .trim_end_matches('|')
        .split('|')
        .map(str::trim)
        .collect()
}

/// Reads the first table whose columns include `Flow stage`, `Lever`, `What it changes` and
/// `Owner`, in any order and any case.
fn parse_levers(text: &str) -> LeversRead {
    let mut per_stage: Vec<Vec<Lever>> = vec![Vec::new(); STAGES.len()];
    let mut unread = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    let column = |header: &[&str], name: &str| {
        header
            .iter()
            .position(|cell| cell.eq_ignore_ascii_case(name))
    };
    for (at, line) in lines.iter().enumerate() {
        if !line.trim_start().starts_with('|') {
            continue;
        }
        let header = cells(line);
        let (Some(stage), Some(lever), Some(_), Some(owner)) = (
            column(&header, "Flow stage"),
            column(&header, "Lever"),
            column(&header, "What it changes"),
            column(&header, "Owner"),
        ) else {
            continue;
        };
        let rows = lines[at + 1..]
            .iter()
            .skip(1)
            .take_while(|row| row.trim_start().starts_with('|'));
        for row in rows {
            let row = cells(row);
            let (Some(stage_cell), Some(lever_cell), Some(owner_cell)) =
                (row.get(stage), row.get(lever), row.get(owner))
            else {
                let name = row.first().copied().unwrap_or_default();
                unread.push(format!(
                    "{name}: {}",
                    row.get(1).copied().unwrap_or_default()
                ));
                continue;
            };
            match STAGES
                .iter()
                .position(|known| known.eq_ignore_ascii_case(stage_cell))
            {
                Some(place) => per_stage[place].push(Lever {
                    name: (*lever_cell).to_string(),
                    owner: (*owner_cell).to_string(),
                }),
                None => unread.push(format!("{stage_cell}: {lever_cell}")),
            }
        }
        break;
    }
    LeversRead::Read { per_stage, unread }
}

// ---------------------------------------------------------------------------
// The page
// ---------------------------------------------------------------------------

use std::sync::Arc;

use askama::Template;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};

use super::AppState;

/// The factory flow page's settings and its source of rows.
pub(crate) struct FactoryFlow {
    settings: FactoryFlowSettings,
    source: Arc<dyn FlowSource>,
}

impl FactoryFlow {
    pub(crate) fn new(settings: FactoryFlowSettings, source: Arc<dyn FlowSource>) -> Self {
        Self { settings, source }
    }

    /// The page as Harold runs it: rows from ClickHouse over HTTP.
    pub(crate) fn from_settings(settings: &FactoryFlowSettings) -> Self {
        Self::new(settings.clone(), Arc::new(ReqwestSource::new()))
    }
}

/// A wait of `seconds`, in hours below 2 days, else in days with one decimal.
pub(crate) fn format_wait(seconds: f64) -> String {
    const TWO_DAYS: f64 = 172_800.0;
    if seconds < TWO_DAYS {
        format!("{:.0} h", seconds / 3_600.0)
    } else {
        format!("{:.1} d", seconds / 86_400.0)
    }
}

struct StageView {
    name: &'static str,
    /// `flow-red`, `flow-grey` or empty.
    class: &'static str,
    summary: String,
    levers: Vec<Lever>,
    /// Shown when the stage has no levers: "no levers file" or "no levers".
    levers_note: &'static str,
}

#[derive(Template)]
#[template(path = "web_app/flow.html")]
struct FlowPage {
    app_name: String,
    has_icon: bool,
    /// The one line that tells why the numbers are missing; empty when the read worked.
    reason: &'static str,
    stages: Vec<StageView>,
    unread: Vec<String>,
}

fn reason_of(outcome: FlowOutcome) -> &'static str {
    match outcome {
        FlowOutcome::Ok => "",
        FlowOutcome::NotSetUp => "The factory flow read is not set up",
        FlowOutcome::Refused => "ClickHouse refused the read, or the users file has no password",
        FlowOutcome::Unreachable => "ClickHouse does not answer",
        FlowOutcome::Timeout => "ClickHouse did not answer in 5 seconds",
    }
}

fn summary_of(row: &StageRow) -> String {
    if !row.has_data {
        return "no data yet".to_string();
    }
    let waiting = format!("{} waiting", row.items.unwrap_or(0.0).round() as i64);
    match row.median_wait_s {
        Some(seconds) => format!("{waiting} \u{b7} median {}", format_wait(seconds)),
        None => format!("{waiting} \u{b7} wait not recorded yet"),
    }
}

fn build_view(read: &FlowRead, levers: LeversRead) -> (Vec<StageView>, Vec<String>) {
    let highest = read
        .rows
        .iter()
        .filter_map(|row| row.median_wait_s)
        .fold(None, |highest: Option<f64>, wait| {
            Some(highest.map_or(wait, |known| known.max(wait)))
        });
    let (mut per_stage, unread, note) = match levers {
        LeversRead::NoFile => (vec![Vec::new(); STAGES.len()], Vec::new(), "no levers file"),
        LeversRead::Read { per_stage, unread } => (per_stage, unread, "no levers"),
    };
    let stages = STAGES
        .iter()
        .enumerate()
        .map(|(at, name)| {
            let row = read.rows.get(at);
            let red = row.is_some_and(|row| highest.is_some() && row.median_wait_s == highest);
            let grey = row.is_none_or(|row| !row.has_data);
            StageView {
                name,
                class: if red {
                    "flow-red"
                } else if grey {
                    "flow-grey"
                } else {
                    ""
                },
                summary: row.map_or_else(|| "no data yet".to_string(), summary_of),
                levers: std::mem::take(&mut per_stage[at]),
                levers_note: note,
            }
        })
        .collect();
    (stages, unread)
}

/// `GET /flow`: the eight flow stages. It reads again at each load and has no POST.
pub(crate) async fn get_flow(State(state): State<AppState>) -> Response {
    let flow = &state.factory_flow;
    let read = read_factory_flow(&flow.settings, flow.source.as_ref()).await;
    let levers = read_levers(flow.settings.levers_file.as_deref()).await;
    let (stages, unread) = build_view(&read, levers);
    let page = FlowPage {
        app_name: state.identity.name.clone(),
        has_icon: state.identity.icon.is_some(),
        reason: reason_of(read.outcome),
        stages,
        unread,
    };
    match page.render() {
        Ok(html) => Html(html).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}
