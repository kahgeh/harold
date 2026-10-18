use std::time::Duration;

use crate::agent::runtime::valid_pane_id;
use crate::away_message::{
    AGENT_NOT_FOUND, INVALID_MESSAGE_LENGTH, INVALID_SOURCE_ID, MESSAGING_PAUSED,
};
use crate::harold::{
    AgentState, ReportAgentStateRequest, SendMessageToAwayDeviceRequest, SpeakLineRequest,
    WatchAgentStatesRequest, harold_client::HaroldClient,
};
use crate::settings::Settings;
use harold_api::tools::backlog::v1 as backlog_api;
use harold_api::tools::backlog::v1::backlog_service_client::BacklogServiceClient;
use harold_api::tools::questions::v1 as questions;
use harold_api::tools::questions::v1::question_service_client::QuestionServiceClient;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Daemon,
    Help,
    Diagnostics { delay_seconds: u64 },
    CheckConfig,
    CheckReady,
    Accept(AcceptArgs),
    SendMessageToAwayDevice(AwayMessageArgs),
    Speak(AwayMessageArgs),
    Tool(ToolCommand),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AcceptEvent {
    TurnStarted,
    StillWorking,
    TurnCompleted,
    SubagentsFinished,
}

impl AcceptEvent {
    fn from_name(name: &str) -> Option<Self> {
        match name {
            "turn-started" => Some(Self::TurnStarted),
            "still-working" => Some(Self::StillWorking),
            "turn-completed" => Some(Self::TurnCompleted),
            "subagents-finished" => Some(Self::SubagentsFinished),
            _ => None,
        }
    }

    pub(crate) fn state(self) -> AgentState {
        match self {
            Self::TurnStarted | Self::StillWorking => AgentState::Busy,
            Self::TurnCompleted | Self::SubagentsFinished => AgentState::Idle,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct AcceptArgs {
    pub(crate) event: AcceptEvent,
    pub(crate) pane: String,
    pub(crate) adapter: String,
}

const ACCEPT_ARGUMENTS: &str = "invalid argument: usage is accept turn-started|still-working|turn-completed|subagents-finished --pane ID --adapter NAME";

fn parse_accept(args: &[&str]) -> Result<AcceptArgs, String> {
    let [event, flags @ ..] = args else {
        return Err(ACCEPT_ARGUMENTS.into());
    };
    let Some(event) = AcceptEvent::from_name(event) else {
        return Err(ACCEPT_ARGUMENTS.into());
    };
    let (mut pane, mut adapter) = (None, None);
    let mut rest = flags.iter();
    while let Some(flag) = rest.next() {
        let slot = match *flag {
            "--pane" => &mut pane,
            "--adapter" => &mut adapter,
            _ => return Err(ACCEPT_ARGUMENTS.into()),
        };
        match rest.next() {
            Some(value) if slot.is_none() && !value.starts_with("--") => {
                *slot = Some(value.to_string())
            }
            _ => return Err(ACCEPT_ARGUMENTS.into()),
        }
    }
    match (pane, adapter) {
        (Some(pane), Some(adapter)) => Ok(AcceptArgs {
            event,
            pane,
            adapter,
        }),
        _ => Err(ACCEPT_ARGUMENTS.into()),
    }
}

pub(crate) const AWAY_MESSAGE_COMMAND: &str = "send-message-to-away-device";

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct AwayMessageArgs {
    pub(crate) src_id: String,
    pub(crate) text: String,
}

const AWAY_MESSAGE_ARGUMENTS: &str =
    "invalid argument: usage is send-message-to-away-device --src-id ID TEXT";
const SOURCE_ID_LINE: &str = "--src-id must be a tmux pane id such as %47";
const MESSAGE_LENGTH_LINE: &str = "the message must be 1 to 1,000 characters";
const NOT_STORED_LINE: &str = "the Harold daemon could not store the message";

pub(crate) const SPEAK_COMMAND: &str = "speak";
pub(crate) const TOOL_COMMAND: &str = "tool";
const TOOL_USAGE_LINE: &str =
    "invalid argument: usage is harold tool questions put|dismiss|order|list";
const SPEAK_ARGUMENTS: &str = "invalid argument: usage is speak --src-id ID TEXT";
const SPOKEN_LENGTH_LINE: &str = "the spoken line must be 1 to 300 characters";
const SPOKEN_NOT_STORED_LINE: &str = "the Harold daemon could not store the spoken line";

/// The arguments after the command name: one `--src-id ID` and one text, in either order.
/// `Err` is the one line the command prints: `usage` for a malformed command,
/// `length_line` for an empty text. An argument that starts with `--` is a flag, so a text
/// cannot start with `--`.
fn parse_source_and_text(
    args: &[&str],
    usage: &str,
    length_line: &str,
) -> Result<AwayMessageArgs, String> {
    let (mut src_id, mut text) = (None, None);
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        if *arg == "--src-id" {
            match rest.next() {
                Some(value)
                    if src_id.is_none() && !value.is_empty() && !value.starts_with("--") =>
                {
                    src_id = Some(*value)
                }
                _ => return Err(usage.into()),
            }
        } else if arg.starts_with("--") || text.is_some() {
            return Err(usage.into());
        } else {
            text = Some(*arg);
        }
    }
    let (Some(src_id), Some(text)) = (src_id, text) else {
        return Err(usage.into());
    };
    if !valid_pane_id(src_id) {
        return Err(SOURCE_ID_LINE.into());
    }
    // The daemon alone makes the text one line and checks its length.
    if text.trim().is_empty() {
        return Err(length_line.into());
    }
    Ok(AwayMessageArgs {
        src_id: src_id.to_string(),
        text: text.to_string(),
    })
}

fn parse_away_message(args: &[&str]) -> Result<AwayMessageArgs, String> {
    parse_source_and_text(args, AWAY_MESSAGE_ARGUMENTS, MESSAGE_LENGTH_LINE)
}

/// The line the command prints for a refusal with this gRPC code and status message.
fn refusal_line(code: tonic::Code, message: &str, src_id: &str) -> String {
    match (code, message) {
        (tonic::Code::InvalidArgument, INVALID_SOURCE_ID) => SOURCE_ID_LINE.into(),
        (tonic::Code::InvalidArgument, INVALID_MESSAGE_LENGTH) => MESSAGE_LENGTH_LINE.into(),
        (tonic::Code::FailedPrecondition, MESSAGING_PAUSED) => "messaging is paused".into(),
        (tonic::Code::FailedPrecondition, AGENT_NOT_FOUND) => {
            format!("no agent is running in pane {src_id}")
        }
        // Includes an older daemon that does not have the RPC.
        _ => NOT_STORED_LINE.into(),
    }
}

/// The line `harold speak` prints for a refusal with this gRPC code and status message.
fn speak_refusal_line(code: tonic::Code, message: &str, src_id: &str) -> String {
    match (code, message) {
        (tonic::Code::InvalidArgument, INVALID_SOURCE_ID) => SOURCE_ID_LINE.into(),
        (tonic::Code::InvalidArgument, INVALID_MESSAGE_LENGTH) => SPOKEN_LENGTH_LINE.into(),
        (tonic::Code::FailedPrecondition, AGENT_NOT_FOUND) => {
            format!("no agent is running in pane {src_id}")
        }
        // Includes an older daemon that does not have the RPC.
        _ => SPOKEN_NOT_STORED_LINE.into(),
    }
}

/// A `harold tool questions` command. `put` reads the question as JSON from stdin.
// The variant names are the planned interface; the group will gain other tools.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ToolCommand {
    QuestionsPut,
    QuestionsDismiss { id: String, reason: String },
    QuestionsOrder { ids: Vec<String> },
    QuestionsList,
    Backlog(BacklogCommand),
}

const BACKLOG_USAGE_LINE: &str = "invalid argument: usage is harold tool backlog put|rank|remove|list|note|notes|queue|import|export";

/// A `harold tool backlog` command. `put`, `note add` and `note update` read JSON from
/// stdin; `queue add`, `queue update` and `queue note` read plain text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BacklogCommand {
    Put {
        product: String,
    },
    Rank {
        product: String,
        ids: Vec<String>,
    },
    Remove {
        product: String,
        id: String,
        reason: Option<String>,
    },
    List {
        product: String,
        json: bool,
    },
    NoteAdd {
        product: String,
        kind: String,
    },
    NoteUpdate {
        product: String,
        id: i64,
    },
    NoteRemove {
        product: String,
        id: i64,
    },
    Notes {
        product: String,
        kind: Option<String>,
        last: Option<u32>,
    },
    QueueAdd {
        place: QueuePlace,
    },
    QueueNow {
        id: String,
    },
    QueueOrder {
        ids: Vec<String>,
    },
    QueueUpdate {
        id: String,
    },
    QueueDone {
        id: String,
    },
    QueueNote,
    QueueList {
        json: bool,
        done_since: Option<time::Date>,
    },
    Import {
        product: String,
        file: std::path::PathBuf,
        apply: bool,
    },
    QueueImport {
        file: std::path::PathBuf,
        apply: bool,
    },
    Export {
        product: String,
    },
    QueueExport,
}

/// Where `queue add` puts the new item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QueuePlace {
    End,
    Now,
    At(u32),
}

impl BacklogCommand {
    /// How long the command waits for the daemon: import and export move a whole file.
    pub(crate) fn deadline(&self) -> Duration {
        match self {
            Self::Import { .. }
            | Self::QueueImport { .. }
            | Self::Export { .. }
            | Self::QueueExport => BULK_DEADLINE,
            _ => CALL_DEADLINE,
        }
    }
}

/// A positional argument: anything that is not a flag.
fn word(arg: &str) -> Result<String, String> {
    match arg.starts_with("--") {
        true => Err(BACKLOG_USAGE_LINE.into()),
        false => Ok(arg.into()),
    }
}

fn words(args: &[&str]) -> Result<Vec<String>, String> {
    match args.is_empty() {
        true => Err(BACKLOG_USAGE_LINE.into()),
        false => args.iter().map(|arg| word(arg)).collect(),
    }
}

fn number<T: std::str::FromStr>(arg: &str) -> Result<T, String> {
    arg.parse().map_err(|_| BACKLOG_USAGE_LINE.to_string())
}

/// A `YYYY-MM-DD` date.
fn date(arg: &str) -> Result<time::Date, String> {
    let usage = || BACKLOG_USAGE_LINE.to_string();
    let mut parts = arg.splitn(3, '-');
    let (Some(year), Some(month), Some(day)) = (parts.next(), parts.next(), parts.next()) else {
        return Err(usage());
    };
    if year.len() != 4 || month.len() != 2 || day.len() != 2 {
        return Err(usage());
    }
    let month = time::Month::try_from(number::<u8>(month)?).map_err(|_| usage())?;
    time::Date::from_calendar_date(number(year)?, month, number(day)?).map_err(|_| usage())
}

/// The flags after a command: each named flag at most once, `valued` ones with a value.
fn flags<'a>(
    args: &[&'a str],
    switches: &[&str],
    valued: &[&str],
) -> Result<std::collections::HashMap<&'a str, Option<&'a str>>, String> {
    let mut found = std::collections::HashMap::new();
    let mut rest = args.iter();
    while let Some(flag) = rest.next() {
        let value = if switches.contains(flag) {
            None
        } else if valued.contains(flag) {
            match rest.next() {
                Some(value) if !value.starts_with("--") => Some(*value),
                _ => return Err(BACKLOG_USAGE_LINE.into()),
            }
        } else {
            return Err(BACKLOG_USAGE_LINE.into());
        };
        if found.insert(*flag, value).is_some() {
            return Err(BACKLOG_USAGE_LINE.into());
        }
    }
    Ok(found)
}

