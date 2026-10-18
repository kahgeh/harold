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
}

fn parse_tool(args: &[&str]) -> Result<ToolCommand, String> {
    match args {
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
    let address = settings.grpc.addr()?;
    let state = args.event.state();
    let response = tokio::time::timeout(Duration::from_secs(3), async {
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
            .map_err(|_| "the Harold daemon refused the call")
    })
    .await
    .map_err(|_| "the Harold daemon did not answer within 3 seconds")??;
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
    let address = settings
        .grpc
        .addr()
        .map_err(|_| "invalid configuration".to_string())?;
    let response = tokio::time::timeout(Duration::from_secs(3), async {
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
    .await
    .map_err(|_| "the Harold daemon did not answer within 3 seconds".to_string())??;
    if !response.into_inner().accepted {
        return Err(NOT_STORED_LINE.into());
    }
    Ok(())
}

/// Ask the daemon to say `args.text` or send it as a voice note. `Ok` means the daemon
/// stored the line, not that it was heard. `Err` is the one line to print.
pub(crate) async fn speak(settings: &Settings, args: &AwayMessageArgs) -> Result<(), String> {
    let address = settings
        .grpc
        .addr()
        .map_err(|_| "invalid configuration".to_string())?;
    tokio::time::timeout(Duration::from_secs(3), async {
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
    .await
    .map_err(|_| "the Harold daemon did not answer within 3 seconds".to_string())??;
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

/// Run a `harold tool questions` command against the daemon at `address`. `Ok` is the line
/// to print; `Err` is the one line to print for a refusal.
pub(crate) async fn tool(
    address: &str,
    command: &ToolCommand,
    mut stdin: impl std::io::Read,
) -> Result<String, String> {
    let question = match command {
        ToolCommand::QuestionsPut => {
            let mut text = String::new();
            stdin.read_to_string(&mut text).map_err(json_error)?;
            Some(question_from_json(&text)?)
        }
        _ => None,
    };
    tokio::time::timeout(Duration::from_secs(3), async {
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
        }
    })
    .await
    .map_err(|_| "the Harold daemon did not answer within 3 seconds".to_string())?
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

    #[tokio::test]
    async fn the_chief_puts_lists_orders_and_dismisses_through_the_tool() {
        let folder = std::env::temp_dir().join(format!("harold-cli-tool-{}", uuid::Uuid::new_v4()));
        let (stop, shutdown) = tokio::sync::watch::channel(());
        let handle = crate::tools::questions::start(crate::tools::ToolContext {
            data_folder: folder.clone(),
            typing: std::sync::Arc::new(Typist),
            chief_pane: std::sync::Arc::new(NoChief),
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
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        drop(listener);
        let line = tool(&address, &ToolCommand::QuestionsList, &b""[..]).await;
        assert_eq!(line, Err("the Harold daemon is not reachable".into()));
    }
}
