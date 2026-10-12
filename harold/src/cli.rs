use std::time::Duration;

use crate::agent::runtime::valid_pane_id;
use crate::away_message::{
    AGENT_NOT_FOUND, INVALID_MESSAGE_LENGTH, INVALID_SOURCE_ID, MESSAGING_PAUSED,
};
use crate::harold::{
    AgentState, ReportAgentStateRequest, SendMessageToAwayDeviceRequest, WatchAgentStatesRequest,
    harold_client::HaroldClient,
};
use crate::settings::Settings;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Daemon,
    Help,
    Diagnostics { delay_seconds: u64 },
    CheckConfig,
    CheckReady,
    Accept(AcceptArgs),
    SendMessageToAwayDevice(AwayMessageArgs),
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

/// The arguments after the command name. `Err` is the one line the command prints.
/// An argument that starts with `--` is a flag, so a text cannot start with `--`.
fn parse_away_message(args: &[&str]) -> Result<AwayMessageArgs, String> {
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
                _ => return Err(AWAY_MESSAGE_ARGUMENTS.into()),
            }
        } else if arg.starts_with("--") || text.is_some() {
            return Err(AWAY_MESSAGE_ARGUMENTS.into());
        } else {
            text = Some(*arg);
        }
    }
    let (Some(src_id), Some(text)) = (src_id, text) else {
        return Err(AWAY_MESSAGE_ARGUMENTS.into());
    };
    if !valid_pane_id(src_id) {
        return Err(SOURCE_ID_LINE.into());
    }
    // The daemon alone makes the text one line and checks its length.
    if text.trim().is_empty() {
        return Err(MESSAGE_LENGTH_LINE.into());
    }
    Ok(AwayMessageArgs {
        src_id: src_id.to_string(),
        text: text.to_string(),
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Mode, String> {
        Mode::parse(&args.iter().map(|a| a.to_string()).collect::<Vec<_>>())
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
}