fn parse_backlog(args: &[&str]) -> Result<BacklogCommand, String> {
    use BacklogCommand as C;
    match args {
        ["put", product] => Ok(C::Put {
            product: word(product)?,
        }),
        ["rank", product, ids @ ..] => Ok(C::Rank {
            product: word(product)?,
            ids: words(ids)?,
        }),
        ["remove", product, id, rest @ ..] => {
            let found = flags(rest, &[], &["--reason"])?;
            Ok(C::Remove {
                product: word(product)?,
                id: word(id)?,
                reason: found.get("--reason").copied().flatten().map(String::from),
            })
        }
        ["list", product, rest @ ..] => Ok(C::List {
            product: word(product)?,
            json: flags(rest, &["--json"], &[])?.contains_key("--json"),
        }),
        ["note", "add", product, kind] => Ok(C::NoteAdd {
            product: word(product)?,
            kind: word(kind)?,
        }),
        ["note", "update", product, id] => Ok(C::NoteUpdate {
            product: word(product)?,
            id: number(id)?,
        }),
        ["note", "remove", product, id] => Ok(C::NoteRemove {
            product: word(product)?,
            id: number(id)?,
        }),
        ["notes", product, rest @ ..] => {
            let found = flags(rest, &[], &["--kind", "--last"])?;
            Ok(C::Notes {
                product: word(product)?,
                kind: found.get("--kind").copied().flatten().map(String::from),
                last: found
                    .get("--last")
                    .copied()
                    .flatten()
                    .map(number)
                    .transpose()?,
            })
        }
        ["queue", "add", rest @ ..] => {
            let found = flags(rest, &["--now"], &["--at"])?;
            let place = match (
                found.contains_key("--now"),
                found.get("--at").copied().flatten(),
            ) {
                (false, None) => QueuePlace::End,
                (true, None) => QueuePlace::Now,
                (false, Some(at)) => match number(at)? {
                    0 => return Err(BACKLOG_USAGE_LINE.into()),
                    at => QueuePlace::At(at),
                },
                (true, Some(_)) => return Err(BACKLOG_USAGE_LINE.into()),
            };
            Ok(C::QueueAdd { place })
        }
        ["queue", "now", id] => Ok(C::QueueNow { id: word(id)? }),
        ["queue", "order", ids @ ..] => Ok(C::QueueOrder { ids: words(ids)? }),
        ["queue", "update", id] => Ok(C::QueueUpdate { id: word(id)? }),
        ["queue", "done", id] => Ok(C::QueueDone { id: word(id)? }),
        ["queue", "note"] => Ok(C::QueueNote),
        ["queue", "list", rest @ ..] => {
            let found = flags(rest, &["--json"], &["--done-since"])?;
            Ok(C::QueueList {
                json: found.contains_key("--json"),
                done_since: found
                    .get("--done-since")
                    .copied()
                    .flatten()
                    .map(date)
                    .transpose()?,
            })
        }
        ["queue", "import", file, rest @ ..] => Ok(C::QueueImport {
            file: word(file)?.into(),
            apply: flags(rest, &["--apply"], &[])?.contains_key("--apply"),
        }),
        ["queue", "export"] => Ok(C::QueueExport),
        ["import", product, file, rest @ ..] => Ok(C::Import {
            product: word(product)?,
            file: word(file)?.into(),
            apply: flags(rest, &["--apply"], &[])?.contains_key("--apply"),
        }),
        ["export", product] => Ok(C::Export {
            product: word(product)?,
        }),
        _ => Err(BACKLOG_USAGE_LINE.into()),
    }
}

fn parse_tool(args: &[&str]) -> Result<ToolCommand, String> {
    match args {
        ["backlog", rest @ ..] => parse_backlog(rest).map(ToolCommand::Backlog),
        ["questions", "put"] => Ok(ToolCommand::QuestionsPut),
        ["questions", "list"] => Ok(ToolCommand::QuestionsList),
        ["questions", "dismiss", id, "--reason", reason] => Ok(ToolCommand::QuestionsDismiss {
            id: (*id).into(),
            reason: (*reason).into(),
        }),
        ["questions", "order", ids @ ..] if !ids.is_empty() => Ok(ToolCommand::QuestionsOrder {
            ids: ids.iter().map(|id| (*id).into()).collect(),
        }),
        _ => Err(TOOL_USAGE_LINE.into()),
    }
}

impl Mode {
    pub(crate) fn parse(args: &[String]) -> Result<Self, String> {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        match args.as_slice() {
            [] => Ok(Self::Daemon),
            ["--help" | "-h"] => Ok(Self::Help),
            ["--check-config"] => Ok(Self::CheckConfig),
            ["--check-ready"] => Ok(Self::CheckReady),
            ["accept", rest @ ..] => parse_accept(rest).map(Self::Accept),
            [AWAY_MESSAGE_COMMAND, rest @ ..] => {
                parse_away_message(rest).map(Self::SendMessageToAwayDevice)
            }
            [SPEAK_COMMAND, rest @ ..] => {
                parse_source_and_text(rest, SPEAK_ARGUMENTS, SPOKEN_LENGTH_LINE).map(Self::Speak)
            }
            [TOOL_COMMAND, rest @ ..] => parse_tool(rest).map(Self::Tool),
            ["--diagnostic" | "--diagnostics"] => Ok(Self::Diagnostics { delay_seconds: 0 }),
            ["--diagnostic" | "--diagnostics", "--delay"] => {
                Ok(Self::Diagnostics { delay_seconds: 10 })
            }
            ["--diagnostic" | "--diagnostics", "--delay", seconds] => {
                let delay_seconds = seconds
                    .parse()
                    .map_err(|_| "invalid argument: --delay requires nonnegative seconds")?;
                Ok(Self::Diagnostics { delay_seconds })
            }
            _ => Err("invalid or conflicting arguments; see harold --help".into()),
        }
    }
}

pub(crate) fn check_config(settings: &Settings) -> Result<(), Box<dyn std::error::Error>> {
    println!(
        "{}",
        serde_json::json!({
            "grpc_addr": settings.grpc.addr()?.to_string(),
            "store_path": settings.store.resolved_path(),
        })
    );
    Ok(())
}

pub(crate) async fn check_ready(settings: &Settings) -> Result<(), Box<dyn std::error::Error>> {
    let address = settings.grpc.addr()?;
    let snapshot = tokio::time::timeout(Duration::from_secs(5), async {
        let mut client = HaroldClient::connect(format!("http://{address}")).await?;
        let mut stream = client
            .watch_agent_states(WatchAgentStatesRequest {})
            .await?
            .into_inner();
        stream
            .message()
            .await?
            .ok_or_else(|| -> Box<dyn std::error::Error> {
                "readiness stream ended before its initial snapshot".into()
            })
    })
    .await
    .map_err(|_| "readiness check timed out after 5 seconds")??;
    println!(
        "{}",
        serde_json::json!({
            "ready": true,
            "through_event_version": snapshot.through_event_version,
        })
    );
    Ok(())
}

pub(crate) const CALL_DEADLINE: Duration = Duration::from_secs(3);
/// The deadline of a backlog import or export, which moves a whole file in one call.
pub(crate) const BULK_DEADLINE: Duration = Duration::from_secs(30);

/// `deadline` is what the timeout line names, `wait` is how long it really waits; a command passes the same value for both.
pub(crate) async fn call_within<T, E: From<String>>(
    deadline: Duration,
    wait: Duration,
    call: impl std::future::Future<Output = Result<T, E>>,
) -> Result<T, E> {
    tokio::time::timeout(wait, call).await.map_err(|_| {
        E::from(format!(
            "the Harold daemon did not answer within {} seconds",
            deadline.as_secs()
        ))
    })?
}

pub(crate) async fn accept(
    settings: &Settings,
    args: &AcceptArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    accept_within(settings, args, CALL_DEADLINE).await
}

pub(crate) async fn accept_within(
    settings: &Settings,
    args: &AcceptArgs,
    wait: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    let address = settings.grpc.addr()?;
    let state = args.event.state();
    let response = call_within(CALL_DEADLINE, wait, async {
        let mut client = HaroldClient::connect(format!("http://{address}"))
            .await
            .map_err(|_| "the Harold daemon is not reachable")?;
        client
            .report_agent_state(ReportAgentStateRequest {
                pane_id: args.pane.clone(),
                state: state as i32,
                adapter_id: args.adapter.clone(),
                work_summary: None,
            })
            .await
            .map_err(|_| Box::<dyn std::error::Error>::from("the Harold daemon refused the call"))
    })
    .await?;
    if !response.into_inner().accepted {
        return Err("the Harold daemon did not accept the report".into());
    }
    Ok(())
}

/// Ask the daemon to send `args.text` to the away device. `Ok` means the daemon stored the
/// message for sending, not that the phone has it. `Err` is the one line to print.
pub(crate) async fn send_message_to_away_device(
    settings: &Settings,
    args: &AwayMessageArgs,
) -> Result<(), String> {
    send_message_to_away_device_within(settings, args, CALL_DEADLINE).await
}

pub(crate) async fn send_message_to_away_device_within(
    settings: &Settings,
    args: &AwayMessageArgs,
    wait: Duration,
) -> Result<(), String> {
    let address = settings
        .grpc
        .addr()
        .map_err(|_| "invalid configuration".to_string())?;
    let response = call_within(CALL_DEADLINE, wait, async {
        let mut client = HaroldClient::connect(format!("http://{address}"))
            .await
            .map_err(|_| "the Harold daemon is not reachable".to_string())?;
        client
            .send_message_to_away_device(SendMessageToAwayDeviceRequest {
                src_id: args.src_id.clone(),
                text: args.text.clone(),
            })
            .await
            .map_err(|status| refusal_line(status.code(), status.message(), &args.src_id))
    })
    .await?;
    if !response.into_inner().accepted {
        return Err(NOT_STORED_LINE.into());
    }
    Ok(())
}

/// Ask the daemon to say `args.text` or send it as a voice note. `Ok` means the daemon
/// stored the line, not that it was heard. `Err` is the one line to print.
pub(crate) async fn speak(settings: &Settings, args: &AwayMessageArgs) -> Result<(), String> {
    speak_within(settings, args, CALL_DEADLINE).await
}

pub(crate) async fn speak_within(
    settings: &Settings,
    args: &AwayMessageArgs,
    wait: Duration,
) -> Result<(), String> {
    let address = settings
        .grpc
        .addr()
        .map_err(|_| "invalid configuration".to_string())?;
    call_within(CALL_DEADLINE, wait, async {
        let mut client = HaroldClient::connect(format!("http://{address}"))
            .await
            .map_err(|_| "the Harold daemon is not reachable".to_string())?;
        client
            .speak_line(SpeakLineRequest {
                src_id: args.src_id.clone(),
                text: args.text.clone(),
            })
            .await
            .map_err(|status| speak_refusal_line(status.code(), status.message(), &args.src_id))
    })
    .await?;
    Ok(())
}

fn json_error(what: impl std::fmt::Display) -> String {
    format!("invalid JSON: {what}")
}

fn json_text(object: &serde_json::Value, field: &str) -> Result<String, String> {
    match object.get(field) {
        Some(serde_json::Value::String(text)) => Ok(text.clone()),
        Some(_) => Err(json_error(format!("{field} must be a string"))),
        None => Err(json_error(format!("{field} is missing"))),
    }
}

fn json_flag(object: &serde_json::Value, field: &str) -> Result<bool, String> {
    match object.get(field) {
        None => Ok(false),
        Some(serde_json::Value::Bool(flag)) => Ok(*flag),
        Some(_) => Err(json_error(format!("{field} must be true or false"))),
    }
}

