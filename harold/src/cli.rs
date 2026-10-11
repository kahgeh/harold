use std::time::Duration;

use crate::harold::{
    AgentState, ReportAgentStateRequest, WatchAgentStatesRequest, harold_client::HaroldClient,
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

impl Mode {
    pub(crate) fn parse(args: &[String]) -> Result<Self, String> {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        match args.as_slice() {
            [] => Ok(Self::Daemon),
            ["--help" | "-h"] => Ok(Self::Help),
            ["--check-config"] => Ok(Self::CheckConfig),
            ["--check-ready"] => Ok(Self::CheckReady),
            ["accept", rest @ ..] => parse_accept(rest).map(Self::Accept),
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
}