fn json_list<'a>(
    object: &'a serde_json::Value,
    field: &str,
) -> Result<&'a [serde_json::Value], String> {
    match object.get(field) {
        Some(serde_json::Value::Array(items)) => Ok(items),
        Some(_) => Err(json_error(format!("{field} must be a list"))),
        None => Err(json_error(format!("{field} is missing"))),
    }
}

fn option_from_json(option: &serde_json::Value) -> Result<questions::DecisionOption, String> {
    if !option.is_object() {
        return Err(json_error("option must be an object"));
    }
    Ok(questions::DecisionOption {
        key: json_text(option, "key")?,
        label: json_text(option, "label")?,
        impact: json_text(option, "impact")?,
        suggested: json_flag(option, "suggested")?,
        free: json_flag(option, "free")?,
    })
}

fn decision_from_json(decision: &serde_json::Value) -> Result<questions::Decision, String> {
    if !decision.is_object() {
        return Err(json_error("decision must be an object"));
    }
    Ok(questions::Decision {
        ask: json_text(decision, "ask")?,
        options: json_list(decision, "options")?
            .iter()
            .map(option_from_json)
            .collect::<Result<_, _>>()?,
    })
}

/// Turns the spec's JSON into a question. It only translates; the daemon checks the rules.
fn question_from_json(text: &str) -> Result<questions::Question, String> {
    let value: serde_json::Value = serde_json::from_str(text).map_err(json_error)?;
    if !value.is_object() {
        return Err(json_error("question must be an object"));
    }
    Ok(questions::Question {
        id: json_text(&value, "id")?,
        product: json_text(&value, "product")?,
        spec: json_text(&value, "spec")?,
        sha: json_text(&value, "sha")?,
        link: json_text(&value, "link")?,
        intro: json_text(&value, "intro")?,
        decisions: json_list(&value, "decisions")?
            .iter()
            .map(decision_from_json)
            .collect::<Result<_, _>>()?,
    })
}

/// The line printed for a refusal from the question service: its status message.
fn tool_refusal_line(status: &tonic::Status) -> String {
    match status.code() {
        tonic::Code::Unimplemented => "the Harold daemon does not serve questions".into(),
        _ if status.message().is_empty() => "the Harold daemon refused the call".into(),
        _ => status.message().to_string(),
    }
}

/// Run a `harold tool` command against the daemon at `address`. `Ok` is the line
/// to print; `Err` is the one line to print for a refusal. A backlog command sends the
/// caller's `TMUX_PANE` and works out local midnight in the Mac's UTC offset.
pub(crate) async fn tool(
    address: &str,
    command: &ToolCommand,
    stdin: impl std::io::Read,
) -> Result<String, String> {
    if let ToolCommand::Backlog(backlog) = command {
        let pane = std::env::var("TMUX_PANE").unwrap_or_default();
        return backlog_within(
            address,
            backlog,
            stdin,
            &pane,
            crate::telemetry::local_offset(),
            time::OffsetDateTime::now_utc(),
            backlog.deadline(),
        )
        .await;
    }
    tool_within(address, command, stdin, CALL_DEADLINE).await
}

/// Runs a `harold tool questions` command.
pub(crate) async fn tool_within(
    address: &str,
    command: &ToolCommand,
    mut stdin: impl std::io::Read,
    wait: Duration,
) -> Result<String, String> {
    let question = match command {
        ToolCommand::QuestionsPut => {
            let mut text = String::new();
            stdin.read_to_string(&mut text).map_err(json_error)?;
            Some(question_from_json(&text)?)
        }
        _ => None,
    };
    call_within(CALL_DEADLINE, wait, async {
        let mut client = QuestionServiceClient::connect(format!("http://{address}"))
            .await
            .map_err(|_| "the Harold daemon is not reachable".to_string())?;
        let refused = |status: tonic::Status| tool_refusal_line(&status);
        match command {
            ToolCommand::QuestionsPut => {
                let stored = client
                    .put_question(questions::PutQuestionRequest { question })
                    .await
                    .map_err(refused)?
                    .into_inner();
                Ok(format!("stored {} v{}", stored.id, stored.version))
            }
            ToolCommand::QuestionsDismiss { id, reason } => {
                client
                    .dismiss_question(questions::DismissQuestionRequest {
                        id: id.clone(),
                        reason: reason.clone(),
                    })
                    .await
                    .map_err(refused)?;
                Ok(format!("dismissed {id}"))
            }
            ToolCommand::QuestionsOrder { ids } => {
                client
                    .reorder_questions(questions::ReorderQuestionsRequest { ids: ids.clone() })
                    .await
                    .map_err(refused)?;
                Ok(format!("ordered {}", ids.len()))
            }
            ToolCommand::QuestionsList => {
                let listed = client
                    .list_questions(questions::ListQuestionsRequest {})
                    .await
                    .map_err(refused)?
                    .into_inner();
                let mut lines: Vec<String> = listed
                    .questions
                    .iter()
                    .map(|listed| {
                        let question = listed.question.clone().unwrap_or_default();
                        let deferred = if listed.deferred { " (deferred)" } else { "" };
                        format!(
                            "{} v{} {} {}{deferred}",
                            question.id, listed.version, question.product, question.spec
                        )
                    })
                    .collect();
                lines.push(format!("{} waiting", listed.waiting));
                Ok(lines.join("\n"))
            }
            // `tool` runs a backlog command through `backlog_within`.
            ToolCommand::Backlog(_) => Err(TOOL_USAGE_LINE.into()),
        }
    })
    .await
}

/// The line printed for a refusal from the backlog service. `pane` and `product` are the
/// caller's, as the command sent them.
fn backlog_refusal_line(status: &tonic::Status, pane: &str, product: &str) -> String {
    match (status.code(), status.message()) {
        (tonic::Code::PermissionDenied, "wrong_product") => {
            format!("refused: pane {pane} is not in {product}'s folder")
        }
        (tonic::Code::PermissionDenied, "not_chief") => {
            format!("refused: pane {pane} is not the chief's pane")
        }
        (tonic::Code::PermissionDenied, "no_pane") => "refused: not run in a tmux pane".into(),
        (tonic::Code::Unimplemented, _) => "the Harold daemon does not serve the backlog".into(),
        (_, "") => "the Harold daemon refused the call".into(),
        (_, message) => message.to_string(),
    }
}

fn json_optional_text(object: &serde_json::Value, field: &str) -> Result<String, String> {
    match object.get(field) {
        None => Ok(String::new()),
        Some(_) => json_text(object, field),
    }
}

/// Turns `put`'s JSON into an item. It only translates; the daemon checks the rules.
fn item_from_json(text: &str) -> Result<backlog_api::Item, String> {
    let value: serde_json::Value = serde_json::from_str(text).map_err(json_error)?;
    if !value.is_object() {
        return Err(json_error("item must be an object"));
    }
    let evidence = match value.get("evidence") {
        None => Vec::new(),
        Some(_) => json_list(&value, "evidence")?
            .iter()
            .map(|pointer| match pointer {
                serde_json::Value::String(pointer) => Ok(pointer.clone()),
                _ => Err(json_error("evidence must be a list of strings")),
            })
            .collect::<Result<_, _>>()?,
    };
    Ok(backlog_api::Item {
        id: json_text(&value, "id")?,
        title: json_text(&value, "title")?,
        value: json_optional_text(&value, "value")?,
        size: json_optional_text(&value, "size")?,
        risk: json_optional_text(&value, "risk")?,
        reason: json_optional_text(&value, "reason")?,
        paths: json_optional_text(&value, "paths")?,
        after: json_optional_text(&value, "after")?,
        evidence,
        rank: 0,
        updated_at_ms: 0,
    })
}

/// A note's `{"title", "body"}` from stdin.
fn note_from_json(text: &str) -> Result<(String, String), String> {
    let value: serde_json::Value = serde_json::from_str(text).map_err(json_error)?;
    if !value.is_object() {
        return Err(json_error("note must be an object"));
    }
    Ok((json_text(&value, "title")?, json_text(&value, "body")?))
}

fn item_json(item: &backlog_api::Item) -> serde_json::Value {
    serde_json::json!({
        "id": item.id,
        "title": item.title,
        "value": item.value,
        "size": item.size,
        "risk": item.risk,
        "reason": item.reason,
        "paths": item.paths,
        "after": item.after,
        "evidence": item.evidence,
        "rank": item.rank,
        "updated_at_ms": item.updated_at_ms,
    })
}

fn queue_item_json(item: &backlog_api::QueueItem) -> serde_json::Value {
    serde_json::json!({
        "id": item.id,
        "state": item.state,
        "position": item.position,
        "text": item.text,
        "done_at_ms": item.done_at_ms,
        "updated_at_ms": item.updated_at_ms,
    })
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or_default()
}

fn ms(at: time::OffsetDateTime) -> i64 {
    (at.unix_timestamp_nanos() / 1_000_000) as i64
}

/// The start of `now`'s day in `offset`, in Unix milliseconds.
fn local_midnight_ms(now: time::OffsetDateTime, offset: time::UtcOffset) -> i64 {
    date_midnight_ms(now.to_offset(offset).date(), offset)
}

/// The start of `date` in `offset`, in Unix milliseconds.
fn date_midnight_ms(date: time::Date, offset: time::UtcOffset) -> i64 {
    ms(date.midnight().assume_offset(offset))
}

fn counts_line(counts: &backlog_api::ImportCounts, queue: bool) -> String {
    match queue {
        true => format!(
            "now {}, parked {}, done {}",
            counts.now, counts.parked, counts.done
        ),
        false => format!(
            "items {}, notes: rule {}, history {}, held {}, left_out {}",
            counts.items, counts.rule, counts.history, counts.held, counts.left_out
        ),
    }
}

/// What a backlog command sends besides its arguments: stdin, read before any call.
enum Input {
    None,
    Item(backlog_api::Item),
    Note(String, String),
    Text(String),
    Markdown(String),
}

fn read_input(command: &BacklogCommand, mut stdin: impl std::io::Read) -> Result<Input, String> {
    use BacklogCommand as C;
    let mut read = || {
        let mut text = String::new();
        stdin
            .read_to_string(&mut text)
            .map(|_| text)
            .map_err(|error| format!("cannot read stdin: {error}"))
    };
    let file = |file: &std::path::Path| {
        std::fs::read_to_string(file)
            .map(Input::Markdown)
            .map_err(|error| format!("cannot read {}: {error}", file.display()))
    };
    match command {
        C::Put { .. } => Ok(Input::Item(item_from_json(&read().map_err(json_error)?)?)),
        C::NoteAdd { .. } | C::NoteUpdate { .. } => {
            let (title, body) = note_from_json(&read().map_err(json_error)?)?;
            Ok(Input::Note(title, body))
        }
        C::QueueAdd { .. } | C::QueueUpdate { .. } | C::QueueNote => {
            let text = read()?;
            Ok(Input::Text(text.trim_end_matches(['\n', '\r']).to_string()))
        }
        C::Import { file: path, .. } | C::QueueImport { file: path, .. } => file(path),
        _ => Ok(Input::None),
    }
}

/// Run a `harold tool backlog` command against the daemon at `address`, as `pane`, with
/// local midnight in `offset` at `now`. `Ok` is the text to print; `Err` the one line.
pub(crate) async fn backlog_within(
    address: &str,
    command: &BacklogCommand,
    stdin: impl std::io::Read,
    pane: &str,
    offset: time::UtcOffset,
    now: time::OffsetDateTime,
    wait: Duration,
) -> Result<String, String> {
    let input = read_input(command, stdin)?;
    call_within(command.deadline(), wait, async {
        let client = BacklogServiceClient::connect(format!("http://{address}"))
            .await
            .map_err(|_| "the Harold daemon is not reachable".to_string())?;
        backlog_call(client, command, input, pane, offset, now).await
    })
    .await
}

async fn backlog_call(
    mut client: BacklogServiceClient<tonic::transport::Channel>,
    command: &BacklogCommand,
    input: Input,
    pane: &str,
    offset: time::UtcOffset,
    now: time::OffsetDateTime,
) -> Result<String, String> {
    use BacklogCommand as C;
    let caller_pane = pane.to_string();
    let product = match command {
        C::Put { product }
        | C::Rank { product, .. }
        | C::Remove { product, .. }
        | C::List { product, .. }
        | C::NoteAdd { product, .. }
        | C::NoteUpdate { product, .. }
        | C::NoteRemove { product, .. }
        | C::Notes { product, .. }
        | C::Import { product, .. }
        | C::Export { product } => product.as_str(),
        _ => "",
    };
    let refused = |status: tonic::Status| backlog_refusal_line(&status, pane, product);
    let (item, (title, body), text, markdown) = match input {
        Input::None => (None, Default::default(), String::new(), String::new()),
        Input::Item(item) => (Some(item), Default::default(), String::new(), String::new()),
        Input::Note(title, body) => (None, (title, body), String::new(), String::new()),
        Input::Text(text) => (None, Default::default(), text, String::new()),
        Input::Markdown(markdown) => (None, Default::default(), String::new(), markdown),
    };
    match command {
        C::Put { product } => {
            let stored = client
                .put_item(backlog_api::PutItemRequest {
                    product: product.clone(),
                    item,
                    caller_pane,
                })
                .await
                .map_err(refused)?
                .into_inner();
            Ok(format!("stored {} at rank {}", stored.id, stored.rank))
        }
        C::Rank { product, ids } => {
            let ranked = client
                .rank_items(backlog_api::RankItemsRequest {
                    product: product.clone(),
                    ids: ids.clone(),
                    caller_pane,
                })
                .await
                .map_err(refused)?
                .into_inner();
            Ok(format!("ranked {}", ranked.count))
        }
        C::Remove {
            product,
            id,
            reason,
        } => {
            client
                .remove_item(backlog_api::RemoveItemRequest {
                    product: product.clone(),
                    id: id.clone(),
                    reason: reason.clone().unwrap_or_default(),
                    caller_pane,
                })
                .await
                .map_err(refused)?;
            Ok(format!("removed {id}"))
        }
        C::List { product, json } => {
            let items = client
                .list_items(backlog_api::ListItemsRequest {
                    product: product.clone(),
                })
                .await
                .map_err(refused)?
                .into_inner()
                .items;
            if *json {
                let items: Vec<_> = items.iter().map(item_json).collect();
                return Ok(serde_json::Value::Array(items).to_string());
            }
            let mut lines: Vec<String> = items
                .iter()
                .map(|item| format!("{}. {}: {}", item.rank, item.id, item.title))
                .collect();
            lines.push(format!("{} items", items.len()));
            Ok(lines.join("\n"))
        }
        C::NoteAdd { product, kind } => {
            let added = client
                .add_note(backlog_api::AddNoteRequest {
                    product: product.clone(),
                    kind: kind.clone(),
                    title,
                    body,
                    caller_pane,
                })
                .await
                .map_err(refused)?
                .into_inner();
            Ok(format!("added note {}", added.id))
        }
        C::NoteUpdate { product, id } => {
            client
                .update_note(backlog_api::UpdateNoteRequest {
                    product: product.clone(),
                    id: *id,
                    title,
                    body,
                    caller_pane,
                })
                .await
                .map_err(refused)?;
            Ok(format!("updated note {id}"))
        }
        C::NoteRemove { product, id } => {
            client
                .remove_note(backlog_api::RemoveNoteRequest {
                    product: product.clone(),
                    id: *id,
                    caller_pane,
                })
                .await
                .map_err(refused)?;
            Ok(format!("removed note {id}"))
        }
        C::Notes {
            product,
            kind,
            last,
        } => {
            let notes = client
                .list_notes(backlog_api::ListNotesRequest {
                    product: product.clone(),
                    kind: kind.clone().unwrap_or_default(),
                    last: last.unwrap_or(0),
                })
                .await
                .map_err(refused)?
                .into_inner()
                .notes;
            let notes: Vec<String> = notes
                .iter()
                .map(|note| format!("{} {} {}\n{}", note.id, note.kind, note.title, note.body))
                .collect();
            Ok(notes.join("\n\n"))
        }
        C::QueueAdd { place } => {
            let (now, at) = match place {
                QueuePlace::End => (false, 0),
                QueuePlace::Now => (true, 0),
                QueuePlace::At(at) => (false, *at),
            };
            let added = client
                .add_queue_item(backlog_api::AddQueueItemRequest {
                    text,
                    now,
                    at,
                    caller_pane,
                })
                .await
                .map_err(refused)?
                .into_inner()
                .item
                .unwrap_or_default();
            match added.state.as_str() {
                "now" => Ok(format!("added {} now", added.id)),
                _ => Ok(format!("added {} parked {}", added.id, added.position)),
            }
        }
        C::QueueNow { id } => {
            let set = client
                .set_now(backlog_api::SetNowRequest {
                    id: id.clone(),
                    caller_pane,
                })
                .await
                .map_err(refused)?
                .into_inner();
            Ok(format!("now {}, {} parked", set.now, set.parked))
        }
        C::QueueOrder { ids } => {
            let ordered = client
                .order_queue(backlog_api::OrderQueueRequest {
                    ids: ids.clone(),
                    caller_pane,
                })
                .await
                .map_err(refused)?
                .into_inner();
            Ok(format!("ordered {}", ordered.count))
        }
        C::QueueUpdate { id } => {
            client
                .update_queue_item(backlog_api::UpdateQueueItemRequest {
                    id: id.clone(),
                    text,
                    caller_pane,
                })
                .await
                .map_err(refused)?;
            Ok(format!("updated {id}"))
        }
        C::QueueDone { id } => {
            client
                .done_queue_item(backlog_api::DoneQueueItemRequest {
                    id: id.clone(),
                    caller_pane,
                })
                .await
                .map_err(refused)?;
            Ok(format!("done {id}"))
        }
        C::QueueNote => {
            client
                .set_queue_note(backlog_api::SetQueueNoteRequest { text, caller_pane })
                .await
                .map_err(refused)?;
            Ok("queue note set".into())
        }
        C::QueueList { json, done_since } => {
            let done_since_ms = match done_since {
                Some(date) => date_midnight_ms(*date, offset),
                None => local_midnight_ms(now, offset),
            };
            let listing = client
                .list_queue(backlog_api::ListQueueRequest { done_since_ms })
                .await
                .map_err(refused)?
                .into_inner();
            if *json {
                let listed = serde_json::json!({
                    "now": listing.now.as_ref().map(queue_item_json),
                    "parked": listing.parked.iter().map(queue_item_json).collect::<Vec<_>>(),
                    "done": listing.done.iter().map(queue_item_json).collect::<Vec<_>>(),
                    "note": listing.note,
                });
                return Ok(listed.to_string());
            }
            let mut lines = vec![match &listing.now {
                Some(item) => format!("Now: {} {}", item.id, first_line(&item.text)),
                None => "Now: nothing".into(),
            }];
            lines.push("Parked:".into());
            lines.extend(listing.parked.iter().map(|item| {
                format!(
                    "  {}. {} {}",
                    item.position,
                    item.id,
                    first_line(&item.text)
                )
            }));
            lines.push(match done_since {
                Some(date) => format!("Done since {date}:"),
                None => "Done today:".into(),
            });
            lines.extend(
                listing
                    .done
                    .iter()
                    .map(|item| format!("  {} {}", item.id, first_line(&item.text))),
            );
            Ok(lines.join("\n"))
        }
        C::Import { apply, .. } | C::QueueImport { apply, .. } => {
            let queue = matches!(command, C::QueueImport { .. });
            let imported = client
                .import(backlog_api::ImportRequest {
                    product: product.into(),
                    queue,
                    markdown,
                    apply: *apply,
                    caller_pane,
                    utc_offset_seconds: offset.whole_seconds(),
                })
                .await
                .map_err(refused)?
                .into_inner();
            let counts = counts_line(&imported.counts.unwrap_or_default(), queue);
            match imported.applied {
                true => Ok(format!("imported {counts}")),
                false => Ok(counts),
            }
        }
        C::Export { .. } | C::QueueExport => {
            let queue = matches!(command, C::QueueExport);
            let exported = client
                .export(backlog_api::ExportRequest {
                    product: product.into(),
                    queue,
                })
                .await
                .map_err(refused)?
                .into_inner();
            Ok(exported.markdown.trim_end_matches('\n').to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Mode, String> {
        Mode::parse(&args.iter().map(|a| a.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn the_call_deadline_is_three_seconds() {
        assert_eq!(CALL_DEADLINE, Duration::from_secs(3));
    }

    #[tokio::test]
    async fn the_timeout_line_names_the_deadline_not_the_wait() {
        let never = std::future::pending::<Result<(), String>>();
        let line = call_within(CALL_DEADLINE, Duration::from_millis(100), never).await;
        assert_eq!(
            line,
            Err("the Harold daemon did not answer within 3 seconds".into())
        );

        let never = std::future::pending::<Result<(), String>>();
        let line = call_within(Duration::from_secs(30), Duration::from_millis(100), never).await;
        assert_eq!(
            line,
            Err("the Harold daemon did not answer within 30 seconds".into())
        );
    }

    #[tokio::test]
    async fn call_within_returns_the_calls_own_result() {
        let line = call_within(CALL_DEADLINE, CALL_DEADLINE, async {
            Err::<(), String>("refused".into())
        })
        .await;
        assert_eq!(line, Err("refused".into()));
    }

    fn accept(event: AcceptEvent) -> Mode {
        Mode::Accept(AcceptArgs {
            event,
            pane: "%47".into(),
            adapter: "claude.lifecycle".into(),
        })
    }

    #[test]
    fn accept_parses_both_events_and_either_flag_order() {
        let started = [
            "accept",
            "turn-started",
            "--pane",
            "%47",
            "--adapter",
            "claude.lifecycle",
        ];
        assert_eq!(parse(&started), Ok(accept(AcceptEvent::TurnStarted)));
        let completed = [
            "accept",
            "turn-completed",
            "--adapter",
            "claude.lifecycle",
            "--pane",
            "%47",
        ];
        assert_eq!(parse(&completed), Ok(accept(AcceptEvent::TurnCompleted)));
    }

    #[test]
    fn accept_rejects_unknown_events_and_malformed_flags() {
        for args in [
            vec!["accept", "turn-paused", "--pane", "%1", "--adapter", "a"],
            vec!["accept", "turn-started", "--pane", "%1"],
            vec!["accept", "turn-started", "--adapter", "a"],
            vec![
                "accept",
                "turn-started",
                "--pane",
                "%1",
                "--pane",
                "%2",
                "--adapter",
                "a",
            ],
            vec!["accept", "turn-started", "--pane", "--adapter", "a"],
            vec![
                "accept",
                "turn-started",
                "--pane",
                "%1",
                "--adapter",
                "a",
                "--extra",
            ],
            vec!["accept"],
        ] {
            let error = parse(&args).unwrap_err();
            assert!(error.contains("argument"), "{args:?}: {error}");
        }
    }
    #[test]
    fn accept_parses_the_two_subagent_events() {
        for (name, event) in [
            ("still-working", AcceptEvent::StillWorking),
            ("subagents-finished", AcceptEvent::SubagentsFinished),
        ] {
            let args = [
                "accept",
                name,
                "--pane",
                "%47",
                "--adapter",
                "claude.lifecycle",
            ];
            assert_eq!(parse(&args), Ok(accept(event)), "{name}");
        }
    }

    #[test]
    fn accept_maps_each_event_to_busy_or_idle() {
        assert_eq!(AcceptEvent::TurnStarted.state(), AgentState::Busy);
        assert_eq!(AcceptEvent::StillWorking.state(), AgentState::Busy);
        assert_eq!(AcceptEvent::TurnCompleted.state(), AgentState::Idle);
        assert_eq!(AcceptEvent::SubagentsFinished.state(), AgentState::Idle);
    }

    const USAGE: &str = "invalid argument: usage is send-message-to-away-device --src-id ID TEXT";

    fn away(text: &str) -> Mode {
        Mode::SendMessageToAwayDevice(AwayMessageArgs {
            src_id: "%47".into(),
            text: text.into(),
        })
    }

    #[test]
    fn away_message_takes_the_source_and_the_text_in_either_order() {
        assert_eq!(
            parse(&[
                "send-message-to-away-device",
                "--src-id",
                "%47",
                "Pod 3 is blocked"
            ]),
            Ok(away("Pod 3 is blocked"))
        );
        assert_eq!(
            parse(&[
                "send-message-to-away-device",
                "Pod 3 is blocked",
                "--src-id",
                "%47"
            ]),
            Ok(away("Pod 3 is blocked"))
        );
    }

    #[test]
    fn away_message_text_reaches_the_daemon_as_typed() {
        let text = "  approval:\nhttps://example.com  ";
        assert_eq!(
            parse(&["send-message-to-away-device", "--src-id", "%47", text]),
            Ok(away(text))
        );
    }

    #[test]
    fn a_text_starting_with_one_dash_is_text() {
        assert_eq!(
            parse(&[
                "send-message-to-away-device",
                "--src-id",
                "%47",
                "- item one"
            ]),
            Ok(away("- item one"))
        );
        assert_eq!(
            parse(&[
                "send-message-to-away-device",
                "--src-id",
                "%47",
                "--flag is broken"
            ]),
            Err(USAGE.to_string())
        );
    }

    #[test]
    fn away_message_rejects_each_malformed_form_with_the_usage_line() {
        for args in [
            vec!["send-message-to-away-device"],
            vec!["send-message-to-away-device", "text"],
            vec!["send-message-to-away-device", "--src-id", "%47"],
            vec!["send-message-to-away-device", "--src-id"],
            vec!["send-message-to-away-device", "text", "--src-id"],
            vec![
                "send-message-to-away-device",
                "--src-id",
                "%1",
                "--src-id",
                "%2",
                "text",
            ],
            vec![
                "send-message-to-away-device",
                "--src-id",
                "%47",
                "one",
                "two",
            ],
            vec![
                "send-message-to-away-device",
                "--src-id",
                "%47",
                "text",
                "--loud",
            ],
            vec!["send-message-to-away-device", "--src-id=%47", "text"],
            // Outside tmux `"$TMUX_PANE"` is the empty string.
            vec!["send-message-to-away-device", "--src-id", "", "text"],
        ] {
            assert_eq!(parse(&args), Err(USAGE.to_string()), "{args:?}");
        }
    }

    #[test]
    fn away_message_checks_the_source_form_and_an_empty_text_itself() {
        for src_id in ["47", "%", "%4x", "pane"] {
            assert_eq!(
                parse(&["send-message-to-away-device", "--src-id", src_id, "text"]),
                Err("--src-id must be a tmux pane id such as %47".to_string()),
                "{src_id:?}"
            );
        }
        for text in ["", "   ", "\n\t"] {
            assert_eq!(
                parse(&["send-message-to-away-device", "--src-id", "%47", text]),
                Err("the message must be 1 to 1,000 characters".to_string()),
                "{text:?}"
            );
        }
    }

    #[test]
    fn each_refusal_status_maps_to_its_line() {
        use tonic::Code;
        for (code, message, line) in [
            (
                Code::InvalidArgument,
                "invalid source id",
                "--src-id must be a tmux pane id such as %47",
            ),
            (
                Code::InvalidArgument,
                "invalid message length",
                "the message must be 1 to 1,000 characters",
            ),
            (
                Code::FailedPrecondition,
                "messaging paused",
                "messaging is paused",
            ),
            (
                Code::FailedPrecondition,
                "agent not found",
                "no agent is running in pane %47",
            ),
            (
                Code::Internal,
                "event store write failed",
                "the Harold daemon could not store the message",
            ),
            (
                Code::Unavailable,
                "agent lookup unavailable",
                "the Harold daemon could not store the message",
            ),
            (
                Code::Unimplemented,
                "",
                "the Harold daemon could not store the message",
            ),
            // A known message under the wrong code is not one of the four refusals.
            (
                Code::Internal,
                "messaging paused",
                "the Harold daemon could not store the message",
            ),
        ] {
            assert_eq!(
                refusal_line(code, message, "%47"),
                line,
                "{code:?} {message}"
            );
        }
    }

    const SPEAK_USAGE: &str = "invalid argument: usage is speak --src-id ID TEXT";

    fn speak(text: &str) -> Mode {
        Mode::Speak(AwayMessageArgs {
            src_id: "%47".into(),
            text: text.into(),
        })
    }

    #[test]
    fn speak_takes_the_source_and_the_text_in_either_order() {
        assert_eq!(
            parse(&["speak", "--src-id", "%47", "All green."]),
            Ok(speak("All green."))
        );
        assert_eq!(
            parse(&["speak", "All green.", "--src-id", "%47"]),
            Ok(speak("All green."))
        );
    }

    #[test]
    fn speak_rejects_malformed_forms_with_its_own_usage_line() {
        for args in [
            vec!["speak"],
            vec!["speak", "text"],
            vec!["speak", "--src-id", "%47"],
            vec!["speak", "--src-id", "%47", "one", "two"],
            vec!["speak", "--src-id", "", "text"],
        ] {
            assert_eq!(parse(&args), Err(SPEAK_USAGE.to_string()), "{args:?}");
        }
        assert_eq!(
            parse(&["speak", "--src-id", "47", "text"]),
            Err("--src-id must be a tmux pane id such as %47".to_string())
        );
        assert_eq!(
            parse(&["speak", "--src-id", "%47", " \n "]),
            Err("the spoken line must be 1 to 300 characters".to_string())
        );
    }

    #[test]
    fn speak_refusals_print_the_spoken_line_wording() {
        assert_eq!(
            speak_refusal_line(tonic::Code::InvalidArgument, INVALID_MESSAGE_LENGTH, "%47"),
            "the spoken line must be 1 to 300 characters"
        );
        assert_eq!(
            speak_refusal_line(tonic::Code::FailedPrecondition, AGENT_NOT_FOUND, "%47"),
            "no agent is running in pane %47"
        );
        assert_eq!(
            speak_refusal_line(tonic::Code::Unimplemented, "", "%47"),
            "the Harold daemon could not store the spoken line"
        );
    }

    #[test]
    fn tool_questions_commands_parse() {
        assert_eq!(
            parse(&["tool", "questions", "put"]),
            Ok(Mode::Tool(ToolCommand::QuestionsPut))
        );
        assert_eq!(
            parse(&["tool", "questions", "list"]),
            Ok(Mode::Tool(ToolCommand::QuestionsList))
        );
        assert_eq!(
            parse(&[
                "tool",
                "questions",
                "dismiss",
                "q-1",
                "--reason",
                "no longer needed"
            ]),
            Ok(Mode::Tool(ToolCommand::QuestionsDismiss {
                id: "q-1".into(),
                reason: "no longer needed".into()
            }))
        );
        assert_eq!(
            parse(&["tool", "questions", "order", "b", "a"]),
            Ok(Mode::Tool(ToolCommand::QuestionsOrder {
                ids: vec!["b".into(), "a".into()]
            }))
        );
    }

    #[test]
    fn an_unknown_tool_command_is_refused_with_usage() {
        let usage = "invalid argument: usage is harold tool questions put|dismiss|order|list";
        for words in [
            vec!["tool"],
            vec!["tool", "questions"],
            vec!["tool", "other", "list"],
            vec!["tool", "questions", "order"],
            vec!["tool", "questions", "dismiss", "q-1"],
            vec!["tool", "questions", "dismiss", "--reason", "x"],
            vec!["tool", "questions", "list", "extra"],
            vec!["tool", "questions", "put", "extra"],
        ] {
            assert_eq!(parse(&words), Err(usage.to_string()), "{words:?}");
        }
    }

    #[test]
    fn there_is_no_answer_command() {
        let result = parse(&["tool", "questions", "answer", "q-1"]);
        assert_eq!(
            result,
            Err("invalid argument: usage is harold tool questions put|dismiss|order|list".into())
        );
    }

    const SPEC_JSON: &str = r#"{
        "id": "q-1", "product": "harold", "spec": "dash", "sha": "abc123",
        "link": "https://example.com/spec", "intro": "Two things",
        "decisions": [
            {"ask": "Which store?", "options": [
                {"key": "a", "label": "Turso", "impact": "fast", "suggested": true},
                {"key": "b", "label": "Other", "impact": "", "free": true}
            ]}
        ]
    }"#;

    #[test]
    fn put_turns_the_spec_json_into_a_question() {
        let question = question_from_json(SPEC_JSON).unwrap();
        assert_eq!(question.id, "q-1");
        assert_eq!(question.product, "harold");
        assert_eq!(question.spec, "dash");
        assert_eq!(question.sha, "abc123");
        assert_eq!(question.link, "https://example.com/spec");
        assert_eq!(question.intro, "Two things");
        assert_eq!(question.decisions.len(), 1);
        let decision = &question.decisions[0];
        assert_eq!(decision.ask, "Which store?");
        assert_eq!(decision.options.len(), 2);
        let (first, second) = (&decision.options[0], &decision.options[1]);
        assert_eq!(
            (&*first.key, &*first.label, &*first.impact),
            ("a", "Turso", "fast")
        );
        assert!(first.suggested && !first.free);
        assert!(!second.suggested && second.free);
    }

    #[test]
    fn bad_json_is_refused() {
        let error = question_from_json("{not json").unwrap_err();
        assert!(error.starts_with("invalid JSON: "), "{error}");
        for (text, what) in [
            (r#"{"id": 5}"#, "id"),
            (r#"[]"#, "question"),
            (
                r#"{"id":"a","product":"p","spec":"s","sha":"h","link":"l","intro":"i","decisions":[{"ask":"x","options":[{"key":"k","label":"l","impact":"i","free":"yes"}]}]}"#,
                "free",
            ),
            (
                r#"{"id":"a","product":"p","spec":"s","sha":"h","link":"l","intro":"i"}"#,
                "decisions",
            ),
        ] {
            let error = question_from_json(text).unwrap_err();
            assert!(
                error.starts_with("invalid JSON: ") && error.contains(what),
                "{text}: {error}"
            );
        }
    }

    struct Typist;
    impl crate::tools::PaneTyping for Typist {
        fn type_line<'a>(
            &'a self,
            _pane_id: &'a str,
            _text: &'a str,
        ) -> crate::tools::BoxFuture<'a, crate::tools::Typing> {
            Box::pin(async { crate::tools::Typing::Typed })
        }
    }
    struct NoChief;
    impl crate::tools::ChiefPane for NoChief {
        fn chief_pane(&self) -> Option<String> {
            None
        }
    }

    struct NoFolder;
    impl crate::tools::PaneFolder for NoFolder {
        fn folder_of(&self, _pane_id: &str) -> Option<std::path::PathBuf> {
            None
        }
    }
    struct NoProducts;
    impl crate::tools::Products for NoProducts {
        fn products(&self) -> Result<Vec<(String, std::path::PathBuf)>, &'static str> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn the_chief_puts_lists_orders_and_dismisses_through_the_tool() {
        let folder = std::env::temp_dir().join(format!("harold-cli-tool-{}", uuid::Uuid::new_v4()));
        let (stop, shutdown) = tokio::sync::watch::channel(());
        let handle = crate::tools::questions::start(crate::tools::ToolContext {
            data_folder: folder.clone(),
            typing: Some(std::sync::Arc::new(Typist)),
            chief_pane: std::sync::Arc::new(NoChief),
            pane_folder: std::sync::Arc::new(NoFolder),
            products: std::sync::Arc::new(NoProducts),
            shutdown,
        })
        .await
        .unwrap_or_else(|e| panic!("start failed: {}", e.reason));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let (incoming, connections) = tokio::sync::mpsc::channel(1);
        let accept = tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                if incoming
                    .send(Ok::<_, std::io::Error>(socket))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        let server = tokio::spawn(
            tonic::transport::Server::builder()
                .add_routes(handle.routes)
                .serve_with_incoming(tokio_stream::wrappers::ReceiverStream::new(connections)),
        );

        let second = SPEC_JSON.replace("q-1", "q-2").replace("dash", "other");
        let mut lines = Vec::new();
        for (command, input) in [
            (ToolCommand::QuestionsPut, SPEC_JSON.to_string()),
            (ToolCommand::QuestionsPut, SPEC_JSON.to_string()),
            (ToolCommand::QuestionsPut, second),
            (ToolCommand::QuestionsList, String::new()),
            (
                ToolCommand::QuestionsOrder {
                    ids: vec!["q-2".into(), "q-1".into()],
                },
                String::new(),
            ),
            (ToolCommand::QuestionsList, String::new()),
            (
                ToolCommand::QuestionsDismiss {
                    id: "q-1".into(),
                    reason: "done".into(),
                },
                String::new(),
            ),
            (ToolCommand::QuestionsList, String::new()),
        ] {
            lines.push(tool(&address, &command, input.as_bytes()).await.unwrap());
        }
        let refused = tool(
            &address,
            &ToolCommand::QuestionsPut,
            SPEC_JSON.replace("https://", "ftp://").as_bytes(),
        )
        .await
        .unwrap_err();
        let missing = tool(
            &address,
            &ToolCommand::QuestionsDismiss {
                id: "q-9".into(),
                reason: "x".into(),
            },
            &b""[..],
        )
        .await
        .unwrap_err();

        server.abort();
        accept.abort();
        drop(stop);
        let _ = handle.task.await;
        println!("{lines:#?}\nrefused: {refused}\nmissing: {missing}");

        assert_eq!(lines[0], "stored q-1 v1");
        assert_eq!(lines[1], "stored q-1 v2");
        assert_eq!(lines[2], "stored q-2 v1");
        assert_eq!(
            lines[3],
            "q-1 v2 harold dash\nq-2 v1 harold other\n2 waiting"
        );
        assert_eq!(lines[4], "ordered 2");
        assert_eq!(
            lines[5],
            "q-2 v1 harold other\nq-1 v2 harold dash\n2 waiting"
        );
        assert_eq!(lines[6], "dismissed q-1");
        assert_eq!(lines[7], "q-2 v1 harold other\n1 waiting");
        assert_eq!(refused, "invalid link");
        assert_eq!(missing, "not_waiting");
    }

    #[tokio::test]
    async fn an_unreachable_daemon_is_named() {
        let line = tool(CLOSED, &ToolCommand::QuestionsList, &b""[..]).await;
        assert_eq!(line, Err("the Harold daemon is not reachable".into()));
    }

    /// Nothing can listen on port 0, so a connect is refused at once and no other process can take it.
    const CLOSED: &str = "127.0.0.1:0";
    const GUARD: Duration = Duration::from_secs(30);
    const NOT_ANSWERING: &str = "the Harold daemon did not answer within 3 seconds";
    const NOT_REACHABLE: &str = "the Harold daemon is not reachable";

    /// A daemon stand-in that takes connections and never reads or writes.
    async fn never_answers() -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let handle = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                held.push(socket);
            }
        });
        (address, handle)
    }

    fn settings_for(address: &str) -> Settings {
        let (host, port) = address.rsplit_once(':').unwrap();
        crate::peers::settings_with(&format!("[grpc]\nhost = \"{host}\"\nport = {port}"))
    }

    fn accept_args() -> AcceptArgs {
        AcceptArgs {
            event: AcceptEvent::TurnCompleted,
            pane: "%47".into(),
            adapter: "claude.lifecycle".into(),
        }
    }

    fn message_args() -> AwayMessageArgs {
        AwayMessageArgs {
            src_id: "%47".into(),
            text: "hello".into(),
        }
    }

    #[tokio::test]
    async fn accept_gives_up_on_a_daemon_that_never_answers() {
        let (hung, stand_in) = never_answers().await;
        let wait = Duration::from_millis(100);
        let result = tokio::time::timeout(
            GUARD,
            accept_within(&settings_for(&hung), &accept_args(), wait),
        )
        .await
        .expect("accept waited past its deadline");
        stand_in.abort();
        assert_eq!(result.unwrap_err().to_string(), NOT_ANSWERING);
    }

    #[tokio::test]
    async fn accept_names_a_daemon_that_is_down() {
        let wait = Duration::from_secs(3600);
        let result = tokio::time::timeout(
            GUARD,
            accept_within(&settings_for(CLOSED), &accept_args(), wait),
        )
        .await
        .expect("accept waited past its deadline");
        assert_eq!(result.unwrap_err().to_string(), NOT_REACHABLE);
    }

    #[tokio::test]
    async fn away_message_gives_up_on_a_daemon_that_never_answers() {
        let (hung, stand_in) = never_answers().await;
        let wait = Duration::from_millis(100);
        let result = tokio::time::timeout(
            GUARD,
            send_message_to_away_device_within(&settings_for(&hung), &message_args(), wait),
        )
        .await
        .expect("away message waited past its deadline");
        stand_in.abort();
        assert_eq!(result, Err(NOT_ANSWERING.into()));
    }

    #[tokio::test]
    async fn away_message_names_a_daemon_that_is_down() {
        let wait = Duration::from_secs(3600);
        let result = tokio::time::timeout(
            GUARD,
            send_message_to_away_device_within(&settings_for(CLOSED), &message_args(), wait),
        )
        .await
        .expect("away message waited past its deadline");
        assert_eq!(result, Err(NOT_REACHABLE.into()));
    }

    #[tokio::test]
    async fn speak_gives_up_on_a_daemon_that_never_answers() {
        let (hung, stand_in) = never_answers().await;
        let wait = Duration::from_millis(100);
        let result = tokio::time::timeout(
            GUARD,
            speak_within(&settings_for(&hung), &message_args(), wait),
        )
        .await
        .expect("speak waited past its deadline");
        stand_in.abort();
        assert_eq!(result, Err(NOT_ANSWERING.into()));
    }

    #[tokio::test]
    async fn speak_names_a_daemon_that_is_down() {
        let wait = Duration::from_secs(3600);
        let result = tokio::time::timeout(
            GUARD,
            speak_within(&settings_for(CLOSED), &message_args(), wait),
        )
        .await
        .expect("speak waited past its deadline");
        assert_eq!(result, Err(NOT_REACHABLE.into()));
    }

    #[tokio::test]
    async fn tool_gives_up_on_a_daemon_that_never_answers() {
        let (hung, stand_in) = never_answers().await;
        let wait = Duration::from_millis(100);
        let result = tokio::time::timeout(
            GUARD,
            tool_within(&hung, &ToolCommand::QuestionsList, &b""[..], wait),
        )
        .await
        .expect("tool waited past its deadline");
        stand_in.abort();
        assert_eq!(result, Err(NOT_ANSWERING.into()));
    }

    #[tokio::test]
    async fn tool_names_a_daemon_that_is_down() {
        let wait = Duration::from_secs(3600);
        let result = tokio::time::timeout(
            GUARD,
            tool_within(CLOSED, &ToolCommand::QuestionsList, &b""[..], wait),
        )
        .await
        .expect("tool waited past its deadline");
        assert_eq!(result, Err(NOT_REACHABLE.into()));
    }

    fn backlog_parse(words: &[&str]) -> Result<Mode, String> {
        let mut args = vec!["tool", "backlog"];
        args.extend_from_slice(words);
        parse(&args)
    }

    fn backlog_mode(command: BacklogCommand) -> Result<Mode, String> {
        Ok(Mode::Tool(ToolCommand::Backlog(command)))
    }

    #[test]
    fn tool_backlog_commands_parse() {
        use BacklogCommand as C;
        let p = || "harold".to_string();
        for (words, command) in [
            (vec!["put", "harold"], C::Put { product: p() }),
            (
                vec!["rank", "harold", "b", "a"],
                C::Rank {
                    product: p(),
                    ids: vec!["b".into(), "a".into()],
                },
            ),
            (
                vec!["remove", "harold", "a"],
                C::Remove {
                    product: p(),
                    id: "a".into(),
                    reason: None,
                },
            ),
            (
                vec!["remove", "harold", "a", "--reason", "done elsewhere"],
                C::Remove {
                    product: p(),
                    id: "a".into(),
                    reason: Some("done elsewhere".into()),
                },
            ),
            (
                vec!["list", "harold"],
                C::List {
                    product: p(),
                    json: false,
                },
            ),
            (
                vec!["list", "harold", "--json"],
                C::List {
                    product: p(),
                    json: true,
                },
            ),
            (
                vec!["note", "add", "harold", "rule"],
                C::NoteAdd {
                    product: p(),
                    kind: "rule".into(),
                },
            ),
            (
                vec!["note", "update", "harold", "7"],
                C::NoteUpdate {
                    product: p(),
                    id: 7,
                },
            ),
            (
                vec!["note", "remove", "harold", "7"],
                C::NoteRemove {
                    product: p(),
                    id: 7,
                },
            ),
            (
                vec!["notes", "harold"],
                C::Notes {
                    product: p(),
                    kind: None,
                    last: None,
                },
            ),
            (
                vec!["notes", "harold", "--kind", "held", "--last", "3"],
                C::Notes {
                    product: p(),
                    kind: Some("held".into()),
                    last: Some(3),
                },
            ),
            (
                vec!["queue", "add"],
                C::QueueAdd {
                    place: QueuePlace::End,
                },
            ),
            (
                vec!["queue", "add", "--now"],
                C::QueueAdd {
                    place: QueuePlace::Now,
                },
            ),
            (
                vec!["queue", "add", "--at", "2"],
                C::QueueAdd {
                    place: QueuePlace::At(2),
                },
            ),
            (vec!["queue", "now", "N3"], C::QueueNow { id: "N3".into() }),
            (
                vec!["queue", "order", "N3", "N1"],
                C::QueueOrder {
                    ids: vec!["N3".into(), "N1".into()],
                },
            ),
            (
                vec!["queue", "update", "N3"],
                C::QueueUpdate { id: "N3".into() },
            ),
            (
                vec!["queue", "done", "N3"],
                C::QueueDone { id: "N3".into() },
            ),
            (vec!["queue", "note"], C::QueueNote),
            (
                vec!["queue", "list"],
                C::QueueList {
                    json: false,
                    done_since: None,
                },
            ),
            (
                vec!["queue", "list", "--json", "--done-since", "2026-03-02"],
                C::QueueList {
                    json: true,
                    done_since: Some(
                        time::Date::from_calendar_date(2026, time::Month::March, 2).unwrap(),
                    ),
                },
            ),
            (
                vec!["import", "harold", "f.md"],
                C::Import {
                    product: p(),
                    file: "f.md".into(),
                    apply: false,
                },
            ),
            (
                vec!["import", "harold", "f.md", "--apply"],
                C::Import {
                    product: p(),
                    file: "f.md".into(),
                    apply: true,
                },
            ),
            (
                vec!["queue", "import", "q.md"],
                C::QueueImport {
                    file: "q.md".into(),
                    apply: false,
                },
            ),
            (
                vec!["queue", "import", "q.md", "--apply"],
                C::QueueImport {
                    file: "q.md".into(),
                    apply: true,
                },
            ),
            (vec!["export", "harold"], C::Export { product: p() }),
            (vec!["queue", "export"], C::QueueExport),
        ] {
            assert_eq!(backlog_parse(&words), backlog_mode(command), "{words:?}");
        }
    }

    #[test]
    fn a_malformed_backlog_command_is_refused_with_usage() {
        let usage = "invalid argument: usage is harold tool backlog put|rank|remove|list|note|notes|queue|import|export";
        for words in [
            vec![],
            vec!["put"],
            vec!["rank", "harold"],
            vec!["note", "add", "harold"],
            vec!["notes", "harold", "--last", "x"],
            vec!["queue", "add", "--at", "0"],
            vec!["queue", "list", "--done-since", "tomorrow"],
            vec!["queue", "add", "--now", "--at", "2"],
            vec!["list", "--json"],
            vec!["list", "harold", "--json", "--json"],
            vec!["remove", "harold", "a", "--reason"],
            vec!["note", "update", "harold", "x"],
            vec!["queue", "list", "--done-since", "2026-02-30"],
            vec!["export", "harold", "extra"],
        ] {
            assert_eq!(backlog_parse(&words), Err(usage.to_string()), "{words:?}");
        }
        // The question tool keeps its own usage line.
        assert_eq!(
            parse(&["tool", "questions"]),
            Err("invalid argument: usage is harold tool questions put|dismiss|order|list".into())
        );
    }

    #[test]
    fn import_and_export_use_the_bulk_deadline() {
        use BacklogCommand as C;
        assert_eq!(BULK_DEADLINE, Duration::from_secs(30));
        for command in [
            C::Import {
                product: "harold".into(),
                file: "f.md".into(),
                apply: true,
            },
            C::QueueImport {
                file: "q.md".into(),
                apply: false,
            },
            C::Export {
                product: "harold".into(),
            },
            C::QueueExport,
        ] {
            assert_eq!(command.deadline(), BULK_DEADLINE, "{command:?}");
        }
        let list = C::List {
            product: "harold".into(),
            json: false,
        };
        assert_eq!(list.deadline(), CALL_DEADLINE);
    }

    #[test]
    fn local_midnight_is_in_the_given_offset() {
        let now = time::Date::from_calendar_date(2026, time::Month::March, 2)
            .unwrap()
            .with_hms(1, 30, 0)
            .unwrap()
            .assume_utc();
        let offset = time::UtcOffset::from_hms(10, 0, 0).unwrap();
        let midnight = time::Date::from_calendar_date(2026, time::Month::March, 1)
            .unwrap()
            .with_hms(14, 0, 0)
            .unwrap()
            .assume_utc();
        assert_eq!(local_midnight_ms(now, offset), ms(midnight));
        let date = time::Date::from_calendar_date(2026, time::Month::March, 2).unwrap();
        assert_eq!(date_midnight_ms(date, offset), ms(midnight));
    }

    #[test]
    fn each_backlog_refusal_maps_to_its_line() {
        use tonic::{Code, Status};
        for (status, line) in [
            (
                Status::permission_denied("wrong_product"),
                "refused: pane %51 is not in harold's folder",
            ),
            (
                Status::permission_denied("not_chief"),
                "refused: pane %51 is not the chief's pane",
            ),
            (
                Status::permission_denied("no_pane"),
                "refused: not run in a tmux pane",
            ),
            (
                Status::new(Code::Unimplemented, ""),
                "the Harold daemon does not serve the backlog",
            ),
            (Status::not_found("unknown_product"), "unknown_product"),
            (Status::failed_precondition("backlog_full"), "backlog_full"),
            (
                Status::invalid_argument("unparsed line 4"),
                "unparsed line 4",
            ),
            (
                Status::unavailable("registry_unreadable"),
                "registry_unreadable",
            ),
            (
                Status::new(Code::Internal, ""),
                "the Harold daemon refused the call",
            ),
        ] {
            assert_eq!(backlog_refusal_line(&status, "%51", "harold"), line);
        }
    }

    /// A folder under the system temp folder, removed when dropped.
    struct TempFolder(std::path::PathBuf);

    impl TempFolder {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("harold-cli-backlog-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempFolder {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    struct ChiefAt(&'static str);
    impl crate::tools::ChiefPane for ChiefAt {
        fn chief_pane(&self) -> Option<String> {
            Some(self.0.into())
        }
    }

    struct Folders(Vec<(&'static str, std::path::PathBuf)>);
    impl crate::tools::PaneFolder for Folders {
        fn folder_of(&self, pane_id: &str) -> Option<std::path::PathBuf> {
            self.0
                .iter()
                .find(|(pane, _)| *pane == pane_id)
                .map(|(_, folder)| folder.clone())
        }
    }

    struct Registry(Vec<(String, std::path::PathBuf)>);
    impl crate::tools::Products for Registry {
        fn products(&self) -> Result<Vec<(String, std::path::PathBuf)>, &'static str> {
            Ok(self.0.clone())
        }
    }

    /// A real gRPC server over `routes` on a local port, stopped when dropped.
    struct Serving {
        address: String,
        tasks: Vec<tokio::task::AbortHandle>,
    }

    impl Drop for Serving {
        fn drop(&mut self) {
            for task in &self.tasks {
                task.abort();
            }
        }
    }

    async fn serve(routes: tonic::service::Routes) -> Serving {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let (incoming, connections) = tokio::sync::mpsc::channel(1);
        let accept = tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                if incoming
                    .send(Ok::<_, std::io::Error>(socket))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        let server = tokio::spawn(
            tonic::transport::Server::builder()
                .add_routes(routes)
                .serve_with_incoming(tokio_stream::wrappers::ReceiverStream::new(connections)),
        );
        Serving {
            address,
            tasks: vec![accept.abort_handle(), server.abort_handle()],
        }
    }

    /// The backlog tool over a temp folder: `%40` is in `harold`'s repo, `%51` in
    /// `turso`'s, and `%2` is the chief.
    struct BacklogServer {
        serving: Serving,
        folder: TempFolder,
        _stop: tokio::sync::watch::Sender<()>,
    }

    async fn backlog_server() -> BacklogServer {
        let folder = TempFolder::new();
        let (harold, turso) = (folder.0.join("harold"), folder.0.join("turso"));
        std::fs::create_dir_all(&harold).unwrap();
        std::fs::create_dir_all(&turso).unwrap();
        let (stop, shutdown) = tokio::sync::watch::channel(());
        let handle = crate::tools::backlog::start(crate::tools::ToolContext {
            data_folder: folder.0.join("data"),
            typing: None,
            chief_pane: std::sync::Arc::new(ChiefAt("%2")),
            pane_folder: std::sync::Arc::new(Folders(vec![
                ("%40", harold.clone()),
                ("%51", turso.clone()),
            ])),
            products: std::sync::Arc::new(Registry(vec![
                ("harold".into(), harold),
                ("turso".into(), turso),
            ])),
            shutdown,
        })
        .await
        .unwrap_or_else(|e| panic!("start failed: {}", e.reason));
        BacklogServer {
            serving: serve(handle.routes).await,
            folder,
            _stop: stop,
        }
    }

    impl BacklogServer {
        async fn run(&self, pane: &str, words: &[&str], input: &str) -> Result<String, String> {
            let Ok(Mode::Tool(ToolCommand::Backlog(command))) = backlog_parse(words) else {
                panic!("not a backlog command: {words:?}");
            };
            backlog_within(
                &self.serving.address,
                &command,
                input.as_bytes(),
                pane,
                time::UtcOffset::UTC,
                time::OffsetDateTime::now_utc(),
                command.deadline(),
            )
            .await
        }
    }

    fn item_json_input(id: &str) -> String {
        format!(
            r#"{{"id":"{id}","title":"Title {id}","size":"small","evidence":["harold.log:1"]}}"#
        )
    }

    #[tokio::test]
    async fn a_product_manager_keeps_its_backlog_through_the_cli() {
        let server = backlog_server().await;
        let pm = "%40";
        let mut lines = Vec::new();
        for id in ["a", "b", "c", "d", "e"] {
            lines.push(
                server
                    .run(pm, &["put", "harold"], &item_json_input(id))
                    .await,
            );
        }
        let full = server
            .run(pm, &["put", "harold"], &item_json_input("f"))
            .await;
        let no_evidence = server
            .run(pm, &["put", "harold"], r#"{"id":"a","title":"Title a"}"#)
            .await;
        let ranked = server
            .run(pm, &["rank", "harold", "e", "d", "c", "b", "a"], "")
            .await;
        let listed = server.run(pm, &["list", "harold"], "").await.unwrap();
        let json = server
            .run(pm, &["list", "harold", "--json"], "")
            .await
            .unwrap();
        let removed = server
            .run(
                pm,
                &["remove", "harold", "a", "--reason", "done elsewhere"],
                "",
            )
            .await;
        let mut added = Vec::new();
        for kind in ["rule", "history", "held", "left_out"] {
            let note = format!(r#"{{"title":"A {kind} note","body":"The {kind} body."}}"#);
            added.push(
                server
                    .run(pm, &["note", "add", "harold", kind], &note)
                    .await
                    .unwrap(),
            );
        }
        let notes = server.run(pm, &["notes", "harold"], "").await.unwrap();
        let history = server
            .run(pm, &["notes", "harold", "--kind", "history"], "")
            .await
            .unwrap();
        let history_id = added[1].strip_prefix("added note ").unwrap().to_string();
        let append_only = server
            .run(
                pm,
                &["note", "update", "harold", &history_id],
                r#"{"title":"t","body":"b"}"#,
            )
            .await;
        let rule_id = added[0].strip_prefix("added note ").unwrap().to_string();
        let updated = server
            .run(
                pm,
                &["note", "update", "harold", &rule_id],
                r#"{"title":"New rule","body":"New body."}"#,
            )
            .await;
        let gone = server
            .run(pm, &["note", "remove", "harold", &rule_id], "")
            .await;
        println!(
            "{lines:#?}\nfull: {full:?}\nno evidence: {no_evidence:?}\nranked: {ranked:?}\n\
             {listed}\n{json}\nremoved: {removed:?}\n{added:?}\n{notes}\n--\n{history}\n\
             append only: {append_only:?}\nupdated: {updated:?}\ngone: {gone:?}"
        );

        for (n, (line, id)) in lines.iter().zip(["a", "b", "c", "d", "e"]).enumerate() {
            assert_eq!(line, &Ok(format!("stored {id} at rank {}", n + 1)));
        }
        assert_eq!(full, Err("backlog_full".into()));
        assert_eq!(no_evidence, Err("invalid evidence".into()));
        assert_eq!(ranked, Ok("ranked 5".into()));
        assert_eq!(
            listed,
            "1. e: Title e\n2. d: Title d\n3. c: Title c\n4. b: Title b\n5. a: Title a\n5 items"
        );
        let json: serde_json::Value = serde_json::from_str(&json).unwrap();
        let items = json.as_array().unwrap();
        assert_eq!(items.len(), 5);
        let mut fields: Vec<&str> = items[0]
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        fields.sort();
        assert_eq!(
            fields,
            [
                "after",
                "evidence",
                "id",
                "paths",
                "rank",
                "reason",
                "risk",
                "size",
                "title",
                "updated_at_ms",
                "value"
            ]
        );
        assert_eq!(items[0]["id"], "e");
        assert_eq!(items[0]["evidence"], serde_json::json!(["harold.log:1"]));
        assert_eq!(removed, Ok("removed a".into()));
        assert!(
            added.iter().all(|line| line.starts_with("added note ")),
            "{added:?}"
        );
        for kind in ["rule", "history", "held", "left_out"] {
            let head = format!(
                "{} {kind} A {kind} note\nThe {kind} body.",
                added_id(&added, kind)
            );
            assert!(notes.contains(&head), "{notes}");
        }
        assert!(
            notes.contains("\n\n"),
            "notes are separated by a blank line: {notes}"
        );
        assert!(history.contains("removed a: done elsewhere"), "{history}");
        assert!(history.contains("A history note"), "{history}");
        assert!(!history.contains("A rule note"), "{history}");
        assert_eq!(append_only, Err("append_only".into()));
        assert_eq!(updated, Ok(format!("updated note {rule_id}")));
        assert_eq!(gone, Ok(format!("removed note {rule_id}")));
    }

    fn added_id(added: &[String], kind: &str) -> String {
        let at = ["rule", "history", "held", "left_out"]
            .iter()
            .position(|k| *k == kind)
            .unwrap();
        added[at].strip_prefix("added note ").unwrap().to_string()
    }

    #[tokio::test]
    async fn the_chief_keeps_the_queue_through_the_cli() {
        let server = backlog_server().await;
        let chief = "%2";
        let mut lines = Vec::new();
        for (words, input) in [
            (vec!["queue", "add"], "first\n  more of first\n"),
            (vec!["queue", "add"], "second\n"),
            (vec!["queue", "add"], "third"),
            (vec!["queue", "add", "--now"], "urgent\n"),
            (vec!["queue", "add", "--at", "1"], "top\n"),
            (vec!["queue", "now", "N1"], ""),
            (vec!["queue", "order", "N3", "N2"], ""),
            (vec!["queue", "update", "N3"], "third, edited\n"),
            (vec!["queue", "done", "N1"], ""),
            (vec!["queue", "note"], "Session started today\n"),
        ] {
            lines.push(server.run(chief, &words, input).await);
        }
        let listed = server.run(chief, &["queue", "list"], "").await.unwrap();
        let json = server
            .run(chief, &["queue", "list", "--json"], "")
            .await
            .unwrap();
        let since = server
            .run(chief, &["queue", "list", "--done-since", "2026-01-01"], "")
            .await
            .unwrap();
        println!("{lines:#?}\n{listed}\n{json}\n{since}");

        let expected = [
            "added N1 parked 1",
            "added N2 parked 2",
            "added N3 parked 3",
            "added N4 now",
            "added N5 parked 1",
            "now N1, 4 parked",
            "ordered 4",
            "updated N3",
            "done N1",
            "queue note set",
        ];
        for (line, expected) in lines.iter().zip(expected) {
            assert_eq!(line, &Ok(expected.to_string()));
        }
        assert_eq!(
            listed,
            "Now: nothing\nParked:\n  1. N3 third, edited\n  2. N2 second\n  3. N4 urgent\n  \
             4. N5 top\nDone today:\n  N1 first"
        );
        let json: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(json["now"], serde_json::Value::Null);
        assert_eq!(json["parked"].as_array().unwrap().len(), 4);
        assert_eq!(json["done"][0]["id"], "N1");
        assert_eq!(json["done"][0]["text"], "first\n  more of first");
        assert_eq!(json["note"], "Session started today");
        assert!(
            since.ends_with("Done since 2026-01-01:\n  N1 first"),
            "{since}"
        );
    }

    #[tokio::test]
    async fn the_guard_refusals_print_their_lines() {
        let server = backlog_server().await;
        let other = server
            .run("%51", &["put", "harold"], &item_json_input("a"))
            .await;
        let not_chief = server.run("%40", &["queue", "add"], "text").await;
        let no_pane = server
            .run("", &["put", "harold"], &item_json_input("a"))
            .await;
        let unknown = server
            .run("%2", &["put", "nobody"], &item_json_input("a"))
            .await;
        // A dry run reads, so it needs no pane, but still names an unknown product.
        let source = server.folder.0.join("backlog.md");
        std::fs::write(&source, include_str!("tools/backlog/fixtures/backlog.md")).unwrap();
        let dry = server
            .run("", &["import", "nobody", source.to_str().unwrap()], "")
            .await;
        println!("{other:?}\n{not_chief:?}\n{no_pane:?}\n{unknown:?}\n{dry:?}");

        assert_eq!(
            other,
            Err("refused: pane %51 is not in harold's folder".into())
        );
        assert_eq!(
            not_chief,
            Err("refused: pane %40 is not the chief's pane".into())
        );
        assert_eq!(no_pane, Err("refused: not run in a tmux pane".into()));
        assert_eq!(unknown, Err("unknown_product".into()));
        assert_eq!(dry, Err("unknown_product".into()));
    }

    /// The lines an export must keep: not blank, not a heading.
    fn content_lines(markdown: &str) -> Vec<String> {
        markdown
            .lines()
            .map(|line| line.replace('\r', ""))
            .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
            .collect()
    }

    #[tokio::test]
    async fn import_dry_run_apply_and_export_through_the_cli() {
        let server = backlog_server().await;
        let backlog_md = include_str!("tools/backlog/fixtures/backlog.md");
        let queue_md = include_str!("tools/backlog/fixtures/queue.md");
        let (source, queue_source) = (
            server.folder.0.join("backlog.md"),
            server.folder.0.join("queue.md"),
        );
        std::fs::write(&source, backlog_md).unwrap();
        std::fs::write(&queue_source, queue_md).unwrap();
        let (source, queue_source) = (source.to_str().unwrap(), queue_source.to_str().unwrap());

        let dry = server.run("%40", &["import", "harold", source], "").await;
        let empty = server.run("%40", &["list", "harold"], "").await;
        let applied = server
            .run("%40", &["import", "harold", source, "--apply"], "")
            .await;
        let again = server
            .run("%40", &["import", "harold", source, "--apply"], "")
            .await;
        let exported = server.run("", &["export", "harold"], "").await.unwrap();

        let queue_dry = server
            .run("%2", &["queue", "import", queue_source], "")
            .await;
        let queue_applied = server
            .run("%2", &["queue", "import", queue_source, "--apply"], "")
            .await;
        let queue_again = server
            .run("%2", &["queue", "import", queue_source, "--apply"], "")
            .await;
        let queue_exported = server.run("", &["queue", "export"], "").await.unwrap();
        let missing = server
            .run("%40", &["import", "harold", "/no/such/backlog.md"], "")
            .await;
        println!(
            "{dry:?}\n{empty:?}\n{applied:?}\n{again:?}\n{queue_dry:?}\n{queue_applied:?}\n\
             {queue_again:?}\n{missing:?}\n{exported}\n--\n{queue_exported}"
        );

        let counts = "items 5, notes: rule 2, history 4, held 3, left_out 3";
        assert_eq!(dry, Ok(counts.into()));
        assert_eq!(empty, Ok("0 items".into()));
        assert_eq!(applied, Ok(format!("imported {counts}")));
        assert_eq!(again, Err("not_empty".into()));
        assert_eq!(content_lines(&exported), content_lines(backlog_md));
        assert_eq!(queue_dry, Ok("now 1, parked 4, done 2".into()));
        assert_eq!(queue_applied, Ok("imported now 1, parked 4, done 2".into()));
        assert_eq!(queue_again, Err("not_empty".into()));
        assert_eq!(content_lines(&queue_exported), content_lines(queue_md));
        assert!(
            missing
                .as_ref()
                .is_err_and(|line| line.starts_with("cannot read /no/such/backlog.md")),
            "{missing:?}"
        );
    }

    #[tokio::test]
    async fn a_bulk_call_names_thirty_seconds() {
        let folder = TempFolder::new();
        let source = folder.0.join("backlog.md");
        std::fs::write(&source, "# Backlog\n").unwrap();
        let (hung, stand_in) = never_answers().await;
        let wait = Duration::from_millis(100);
        let import = BacklogCommand::Import {
            product: "harold".into(),
            file: source,
            apply: true,
        };
        let list = BacklogCommand::List {
            product: "harold".into(),
            json: false,
        };
        let mut lines = Vec::new();
        for command in [import, list] {
            let line = tokio::time::timeout(
                GUARD,
                backlog_within(
                    &hung,
                    &command,
                    &b""[..],
                    "%40",
                    time::UtcOffset::UTC,
                    time::OffsetDateTime::now_utc(),
                    wait,
                ),
            )
            .await
            .expect("the call waited past its deadline");
            lines.push(line);
        }
        stand_in.abort();
        assert_eq!(
            lines,
            [
                Err("the Harold daemon did not answer within 30 seconds".to_string()),
                Err(NOT_ANSWERING.to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn an_older_daemon_without_the_tool_is_named() {
        let folder = TempFolder::new();
        let (_stop, shutdown) = tokio::sync::watch::channel(());
        let handle = crate::tools::questions::start(crate::tools::ToolContext {
            data_folder: folder.0.clone(),
            typing: Some(std::sync::Arc::new(Typist)),
            chief_pane: std::sync::Arc::new(NoChief),
            pane_folder: std::sync::Arc::new(NoFolder),
            products: std::sync::Arc::new(NoProducts),
            shutdown,
        })
        .await
        .unwrap_or_else(|e| panic!("start failed: {}", e.reason));
        let serving = serve(handle.routes).await;
        let line = backlog_within(
            &serving.address,
            &BacklogCommand::List {
                product: "harold".into(),
                json: false,
            },
            &b""[..],
            "%40",
            time::UtcOffset::UTC,
            time::OffsetDateTime::now_utc(),
            CALL_DEADLINE,
        )
        .await;
        assert_eq!(
            line,
            Err("the Harold daemon does not serve the backlog".into())
        );
    }

    #[tokio::test]
    async fn bad_stdin_json_is_one_line() {
        let put = BacklogCommand::Put {
            product: "harold".into(),
        };
        let note = BacklogCommand::NoteAdd {
            product: "harold".into(),
            kind: "rule".into(),
        };
        for (command, input) in [
            (&put, ""),
            (&put, r#"{"id":"#),
            (&put, r#"{"id":"a"}"#),
            (&put, r#"{"id":"a","title":"t","evidence":[1]}"#),
            (&note, ""),
            (&note, r#"{"title":"t"}"#),
        ] {
            // Against a closed port a call would say `not reachable`: no call is made.
            let line = backlog_within(
                CLOSED,
                command,
                input.as_bytes(),
                "%40",
                time::UtcOffset::UTC,
                time::OffsetDateTime::now_utc(),
                CALL_DEADLINE,
            )
            .await
            .unwrap_err();
            assert!(line.starts_with("invalid JSON: "), "{input:?}: {line}");
            assert!(!line.contains('\n'), "{line}");
        }
    }
}
