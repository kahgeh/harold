use std::time::Duration;

use crate::agent::runtime::valid_pane_id;
use crate::away_message::{
    AGENT_NOT_FOUND, INVALID_MESSAGE_LENGTH, INVALID_SOURCE_ID, MESSAGING_PAUSED,
    WRONG_MULTIPLEXER_PREFIX,
};
use crate::harold::{
    AgentState, GetHaroldStateRequest, GetHaroldStateResponse, ReportAgentStateRequest,
    SendMessageToAwayDeviceRequest, SpeakLineRequest, WatchAgentStatesRequest,
    harold_client::HaroldClient,
};
use crate::settings::Settings;
use harold_api::tools::backlog::v1 as backlog_api;
use harold_api::tools::backlog::v1::backlog_service_client::BacklogServiceClient;
use harold_api::tools::knowledge::v1 as knowledge_api;
use harold_api::tools::knowledge::v1::knowledge_service_client::KnowledgeServiceClient;
use harold_api::tools::questions::v1 as questions;
use harold_api::tools::questions::v1::question_service_client::QuestionServiceClient;
use harold_api::tools::release::v1 as release_api;
use harold_api::tools::release::v1::release_service_client::ReleaseServiceClient;
use harold_api::tools::telemetry::v1 as telemetry_api;
use harold_api::tools::telemetry::v1::telemetry_service_client::TelemetryServiceClient;
use harold_api::tools::watch::v1 as watch_api;
use harold_api::tools::watch::v1::watch_service_client::WatchServiceClient;
use harold_api::tools::work::v1 as work_api;
use harold_api::tools::work::v1::work_service_client::WorkServiceClient;
use harold_api::tts::v1 as tts_api;
use harold_api::tts::v1::tts_service_client::TtsServiceClient;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Daemon,
    Help,
    Diagnostics { delay_seconds: u64 },
    CheckConfig,
    CheckReady,
    BuildFeatures,
    Accept(AcceptArgs),
    Get(GetThing),
    SendMessageToAwayDevice(AwayMessageArgs),
    Speak(AwayMessageArgs),
    Tool(ToolCommand),
    App(AppCommand),
    StartControlTower(StartControlTowerArgs),
    Slaves(crate::slave::pairing::SlavesCommand),
    Master(crate::slave::pairing::MasterCommand),
}

/// A `harold app` command: the web app's pairing and its paired devices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AppCommand {
    /// Report whether pairing is set up and locked; `unlock` is done by `main` before the report.
    Pair {
        unlock: bool,
    },
    Devices,
    Unpair {
        name: String,
    },
}

pub(crate) const APP_COMMAND: &str = "app";
const APP_USAGE_LINE: &str =
    "invalid argument: usage is harold app pair [--unlock] | devices | unpair NAME";

fn parse_app(args: &[&str]) -> Result<AppCommand, String> {
    match args {
        ["pair"] => Ok(AppCommand::Pair { unlock: false }),
        ["pair", "--unlock"] => Ok(AppCommand::Pair { unlock: true }),
        ["devices"] => Ok(AppCommand::Devices),
        ["unpair", name] if !name.is_empty() && !name.starts_with("--") => Ok(AppCommand::Unpair {
            name: (*name).into(),
        }),
        _ => Err(APP_USAGE_LINE.into()),
    }
}

const SLAVES_USAGE_LINE: &str = "invalid argument: usage is harold slaves pair NAME | unpair NAME";
const MASTER_USAGE_LINE: &str =
    "invalid argument: usage is harold master pair SECRET --address HOST:PORT";

fn parse_slaves(args: &[&str]) -> Result<crate::slave::pairing::SlavesCommand, String> {
    use crate::slave::pairing::SlavesCommand;
    match args {
        ["pair", name] if !name.is_empty() && !name.starts_with('-') => Ok(SlavesCommand::Pair {
            name: (*name).into(),
        }),
        ["unpair", name] if !name.is_empty() && !name.starts_with('-') => {
            Ok(SlavesCommand::Unpair {
                name: (*name).into(),
            })
        }
        _ => Err(SLAVES_USAGE_LINE.into()),
    }
}

fn parse_master(args: &[&str]) -> Result<crate::slave::pairing::MasterCommand, String> {
    match args {
        ["pair", secret, "--address", address] if !secret.starts_with('-') => {
            Ok(crate::slave::pairing::MasterCommand::Pair {
                secret: (*secret).into(),
                address: (*address).into(),
            })
        }
        _ => Err(MASTER_USAGE_LINE.into()),
    }
}

pub(crate) const START_CONTROL_TOWER_COMMAND: &str = "start-control-tower";
const START_CONTROL_TOWER_USAGE: &str =
    "invalid argument: usage is harold start-control-tower [--remote] [--endpoint URI]";
/// The program that `harold start-control-tower` starts. It sits beside `harold`.
const CONTROL_TOWER_PROGRAM: &str = "control-tower";

/// The arguments of `harold start-control-tower`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StartControlTowerArgs {
    pub(crate) endpoint: Option<String>,
    pub(crate) remote: bool,
}

fn parse_start_control_tower(args: &[&str]) -> Result<StartControlTowerArgs, String> {
    let mut parsed = StartControlTowerArgs {
        endpoint: None,
        remote: false,
    };
    let mut rest = args.iter();
    while let Some(&argument) = rest.next() {
        match argument {
            "--remote" if !parsed.remote => parsed.remote = true,
            "--endpoint" if parsed.endpoint.is_none() => {
                let Some(&endpoint) = rest.next() else {
                    return Err(START_CONTROL_TOWER_USAGE.into());
                };
                parsed.endpoint = Some(endpoint.into());
            }
            _ => return Err(START_CONTROL_TOWER_USAGE.into()),
        }
    }
    Ok(parsed)
}

/// The command that starts the control-tower in `folder`. It never reads `PATH`.
fn control_tower_command(
    folder: &std::path::Path,
    settings: &Settings,
    args: &StartControlTowerArgs,
) -> Result<std::process::Command, String> {
    let program = folder.join(CONTROL_TOWER_PROGRAM);
    if !program.exists() {
        return Err(format!(
            "harold: no control-tower at {}. Install Harold again.",
            program.display()
        ));
    }
    let endpoint = match &args.endpoint {
        Some(endpoint) => endpoint.clone(),
        None => format!(
            "http://{}",
            settings
                .grpc
                .addr()
                .map_err(|_| "invalid configuration".to_string())?
        ),
    };
    let mut command = std::process::Command::new(program);
    if args.remote {
        command.arg("--remote");
    }
    command.args(["--endpoint", &endpoint]);
    command.args(["--host-marker", &settings.control_tower.host_marker]);
    Ok(command)
}

/// Replace this process with the control-tower beside the running `harold`.
/// It returns only the line to print, because a good `exec` does not return.
pub(crate) fn start_control_tower(
    settings: &Settings,
    args: &StartControlTowerArgs,
) -> Result<std::convert::Infallible, String> {
    use std::os::unix::process::CommandExt;
    let exe = std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .map_err(|error| format!("harold: cannot find the running program: {error}"))?;
    let folder = exe
        .parent()
        .ok_or_else(|| "harold: cannot find the running program's folder".to_string())?;
    let mut command = control_tower_command(folder, settings, args)?;
    let error = command.exec();
    Err(format!(
        "harold: cannot start the control-tower at {}: {error}",
        folder.join(CONTROL_TOWER_PROGRAM).display()
    ))
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
            Self::TurnStarted | Self::StillWorking => AgentState::Working,
            Self::TurnCompleted | Self::SubagentsFinished => AgentState::Done,
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
    "invalid argument: usage is harold tool questions put|dismiss|order|list|show|checked";
const RELEASE_USAGE_LINE: &str =
    "invalid argument: usage is harold tool release next [product] [--json]";
const SPEAK_ARGUMENTS: &str = "invalid argument: usage is speak --src-id ID TEXT";
const SPOKEN_LENGTH_LINE: &str = "the spoken line must be 1 to 300 characters";
const SPOKEN_LOOKUP_UNAVAILABLE_LINE: &str = "the agent lookup is not available now; try again";
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
        (tonic::Code::FailedPrecondition, message)
            if message.starts_with(WRONG_MULTIPLEXER_PREFIX) =>
        {
            format!("refused: {message}")
        }
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
        (tonic::Code::FailedPrecondition, message)
            if message.starts_with(WRONG_MULTIPLEXER_PREFIX) =>
        {
            format!("refused: {message}")
        }
        (tonic::Code::FailedPrecondition, AGENT_NOT_FOUND) => {
            format!("no agent is running in pane {src_id}")
        }
        // The daemon answers this when the inventory is unavailable, an event append failed
        // or the monitor runtime stopped.
        (tonic::Code::Unavailable, "agent lookup unavailable") => {
            SPOKEN_LOOKUP_UNAVAILABLE_LINE.into()
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
    QuestionsShow { id: String },
    QuestionsChecked,
    Backlog(BacklogCommand),
    Telemetry(TelemetryCommand),
    Watch(WatchCommand),
    Release(ReleaseCommand),
    Knowledge(KnowledgeCommand),
    Tts(TtsCommand),
    Work(WorkCommand),
}

const TTS_USAGE_LINE: &str = "invalid argument: usage is harold tool tts models | add REPO@REVISION | remove ID | default MODEL VOICE | speak TEXT";

/// How long a TTS call may take: the first line after a pause waits for the model to load.
const TTS_DEADLINE: Duration = Duration::from_secs(120);

/// A `harold tool tts` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TtsCommand {
    Models,
    Add { repo: String, revision: String },
    Remove { id: String },
    Default { model: String, voice: String },
    Speak { text: String },
}

fn parse_tts(args: &[&str]) -> Result<TtsCommand, String> {
    let usage = || TTS_USAGE_LINE.to_string();
    match args {
        ["models"] => Ok(TtsCommand::Models),
        ["add", spec] => {
            let (repo, revision) = spec.split_once('@').ok_or_else(usage)?;
            if repo.is_empty() || revision.is_empty() {
                return Err(usage());
            }
            Ok(TtsCommand::Add {
                repo: repo.into(),
                revision: revision.into(),
            })
        }
        ["remove", id] => Ok(TtsCommand::Remove { id: (*id).into() }),
        ["default", model, voice] => Ok(TtsCommand::Default {
            model: (*model).into(),
            voice: (*voice).into(),
        }),
        ["speak", words @ ..] if !words.is_empty() => Ok(TtsCommand::Speak {
            text: words.join(" "),
        }),
        _ => Err(usage()),
    }
}

/// The line printed for a refusal from the TTS server.
fn tts_refusal_line(status: &tonic::Status) -> String {
    match (status.code(), status.message()) {
        (tonic::Code::Unimplemented, _) => "the Harold daemon does not serve the TTS server".into(),
        (tonic::Code::PermissionDenied, "not_local") => {
            "refused: the call is not from this machine".into()
        }
        (tonic::Code::PermissionDenied, "not_approved") => "waiting for approval: the model is not on the approved list (tts-models.toml); the chief has it audited".into(),
        (_, "invalid_revision") => "invalid revision: use 40 lowercase hex characters".into(),
        (_, "invalid_repo") => "invalid repository: use ORG/NAME".into(),
        (_, "unsupported_source") => "unsupported source: only huggingface".into(),
        (_, "unsupported_worker") => "this machine's TTS worker cannot run that model".into(),
        (_, "unsafe_format") => "refused: the model has a weight file that can run code (.bin, .pt, .pth or .pkl)".into(),
        (_, "default_model") => "the default model cannot be removed".into(),
        (_, "unknown_model") => "unknown model: run harold tool tts models".into(),
        (_, "unknown_voice") => "unknown voice for that model".into(),
        (_, "model_not_ready") => "the model is not ready: it waits for approval".into(),
        (_, "model_downloading") => "the TTS model is still downloading: try again soon".into(),
        (_, "model_missing") => "the TTS model is not on this machine: run harold tool tts models".into(),
        (_, "voice_muted") => "the voice is muted: nothing was said (unmute it in the control-tower)".into(),
        (_, "worker_failed") => "the TTS worker failed: see harold.log".into(),
        (_, "timed_out") => "the TTS worker did not answer in time".into(),
        (_, "invalid_text") => "the text must be 1 to 4,000 characters".into(),
        (_, "") => "the Harold daemon refused the call".into(),
        (_, message) => message.to_string(),
    }
}

fn tts_state_word(state: i32) -> &'static str {
    match tts_api::ModelState::try_from(state) {
        Ok(tts_api::ModelState::Ready) => "ready",
        Ok(tts_api::ModelState::Downloading) => "downloading",
        Ok(tts_api::ModelState::Failed) => "failed",
        Ok(tts_api::ModelState::WaitingForApproval) => "waiting_for_approval",
        _ => "unknown",
    }
}

/// One line for a model: the default mark, its id, its state and reason, its worker, size and
/// voices.
fn tts_model_line(model: &tts_api::Model) -> String {
    let mark = if model.is_default { "*" } else { " " };
    let reason = if model.reason.is_empty() {
        String::new()
    } else {
        format!(" ({})", model.reason)
    };
    let voice = if model.default_voice.is_empty() {
        String::new()
    } else {
        format!(", default voice {}", model.default_voice)
    };
    let voices = if model.voices.is_empty() {
        String::new()
    } else {
        format!(", voices {}", model.voices.join(" "))
    };
    format!(
        "{mark} {} {}{reason} {} {}@{} {} MB{voice}{voices}",
        model.id,
        tts_state_word(model.state),
        model.worker,
        model.source,
        model.revision,
        model.size_bytes / 1_000_000,
    )
}

/// Runs a `harold tool tts` command. `Ok` is the text to print; `Err` the one line. `speak`
/// plays the line with `player` at the desk.
pub(crate) async fn tts_within(
    address: &str,
    command: &TtsCommand,
    wait: Duration,
    player: &str,
) -> Result<String, String> {
    let connect = || async {
        TtsServiceClient::connect(format!("http://{address}"))
            .await
            .map_err(|_| "the Harold daemon is not reachable".to_string())
    };
    if let TtsCommand::Speak { text } = command {
        let audio = call_within(TTS_DEADLINE, wait, async {
            let mut client = connect().await?;
            let mut stream = client
                .speak(tts_api::SpeakRequest {
                    text: text.clone(),
                    model: String::new(),
                    voice: String::new(),
                })
                .await
                .map_err(|status| tts_refusal_line(&status))?
                .into_inner();
            let mut audio = crate::tts::Audio {
                sample_rate: 0,
                pcm: Vec::new(),
            };
            while let Some(chunk) = stream
                .message()
                .await
                .map_err(|status| tts_refusal_line(&status))?
            {
                if chunk.sample_rate != 0 {
                    audio.sample_rate = chunk.sample_rate;
                }
                audio.pcm.extend(chunk.pcm);
            }
            Ok::<_, String>(audio)
        })
        .await?;
        // The playing is not part of the daemon's wait.
        let wav = crate::tts::play::TempWav::write(&std::env::temp_dir(), &audio)
            .map_err(|_| "the line could not be written to play".to_string())?;
        let played = tokio::process::Command::new(player)
            .arg(wav.path())
            .status()
            .await
            .map_err(|_| format!("{player} could not start"))?;
        return if played.success() {
            Ok("said".into())
        } else {
            Err(format!("{player} failed"))
        };
    }
    call_within(TTS_DEADLINE, wait, async {
        let mut client = connect().await?;
        let refused = |status: tonic::Status| tts_refusal_line(&status);
        match command {
            TtsCommand::Models => {
                let models = client
                    .list_models(tts_api::ListModelsRequest {})
                    .await
                    .map_err(refused)?
                    .into_inner()
                    .models;
                Ok(models
                    .iter()
                    .map(tts_model_line)
                    .collect::<Vec<_>>()
                    .join("\n"))
            }
            TtsCommand::Add { repo, revision } => {
                let added = client
                    .add_model(tts_api::AddModelRequest {
                        source: "huggingface".into(),
                        repo: repo.clone(),
                        revision: revision.clone(),
                    })
                    .await
                    .map_err(refused)?
                    .into_inner();
                Ok(format!("{} {}", added.id, tts_state_word(added.state)))
            }
            TtsCommand::Remove { id } => {
                client
                    .remove_model(tts_api::RemoveModelRequest { id: id.clone() })
                    .await
                    .map_err(refused)?;
                Ok(format!("removed {id}"))
            }
            TtsCommand::Default { model, voice } => {
                client
                    .set_default(tts_api::SetDefaultRequest {
                        model: model.clone(),
                        voice: voice.clone(),
                    })
                    .await
                    .map_err(refused)?;
                Ok(format!("default is {model} {voice}"))
            }
            TtsCommand::Speak { .. } => Err(TTS_USAGE_LINE.into()),
        }
    })
    .await
}

const KNOWLEDGE_USAGE_LINE: &str = "invalid argument: usage is harold tool knowledge search WORDS [--limit N] | recent [--limit N] | related NOTE [--limit N] | links NOTE | show NOTE | status | rebuild | relate FROM RELATION TO | unrelate FROM RELATION TO | relations NOTE | graph status | graph rebuild | graph void SEQUENCE --reason WORD";

/// How long a knowledge call may take: the first search after a pause waits for the
/// embedding model to load, and a rebuild embeds the whole vault.
const KNOWLEDGE_DEADLINE: Duration = Duration::from_secs(180);

/// A `harold tool knowledge` command. `NOTE` is a vault note's name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum KnowledgeCommand {
    Search {
        query: String,
        limit: u32,
    },
    Recent {
        limit: u32,
    },
    Related {
        name: String,
        limit: u32,
    },
    Links {
        name: String,
    },
    Show {
        name: String,
    },
    Status,
    Rebuild,
    Relate {
        from: String,
        relation: String,
        to: String,
    },
    Unrelate {
        from: String,
        relation: String,
        to: String,
    },
    Relations {
        name: String,
    },
    GraphStatus,
    GraphRebuild,
    GraphVoid {
        sequence: u64,
        reason: String,
    },
}

/// Splits `--limit N` off the end of a command's arguments.
fn split_limit<'a>(args: &'a [&'a str]) -> Result<(&'a [&'a str], u32), String> {
    match args {
        [rest @ .., "--limit", n] => n
            .parse::<u32>()
            .ok()
            .filter(|n| (1..=10).contains(n))
            .map(|n| (rest, n))
            .ok_or_else(|| KNOWLEDGE_USAGE_LINE.to_string()),
        _ => Ok((args, 10)),
    }
}

fn parse_knowledge(args: &[&str]) -> Result<KnowledgeCommand, String> {
    let usage = || KNOWLEDGE_USAGE_LINE.to_string();
    match args {
        ["search", rest @ ..] => {
            let (words, limit) = split_limit(rest)?;
            let query = words.join(" ");
            if query.trim().is_empty() {
                return Err(usage());
            }
            Ok(KnowledgeCommand::Search { query, limit })
        }
        ["recent", rest @ ..] => {
            let (extra, limit) = split_limit(rest)?;
            if !extra.is_empty() {
                return Err(usage());
            }
            Ok(KnowledgeCommand::Recent { limit })
        }
        ["related", name, rest @ ..] => {
            let (extra, limit) = split_limit(rest)?;
            if !extra.is_empty() {
                return Err(usage());
            }
            Ok(KnowledgeCommand::Related {
                name: (*name).into(),
                limit,
            })
        }
        ["links", name] => Ok(KnowledgeCommand::Links {
            name: (*name).into(),
        }),
        ["show", name] => Ok(KnowledgeCommand::Show {
            name: (*name).into(),
        }),
        ["status"] => Ok(KnowledgeCommand::Status),
        ["rebuild"] => Ok(KnowledgeCommand::Rebuild),
        ["relate", from, relation, to] => Ok(KnowledgeCommand::Relate {
            from: (*from).into(),
            relation: (*relation).into(),
            to: (*to).into(),
        }),
        ["unrelate", from, relation, to] => Ok(KnowledgeCommand::Unrelate {
            from: (*from).into(),
            relation: (*relation).into(),
            to: (*to).into(),
        }),
        ["relations", name] => Ok(KnowledgeCommand::Relations {
            name: (*name).into(),
        }),
        ["graph", "status"] => Ok(KnowledgeCommand::GraphStatus),
        ["graph", "rebuild"] => Ok(KnowledgeCommand::GraphRebuild),
        ["graph", "void", sequence, "--reason", reason] => Ok(KnowledgeCommand::GraphVoid {
            sequence: sequence.parse().map_err(|_| usage())?,
            reason: (*reason).into(),
        }),
        _ => Err(usage()),
    }
}

/// One line per hit: score, name, kind and description. A list with no score shows `-`.
fn knowledge_hit_line(hit: &knowledge_api::Hit, scored: bool) -> String {
    let score = if scored {
        format!("{:.2}", hit.score)
    } else {
        "-".into()
    };
    let linked = if hit.linked { " (linked)" } else { "" };
    format!(
        "{score} {} {}{linked} {}",
        hit.name, hit.kind, hit.description
    )
}

/// The line printed for a refusal from the knowledge service. `command` is the caller's.
fn knowledge_refusal_line(status: &tonic::Status, command: &KnowledgeCommand) -> String {
    let name = match command {
        KnowledgeCommand::Related { name, .. }
        | KnowledgeCommand::Links { name }
        | KnowledgeCommand::Relations { name }
        | KnowledgeCommand::Show { name } => name.as_str(),
        _ => "",
    };
    match (status.code(), status.message()) {
        (tonic::Code::Unimplemented, _) => {
            "the Harold daemon does not serve the knowledge tool".into()
        }
        (tonic::Code::NotFound, "unknown_vault_note") if name.is_empty() => {
            "unknown vault note: one of the two is not in the graph".into()
        }
        (tonic::Code::NotFound, "unknown_vault_note") => format!("unknown vault note {name}"),
        (tonic::Code::Unavailable, "graph_off") => {
            "the knowledge graph is off: set [knowledge.graph] enabled".into()
        }
        (tonic::Code::Unavailable, "graph_behind") => {
            "the knowledge graph is behind its logs: run harold tool knowledge graph status".into()
        }
        (tonic::Code::Unavailable, "graph_rebuilding") => {
            "the knowledge graph is rebuilding: try again soon".into()
        }
        (tonic::Code::Unavailable, "log_failed") => {
            "the knowledge graph could not write its log, so nothing changed".into()
        }
        (tonic::Code::Unavailable, "typedb_unreachable") => {
            "TypeDB did not answer: a change already logged applies on the next call".into()
        }
        (tonic::Code::FailedPrecondition, "not_refused") => {
            "that record was not refused: only the record the catch-up stopped at can be voided"
                .into()
        }
        (tonic::Code::Unavailable, message) if message.starts_with("search_resting") => {
            "search is resting: the model didn't load".into()
        }
        (tonic::Code::Unavailable, _) => "the knowledge index is unavailable".into(),
        (tonic::Code::InvalidArgument, message) => message.into(),
        _ => "the Harold daemon refused the call".into(),
    }
}

/// Runs a `harold tool knowledge` command. `Ok` is the text to print; `Err` the one line.
pub(crate) async fn knowledge_within(
    address: &str,
    command: &KnowledgeCommand,
    wait: Duration,
) -> Result<String, String> {
    call_within(KNOWLEDGE_DEADLINE, wait, async {
        let mut client = KnowledgeServiceClient::connect(format!("http://{address}"))
            .await
            .map_err(|_| "the Harold daemon is not reachable".to_string())?;
        let refused = |status: tonic::Status| knowledge_refusal_line(&status, command);
        let lines = |hits: &[knowledge_api::Hit], scored: bool| {
            if hits.is_empty() {
                return "no vault notes".to_string();
            }
            hits.iter()
                .map(|hit| knowledge_hit_line(hit, scored))
                .collect::<Vec<_>>()
                .join("\n")
        };
        match command {
            KnowledgeCommand::Search { query, limit } => {
                let found = client
                    .search(knowledge_api::SearchRequest {
                        query: query.clone(),
                        limit: *limit,
                        source: "chief".into(),
                    })
                    .await
                    .map_err(refused)?
                    .into_inner();
                Ok(lines(&found.hits, true))
            }
            KnowledgeCommand::Recent { limit } => {
                let found = client
                    .recent(knowledge_api::RecentRequest { limit: *limit })
                    .await
                    .map_err(refused)?
                    .into_inner();
                Ok(lines(&found.hits, false))
            }
            KnowledgeCommand::Related { name, limit } => {
                let found = client
                    .related(knowledge_api::RelatedRequest {
                        name: name.clone(),
                        limit: *limit,
                    })
                    .await
                    .map_err(refused)?
                    .into_inner();
                Ok(lines(&found.hits, true))
            }
            KnowledgeCommand::Links { name } => {
                let found = client
                    .suggest_links(knowledge_api::SuggestLinksRequest { name: name.clone() })
                    .await
                    .map_err(refused)?
                    .into_inner();
                Ok(lines(&found.hits, true))
            }
            KnowledgeCommand::Show { name } => {
                let note = client
                    .show_vault_note(knowledge_api::ShowVaultNoteRequest { name: name.clone() })
                    .await
                    .map_err(refused)?
                    .into_inner();
                Ok(format!(
                    "name: {}\ntitle: {}\nkind: {}\ndescription: {}\nlinks: {}\n\n{}",
                    note.name,
                    note.title,
                    note.kind,
                    note.description,
                    note.links.join(", "),
                    note.text.trim_end()
                ))
            }
            KnowledgeCommand::Status => {
                let status = client
                    .status(knowledge_api::StatusRequest {})
                    .await
                    .map_err(refused)?
                    .into_inner();
                Ok(knowledge_status_text(&status))
            }
            KnowledgeCommand::Rebuild => {
                let built = client
                    .rebuild(knowledge_api::RebuildRequest {})
                    .await
                    .map_err(refused)?
                    .into_inner();
                Ok(format!(
                    "rebuilt {} vault notes, {} passages",
                    built.vault_notes, built.passages
                ))
            }
            KnowledgeCommand::Relate { from, relation, to } => {
                let done = client
                    .relate(knowledge_api::RelateRequest {
                        from: from.clone(),
                        relation: relation.clone(),
                        to: to.clone(),
                    })
                    .await
                    .map_err(refused)?
                    .into_inner();
                Ok(if done.changed {
                    format!("related {from} {relation} {to}")
                } else {
                    format!("already related {from} {relation} {to}")
                })
            }
            KnowledgeCommand::Unrelate { from, relation, to } => {
                let done = client
                    .unrelate(knowledge_api::RelateRequest {
                        from: from.clone(),
                        relation: relation.clone(),
                        to: to.clone(),
                    })
                    .await
                    .map_err(refused)?
                    .into_inner();
                Ok(if done.changed {
                    format!("unrelated {from} {relation} {to}")
                } else {
                    format!("no relation {from} {relation} {to}")
                })
            }
            KnowledgeCommand::Relations { name } => {
                let found = client
                    .relations_of(knowledge_api::RelationsOfRequest {
                        vault_note: name.clone(),
                    })
                    .await
                    .map_err(refused)?
                    .into_inner();
                Ok(knowledge_relations_text(&found.relations))
            }
            KnowledgeCommand::GraphStatus => {
                let status = client
                    .graph_status(knowledge_api::GraphStatusRequest {})
                    .await
                    .map_err(refused)?
                    .into_inner();
                Ok(knowledge_graph_status_text(&status))
            }
            KnowledgeCommand::GraphRebuild => {
                let built = client
                    .rebuild_graph(knowledge_api::RebuildGraphRequest {})
                    .await
                    .map_err(refused)?
                    .into_inner();
                Ok(format!(
                    "rebuilt the knowledge graph: {} schema {}, {} data records, {} voided, {} vault notes, {} links, {} typed relations, {:.1} s",
                    built.schema_steps,
                    if built.schema_steps == 1 { "step" } else { "steps" },
                    built.records,
                    built.voided,
                    built.vault_notes,
                    built.links,
                    built.typed_relations,
                    built.ms as f64 / 1000.0
                ))
            }
            KnowledgeCommand::GraphVoid { sequence, reason } => {
                let done = client
                    .void_graph_record(knowledge_api::VoidGraphRecordRequest {
                        sequence: *sequence,
                        reason: reason.clone(),
                    })
                    .await
                    .map_err(refused)?
                    .into_inner();
                Ok(format!(
                    "voided record {sequence} (void record {})",
                    done.void_sequence
                ))
            }
        }
    })
    .await
}

/// One line per relation: the direction, the relation, the other vault note and its title.
fn knowledge_relations_text(relations: &[knowledge_api::GraphRelation]) -> String {
    if relations.is_empty() {
        return "no relations".into();
    }
    relations
        .iter()
        .map(|relation| {
            let arrow = if relation.outgoing { "->" } else { "<-" };
            let missing = if relation.missing { " (missing)" } else { "" };
            format!(
                "{arrow} {} {}{missing} {}",
                relation.relation, relation.name, relation.title
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn knowledge_graph_status_text(status: &knowledge_api::GraphStatusResponse) -> String {
    if !status.enabled {
        return "the knowledge graph is off".into();
    }
    let sequence = |has: bool, value: u64| {
        if has {
            value.to_string()
        } else {
            "none".into()
        }
    };
    let mut lines = vec![
        format!(
            "position: schema {}, data {}",
            sequence(status.has_schema_position, status.schema_position),
            sequence(status.has_data_position, status.data_position)
        ),
        format!(
            "log ends: schema {}, data {}",
            sequence(status.has_schema_end, status.schema_end),
            sequence(status.has_data_end, status.data_end)
        ),
        format!("records waiting: {}", status.waiting),
    ];
    if status.has_last_refusal {
        lines.push(format!(
            "last refusal: record {} ({})",
            status.last_refusal_sequence, status.last_refusal_reason
        ));
    }
    lines.join("\n")
}

fn knowledge_status_text(status: &knowledge_api::StatusResponse) -> String {
    let last = if status.last_ok_ms == 0 {
        "no pass has succeeded yet".to_string()
    } else {
        let now = time::OffsetDateTime::now_utc().unix_timestamp() * 1000;
        format!("{} s ago", ((now - status.last_ok_ms) / 1000).max(0))
    };
    let mut lines = vec![
        format!("vault notes: {}", status.vault_notes),
        format!("passages: {}", status.passages),
        format!("last good pass: {last}"),
        format!("pending: {}", status.pending),
        format!(
            "runtime: {} {} ({} dimensions)",
            status.runtime, status.model, status.dimensions
        ),
        format!("worker: {}", status.worker),
    ];
    if status.vault_unreadable {
        lines.push("the vault cannot be read: searches answer from the last index".into());
    } else if !status.last_failure.is_empty() {
        lines.push(format!("last pass failed: {}", status.last_failure));
    }
    lines.join("\n")
}

const WATCH_USAGE_LINE: &str = "invalid argument: usage is harold tool watch start NAME --skill SKILL (--every MINUTES | --cron \"EXPR\") [--for TIME] [--report-to chief|self] [--run-budget USD] [--run-limit SECONDS] [--every-run] | stop NAME | list | pause (NAME | --all) [--for TIME] | resume (NAME | --all) | remind NAME (--in TIME | --at HH:MM | --cron \"EXPR\") [--report-to chief|self] [--] \"TEXT\"";

/// A `harold tool watch` command. The numbers stay text here: the daemon checks the
/// ranges, and the CLI only needs them to be numbers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WatchCommand {
    Start {
        name: String,
        skill: String,
        schedule: WatchSchedule,
        for_minutes: Option<u32>,
        report_to: Option<String>,
        run_budget_usd: Option<String>,
        run_limit_secs: Option<u32>,
        every_run: bool,
    },
    Stop {
        name: String,
    },
    List,
    Pause {
        name: Option<String>,
        all: bool,
        for_minutes: Option<u32>,
    },
    Resume {
        name: Option<String>,
        all: bool,
    },
    Remind {
        name: String,
        when: RemindWhen,
        report_to: Option<String>,
        text: String,
    },
}

/// When a watch runs: every some minutes, or at the minutes of a cron expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WatchSchedule {
    Every(u32),
    /// The daemon checks the expression.
    Cron(String),
}

/// When a reminder is due: in some minutes, at a local time of day, or at the next
/// minute that a cron expression matches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RemindWhen {
    In(u32),
    At(String),
    Cron(String),
}

/// Reads a time span: `90` and `90m` are minutes, `2h` is hours. The daemon checks the
/// range. `field` names the flag in the error line.
pub(crate) fn parse_minutes(text: &str, field: &str) -> Result<u32, String> {
    let invalid = || format!("invalid {field}");
    let (digits, hours) = match text.strip_suffix('h') {
        Some(digits) => (digits, true),
        None => (text.strip_suffix('m').unwrap_or(text), false),
    };
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid());
    }
    let number = digits.parse::<u32>().map_err(|_| invalid())?;
    if hours {
        number.checked_mul(60).ok_or_else(invalid)
    } else {
        Ok(number)
    }
}

/// A `harold tool release` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReleaseCommand {
    Next { product: Option<String>, json: bool },
}

fn parse_release(args: &[&str]) -> Result<ReleaseCommand, String> {
    let ["next", rest @ ..] = args else {
        return Err(RELEASE_USAGE_LINE.into());
    };
    let mut product = None;
    let mut json = false;
    for arg in rest {
        match *arg {
            "--json" if !json => json = true,
            name if !name.starts_with('-') && product.is_none() => product = Some(name.into()),
            _ => return Err(RELEASE_USAGE_LINE.into()),
        }
    }
    Ok(ReleaseCommand::Next { product, json })
}

fn parse_watch(args: &[&str]) -> Result<WatchCommand, String> {
    match args {
        ["list"] => Ok(WatchCommand::List),
        ["stop", name] => Ok(WatchCommand::Stop {
            name: (*name).into(),
        }),
        ["start", name, flags @ ..] => parse_watch_start(name, flags),
        ["pause", rest @ ..] => parse_watch_hold(rest, true),
        ["resume", rest @ ..] => parse_watch_hold(rest, false),
        ["remind", name, rest @ ..] if !name.starts_with('-') => parse_watch_remind(name, rest),
        _ => Err(WATCH_USAGE_LINE.into()),
    }
}

/// Reads `pause` or `resume`: one name or `--all`, and for `pause` an optional `--for`.
fn parse_watch_hold(args: &[&str], pausing: bool) -> Result<WatchCommand, String> {
    let (mut name, mut all, mut minutes) = (None::<String>, false, None::<String>);
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match *arg {
            "--all" if !all => all = true,
            "--for" if pausing && minutes.is_none() => {
                let value = rest.next().ok_or_else(|| WATCH_USAGE_LINE.to_string())?;
                minutes = Some((*value).to_string());
            }
            other if !other.starts_with('-') && name.is_none() => name = Some(other.into()),
            _ => return Err(WATCH_USAGE_LINE.into()),
        }
    }
    if all == name.is_some() {
        return Err(WATCH_USAGE_LINE.into());
    }
    if !pausing {
        return Ok(WatchCommand::Resume { name, all });
    }
    let for_minutes = minutes
        .map(|text| parse_minutes(&text, "for_minutes"))
        .transpose()?;
    Ok(WatchCommand::Pause {
        name,
        all,
        for_minutes,
    })
}

/// Reads `remind NAME (--in TIME | --at HH:MM | --cron EXPR) [--report-to chief|self] TEXT`.
fn parse_watch_remind(name: &str, args: &[&str]) -> Result<WatchCommand, String> {
    let (mut within, mut at, mut report_to) = (None::<String>, None::<String>, None);
    let mut cron = None::<String>;
    let mut text = None::<String>;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        let slot = match *arg {
            // After `--` the next argument is the text, even if it starts with `-`.
            "--" if text.is_none() => {
                text = Some((*rest.next().ok_or_else(|| WATCH_USAGE_LINE.to_string())?).into());
                continue;
            }
            "--in" => &mut within,
            "--at" => &mut at,
            "--cron" => &mut cron,
            "--report-to" => &mut report_to,
            other if !other.starts_with('-') && text.is_none() => {
                text = Some(other.into());
                continue;
            }
            _ => return Err(WATCH_USAGE_LINE.into()),
        };
        let value = rest.next().ok_or_else(|| WATCH_USAGE_LINE.to_string())?;
        if slot.replace((*value).to_string()).is_some() {
            return Err(WATCH_USAGE_LINE.into());
        }
    }
    let when = match (within, at, cron) {
        (Some(minutes), None, None) => RemindWhen::In(parse_minutes(&minutes, "in_minutes")?),
        (None, Some(time), None) => RemindWhen::At(time),
        (None, None, Some(cron)) => RemindWhen::Cron(cron),
        _ => return Err(WATCH_USAGE_LINE.into()),
    };
    Ok(WatchCommand::Remind {
        name: name.into(),
        when,
        report_to,
        text: text.ok_or_else(|| WATCH_USAGE_LINE.to_string())?,
    })
}

fn parse_watch_start(name: &str, flags: &[&str]) -> Result<WatchCommand, String> {
    let (mut skill, mut every, mut minutes) = (None, None, None);
    let (mut report_to, mut budget, mut limit) = (None, None, None);
    let mut cron = None;
    let mut every_run = false;
    let mut rest = flags.iter();
    while let Some(flag) = rest.next() {
        let slot = match *flag {
            "--every-run" if !every_run => {
                every_run = true;
                continue;
            }
            "--skill" => &mut skill,
            "--every" => &mut every,
            "--cron" => &mut cron,
            "--for" => &mut minutes,
            "--report-to" => &mut report_to,
            "--run-budget" => &mut budget,
            "--run-limit" => &mut limit,
            _ => return Err(WATCH_USAGE_LINE.into()),
        };
        let value = rest.next().ok_or_else(|| WATCH_USAGE_LINE.to_string())?;
        if slot.replace((*value).to_string()).is_some() {
            return Err(WATCH_USAGE_LINE.into());
        }
    }
    if let Some(text) = &budget
        && !text
            .parse::<f64>()
            .is_ok_and(|budget| budget.is_finite() && budget > 0.0)
    {
        return Err("invalid run_budget_usd".into());
    }
    let run_limit_secs = match limit {
        Some(text) => match text.parse::<u32>() {
            Ok(secs) if secs > 0 => Some(secs),
            _ => return Err("invalid run_limit_secs".into()),
        },
        None => None,
    };
    let skill = skill.ok_or_else(|| WATCH_USAGE_LINE.to_string())?;
    // Exactly one of `--every` and `--cron`.
    let schedule = match (every, cron) {
        (Some(every), None) => WatchSchedule::Every(
            every
                .parse::<u32>()
                .map_err(|_| "invalid every_minutes".to_string())?,
        ),
        (None, Some(cron)) => WatchSchedule::Cron(cron),
        _ => return Err(WATCH_USAGE_LINE.into()),
    };
    Ok(WatchCommand::Start {
        name: name.into(),
        skill,
        schedule,
        for_minutes: minutes
            .map(|text| parse_minutes(&text, "for_minutes"))
            .transpose()?,
        report_to,
        run_budget_usd: budget,
        run_limit_secs,
        every_run,
    })
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

const TELEMETRY_USAGE_LINE: &str =
    "invalid argument: usage is harold tool telemetry declare|count|gauge|event|list|remove";

/// The kind of a declared signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TelemetryKind {
    Counter,
    Gauge,
    Event,
}

impl TelemetryKind {
    fn proto(self) -> telemetry_api::Kind {
        match self {
            Self::Counter => telemetry_api::Kind::Counter,
            Self::Gauge => telemetry_api::Kind::Gauge,
            Self::Event => telemetry_api::Kind::Event,
        }
    }
}

/// A gauge value. Two values are equal when their bits are equal, so a command can be
/// compared.
#[derive(Debug, Clone, Copy)]
pub(crate) struct GaugeValue(pub(crate) f64);

impl PartialEq for GaugeValue {
    fn eq(&self, other: &Self) -> bool {
        self.0.to_bits() == other.0.to_bits()
    }
}

impl Eq for GaugeValue {}

/// A `harold tool telemetry` command. The CLI checks only the shape of the command; the
/// daemon checks names, prefixes, kinds and declared values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TelemetryCommand {
    Declare {
        name: String,
        kind: TelemetryKind,
        meaning: String,
        attrs: Vec<(String, Vec<String>)>,
    },
    Count {
        name: String,
        n: u64,
        attrs: Vec<(String, String)>,
    },
    Gauge {
        name: String,
        value: GaugeValue,
        attrs: Vec<(String, String)>,
    },
    Event {
        name: String,
        attrs: Vec<(String, String)>,
    },
    List {
        prefix: Option<String>,
    },
    Remove {
        name: String,
    },
}

/// The `key=value` pairs of a command's `--attr` flags, in the given order.
type TelemetryPairs = Vec<(String, String)>;

/// The positional words before the first flag, and the `key=value` of each `--attr` after
/// them. A flag other than `--attr`, an `--attr` with no value, no `=` or an empty key, a
/// repeated key, and a word after a flag give the usage line.
fn telemetry_args<'a>(args: &[&'a str]) -> Result<(Vec<&'a str>, TelemetryPairs), String> {
    let usage = || TELEMETRY_USAGE_LINE.to_string();
    let first_flag = args
        .iter()
        .position(|arg| arg.starts_with("--"))
        .unwrap_or(args.len());
    let (words, flags) = args.split_at(first_flag);
    let mut attrs: Vec<(String, String)> = Vec::new();
    let mut rest = flags.iter();
    while let Some(flag) = rest.next() {
        let (Some(attr), true) = (rest.next(), *flag == "--attr") else {
            return Err(usage());
        };
        let Some((key, value)) = attr.split_once('=') else {
            return Err(usage());
        };
        if key.is_empty() || attrs.iter().any(|(known, _)| known == key) {
            return Err(usage());
        }
        attrs.push((key.into(), value.into()));
    }
    Ok((words.to_vec(), attrs))
}

fn parse_telemetry(args: &[&str]) -> Result<TelemetryCommand, String> {
    use TelemetryCommand as C;
    let usage = || TELEMETRY_USAGE_LINE.to_string();
    let (words, attrs) = telemetry_args(args)?;
    let no_attrs = |command: C| match attrs.is_empty() {
        true => Ok(command),
        false => Err(usage()),
    };
    match words.as_slice() {
        ["declare", name, kind, meaning] => {
            let kind = match *kind {
                "counter" => TelemetryKind::Counter,
                "gauge" => TelemetryKind::Gauge,
                "event" => TelemetryKind::Event,
                _ => return Err(usage()),
            };
            Ok(C::Declare {
                name: (*name).into(),
                kind,
                meaning: (*meaning).into(),
                attrs: attrs
                    .into_iter()
                    .map(|(key, values)| (key, values.split(',').map(String::from).collect()))
                    .collect(),
            })
        }
        ["count", name, rest @ ..] => {
            let n = match rest {
                [] => 1,
                [n] => match n.parse::<u64>() {
                    Ok(0) | Err(_) => return Err(usage()),
                    Ok(n) => n,
                },
                _ => return Err(usage()),
            };
            Ok(C::Count {
                name: (*name).into(),
                n,
                attrs,
            })
        }
        ["gauge", name, value] => Ok(C::Gauge {
            name: (*name).into(),
            value: GaugeValue(value.parse().map_err(|_| usage())?),
            attrs,
        }),
        ["event", name] => Ok(C::Event {
            name: (*name).into(),
            attrs,
        }),
        ["list"] => no_attrs(C::List { prefix: None }),
        ["list", prefix] => no_attrs(C::List {
            prefix: Some((*prefix).into()),
        }),
        ["remove", name] => no_attrs(C::Remove {
            name: (*name).into(),
        }),
        _ => Err(usage()),
    }
}

fn parse_tool(args: &[&str]) -> Result<ToolCommand, String> {
    match args {
        ["backlog", rest @ ..] => parse_backlog(rest).map(ToolCommand::Backlog),
        ["telemetry", rest @ ..] => parse_telemetry(rest).map(ToolCommand::Telemetry),
        ["watch", rest @ ..] => parse_watch(rest).map(ToolCommand::Watch),
        ["release", rest @ ..] => parse_release(rest).map(ToolCommand::Release),
        ["knowledge", rest @ ..] => parse_knowledge(rest).map(ToolCommand::Knowledge),
        ["tts", rest @ ..] => parse_tts(rest).map(ToolCommand::Tts),
        ["work", rest @ ..] => parse_work(rest).map(ToolCommand::Work),
        ["questions", "put"] => Ok(ToolCommand::QuestionsPut),
        ["questions", "list"] => Ok(ToolCommand::QuestionsList),
        ["questions", "checked"] => Ok(ToolCommand::QuestionsChecked),
        ["questions", "show", id] => Ok(ToolCommand::QuestionsShow { id: (*id).into() }),
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
            ["--build-features"] => Ok(Self::BuildFeatures),
            ["accept", rest @ ..] => parse_accept(rest).map(Self::Accept),
            [GET_COMMAND, rest @ ..] => parse_get(rest).map(Self::Get),
            [AWAY_MESSAGE_COMMAND, rest @ ..] => {
                parse_away_message(rest).map(Self::SendMessageToAwayDevice)
            }
            [SPEAK_COMMAND, rest @ ..] => {
                parse_source_and_text(rest, SPEAK_ARGUMENTS, SPOKEN_LENGTH_LINE).map(Self::Speak)
            }
            [TOOL_COMMAND, rest @ ..] => parse_tool(rest).map(Self::Tool),
            [APP_COMMAND, rest @ ..] => parse_app(rest).map(Self::App),
            [START_CONTROL_TOWER_COMMAND, rest @ ..] => {
                parse_start_control_tower(rest).map(Self::StartControlTower)
            }
            ["slaves", rest @ ..] => parse_slaves(rest).map(Self::Slaves),
            ["master", rest @ ..] => parse_master(rest).map(Self::Master),
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
            "tts_engine": match settings.tts.engine {
                crate::settings::TtsEngine::Command => "command",
                crate::settings::TtsEngine::Server => "server",
            },
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

pub(crate) const GET_COMMAND: &str = "get";
/// The exit code of `harold get` for a name that is not a thing.
pub(crate) const UNKNOWN_THING_EXIT: i32 = 2;

/// One named item of the Harold state, which `harold get` prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GetThing {
    Voice,
    Messaging,
    Version,
    Machine,
    Multiplexer,
    Hosts,
    Chief,
}

impl GetThing {
    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "voice" => Self::Voice,
            "messaging" => Self::Messaging,
            "version" => Self::Version,
            "machine" => Self::Machine,
            "multiplexer" => Self::Multiplexer,
            "hosts" => Self::Hosts,
            "chief" => Self::Chief,
            _ => return None,
        })
    }
}

/// A `harold get` failure: the line for standard error and the exit code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GetFailure {
    pub(crate) line: String,
    pub(crate) exit_code: i32,
}

impl GetFailure {
    fn other(line: impl Into<String>) -> Self {
        Self {
            line: line.into(),
            exit_code: 1,
        }
    }
}

impl From<String> for GetFailure {
    fn from(line: String) -> Self {
        Self::other(line)
    }
}

fn parse_get(args: &[&str]) -> Result<GetThing, String> {
    let unknown = |name: &str| {
        format!(
            "unknown thing \"{name}\": voice, messaging, version, machine, multiplexer, hosts, chief"
        )
    };
    match args {
        [name] => GetThing::from_name(name).ok_or_else(|| unknown(name)),
        _ => Err(unknown(&args.join(" "))),
    }
}

/// `on` unless the switch stops the thing. A missing flag counts as `on`.
fn switch_state(stopped: Option<bool>) -> &'static str {
    if stopped == Some(true) { "off" } else { "on" }
}

/// The JSON object that `harold get` prints for `thing`. Structs keep the field order.
fn render_get(thing: GetThing, state: &GetHaroldStateResponse) -> String {
    use serde::Serialize;
    #[derive(Serialize)]
    struct Voice<'a> {
        state: &'static str,
        engine: &'a str,
    }
    #[derive(Serialize)]
    struct Switch {
        state: &'static str,
    }
    #[derive(Serialize)]
    struct Version<'a> {
        release_version: &'a str,
    }
    #[derive(Serialize)]
    struct Machine<'a> {
        machine_name: &'a str,
        host_icon: &'a str,
    }
    #[derive(Serialize)]
    struct Multiplexer<'a> {
        multiplexer: &'a str,
    }
    #[derive(Serialize)]
    struct Host<'a> {
        label: &'a str,
        host_icon: &'a str,
    }
    #[derive(Serialize)]
    struct Hosts<'a> {
        hosts: Vec<Host<'a>>,
    }
    #[derive(Serialize)]
    struct Chief<'a> {
        chief_session: &'a str,
    }
    let json = match thing {
        GetThing::Voice => serde_json::to_string(&Voice {
            state: switch_state(state.tts_muted),
            engine: &state.tts_engine,
        }),
        GetThing::Messaging => serde_json::to_string(&Switch {
            state: switch_state(state.messaging_paused),
        }),
        GetThing::Version => serde_json::to_string(&Version {
            release_version: &state.release_version,
        }),
        GetThing::Machine => serde_json::to_string(&Machine {
            machine_name: &state.machine_name,
            host_icon: &state.machine_host_icon,
        }),
        GetThing::Multiplexer => serde_json::to_string(&Multiplexer {
            multiplexer: &state.multiplexer,
        }),
        GetThing::Hosts => serde_json::to_string(&Hosts {
            hosts: state
                .rex_hosts
                .iter()
                .map(|host| Host {
                    label: &host.label,
                    host_icon: &host.host_icon,
                })
                .collect(),
        }),
        GetThing::Chief => serde_json::to_string(&Chief {
            chief_session: &state.chief_session,
        }),
    };
    json.expect("a struct of strings is valid JSON")
}

pub(crate) async fn get(settings: &Settings, thing: GetThing) -> Result<String, GetFailure> {
    get_within(settings, thing, CALL_DEADLINE).await
}

/// Ask the daemon for its Harold state and render one thing from the answer.
pub(crate) async fn get_within(
    settings: &Settings,
    thing: GetThing,
    wait: Duration,
) -> Result<String, GetFailure> {
    let address = settings
        .grpc
        .addr()
        .map_err(|_| GetFailure::other("invalid configuration"))?;
    let response = call_within(CALL_DEADLINE, wait, async {
        let mut client = HaroldClient::connect(format!("http://{address}"))
            .await
            .map_err(|_| GetFailure::other("the Harold daemon is not reachable"))?;
        client
            .get_harold_state(GetHaroldStateRequest {})
            .await
            .map_err(|status| match status.code() {
                tonic::Code::Unimplemented => {
                    GetFailure::other("this Harold is too old for harold get")
                }
                _ => GetFailure::other("the Harold daemon refused the call"),
            })
    })
    .await?;
    Ok(render_get(thing, &response.into_inner()))
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

/// Ask the daemon to say `args.text` at the desk, or to send it to the phone as text. `Ok`
/// means the daemon stored the line, not that you heard or read it. `Err` is the one line to
/// print.
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

fn step_from_json(step: &serde_json::Value) -> Result<questions::Step, String> {
    if !step.is_object() {
        return Err(json_error("step must be an object"));
    }
    Ok(questions::Step {
        text: json_text(step, "text")?,
        detail: json_optional_text(step, "detail")?,
    })
}

/// Turns the spec's JSON into a question or instructions. It only translates;
/// the daemon checks the rules. With a `steps` key, `decisions`, `spec`, `sha`,
/// `link` and `intro` may be absent.
fn question_from_json(text: &str) -> Result<questions::Question, String> {
    let value: serde_json::Value = serde_json::from_str(text).map_err(json_error)?;
    if !value.is_object() {
        return Err(json_error("question must be an object"));
    }
    let has_steps = value.get("steps").is_some();
    let optional = |field: &str| -> Result<String, String> {
        if has_steps {
            json_optional_text(&value, field)
        } else {
            json_text(&value, field)
        }
    };
    let steps = if has_steps {
        json_list(&value, "steps")?
            .iter()
            .map(step_from_json)
            .collect::<Result<_, _>>()?
    } else {
        Vec::new()
    };
    let decisions = if has_steps && value.get("decisions").is_none() {
        Vec::new()
    } else {
        json_list(&value, "decisions")?
            .iter()
            .map(decision_from_json)
            .collect::<Result<_, _>>()?
    };
    Ok(questions::Question {
        id: json_text(&value, "id")?,
        product: json_text(&value, "product")?,
        spec: optional("spec")?,
        sha: optional("sha")?,
        link: optional("link")?,
        intro: optional("intro")?,
        decisions,
        title: json_optional_text(&value, "title")?,
        steps,
        spec_file: json_optional_text(&value, "spec_file")?,
    })
}

/// One `list` line: instructions show their tick count, questions their spec.
fn list_line(listed: &questions::ListedQuestion) -> String {
    let question = listed.question.clone().unwrap_or_default();
    if !question.steps.is_empty() {
        return format!(
            "{} v{} {} steps {}/{}{}",
            question.id,
            listed.version,
            question.product,
            listed.ticked.len(),
            question.steps.len(),
            changed_spec_suffix(listed.spec_state)
        );
    }
    format!(
        "{} v{} {} {} picks {}/{}{}",
        question.id,
        listed.version,
        question.product,
        question.spec,
        listed.picks.len(),
        question.decisions.len(),
        changed_spec_suffix(listed.spec_state)
    )
}

/// ` (changed spec)` after a todo whose spec file no longer matches its checksum.
fn changed_spec_suffix(spec_state: i32) -> &'static str {
    match questions::SpecState::try_from(spec_state) {
        Ok(questions::SpecState::Changed) => " (changed spec)",
        _ => "",
    }
}

/// The `show` output: a head line, then one line per decision with its pick.
fn show_lines(shown: &questions::ShowQuestionResponse) -> String {
    let question = shown.question.clone().unwrap_or_default();
    let mut lines = vec![format!(
        "{} v{} {} {} {}",
        question.id, shown.version, question.product, question.spec, shown.state
    )];
    for (index, decision) in question.decisions.iter().enumerate() {
        let picked = shown
            .picks
            .iter()
            .find(|pick| pick.decision as usize == index);
        let what = match picked {
            None => "no pick".to_string(),
            Some(pick) if !pick.text.is_empty() => format!("{} \"{}\"", pick.key, pick.text),
            Some(pick) => {
                let label = decision
                    .options
                    .iter()
                    .find(|option| option.key == pick.key)
                    .map_or("", |option| option.label.as_str());
                format!("{} {label}", pick.key)
            }
        };
        lines.push(format!("  {} {}: {what}", index + 1, decision.ask));
    }
    lines.join("\n")
}

/// The line printed for a refusal from the question service: its status message.
fn tool_refusal_line(status: &tonic::Status) -> String {
    match status.code() {
        tonic::Code::Unimplemented => "the Harold daemon does not serve questions".into(),
        _ if status.message().is_empty() => "the Harold daemon refused the call".into(),
        _ => status.message().to_string(),
    }
}

/// For `send-message-to-away-device` and `speak` with no `--src-id`: adds the caller's Rex
/// block (`$REX_BLOCK`) as `--src-id`, when there is one. A tmux pane still needs `--src-id`,
/// as before.
pub(crate) fn with_caller_pane(args: Vec<String>) -> Vec<String> {
    let rex_block = std::env::var("REX_BLOCK").unwrap_or_default();
    with_caller_pane_from(args, rex_block.trim().to_string())
}

pub(crate) fn with_caller_pane_from(mut args: Vec<String>, pane: String) -> Vec<String> {
    let sends = matches!(
        args.first().map(String::as_str),
        Some(AWAY_MESSAGE_COMMAND | SPEAK_COMMAND)
    );
    if sends && !pane.is_empty() && !args.iter().any(|arg| arg == "--src-id") {
        args.insert(1, "--src-id".to_string());
        args.insert(2, pane);
    }
    args
}

/// The caller's pane id: `$TMUX_PANE` in a tmux pane, `$REX_BLOCK` in a Rex pane, or an empty
/// string outside both.
pub(crate) fn caller_pane() -> String {
    caller_pane_from(|name| std::env::var(name).ok())
}

pub(crate) fn caller_pane_from(var: impl Fn(&str) -> Option<String>) -> String {
    ["TMUX_PANE", "REX_BLOCK"]
        .into_iter()
        .filter_map(var)
        .map(|value| value.trim().to_string())
        .find(|value| !value.is_empty())
        .unwrap_or_default()
}

/// Run a `harold tool` command against the daemon at `address`. `Ok` is the line
/// to print; `Err` is the one line to print for a refusal. A backlog command sends the
/// caller's `TMUX_PANE` and works out local midnight in the Mac's UTC offset. A telemetry
/// command also sends the caller's pane.
pub(crate) async fn tool(
    address: &str,
    command: &ToolCommand,
    stdin: impl std::io::Read,
) -> Result<String, String> {
    if let ToolCommand::Backlog(backlog) = command {
        let pane = caller_pane();
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
    if let ToolCommand::Telemetry(telemetry) = command {
        return telemetry_within(address, telemetry, &caller_pane(), CALL_DEADLINE).await;
    }
    if let ToolCommand::Watch(watch) = command {
        let folder = std::env::current_dir().map_err(|_| "invalid folder".to_string())?;
        return watch_within(
            address,
            watch,
            &folder,
            crate::telemetry::local_offset(),
            CALL_DEADLINE,
        )
        .await;
    }
    if let ToolCommand::Release(release) = command {
        return release_within(address, release, CALL_DEADLINE).await;
    }
    if let ToolCommand::Knowledge(knowledge) = command {
        return knowledge_within(address, knowledge, KNOWLEDGE_DEADLINE).await;
    }
    if let ToolCommand::Tts(tts) = command {
        return tts_within(address, tts, TTS_DEADLINE, crate::tts::play::PLAYER).await;
    }
    tool_within(address, command, stdin, CALL_DEADLINE).await
}

/// The line printed for a refusal from the watch service. `skill` and `name` are the
/// caller's.
fn watch_refusal_line(status: &tonic::Status, command: &WatchCommand) -> String {
    let subject = match command {
        WatchCommand::Start { name, .. }
        | WatchCommand::Stop { name }
        | WatchCommand::Remind { name, .. } => name.as_str(),
        WatchCommand::Pause { name, .. } | WatchCommand::Resume { name, .. } => {
            name.as_deref().unwrap_or_default()
        }
        WatchCommand::List => "",
    };
    // `--all` has no name, so these lines then name no watch.
    let which = if subject.is_empty() {
        "the watch"
    } else {
        subject
    };
    match (status.code(), status.message()) {
        (tonic::Code::Unimplemented, _) => "the Harold daemon does not serve watches".into(),
        (tonic::Code::NotFound, "unknown_skill") => match command {
            WatchCommand::Start { skill, .. } => format!("unknown skill {skill}"),
            _ => "unknown skill".into(),
        },
        (tonic::Code::NotFound, "unknown_watch") => {
            format!("unknown watch {subject}").trim_end().to_string()
        }
        (tonic::Code::FailedPrecondition, "not_running") => format!("{which} is not running"),
        (tonic::Code::FailedPrecondition, "not_paused") => format!("{which} is not paused"),
        (tonic::Code::FailedPrecondition, "budget_day") => {
            format!("{which} is capped by the daily budget")
        }
        (tonic::Code::FailedPrecondition, "name_in_use") => format!("name {subject} is in use"),
        (tonic::Code::FailedPrecondition, "too_many_reminders") => {
            "20 reminders are waiting: stop one first".into()
        }
        (tonic::Code::FailedPrecondition, "daily_budget_unset") => "daily budget not set".into(),
        (tonic::Code::FailedPrecondition, "too_many_watches") => "too many watches".into(),
        (tonic::Code::FailedPrecondition, "daily_budget_spent") => "daily budget spent".into(),
        _ if status.message().is_empty() => "the Harold daemon refused the call".into(),
        _ => status.message().to_string(),
    }
}

fn plural(count: u32, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

fn watch_time(ms: i64, offset: time::UtcOffset) -> String {
    let moment = time::OffsetDateTime::from_unix_timestamp(ms / 1000)
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH)
        .to_offset(offset);
    format!("{:02}:{:02}", moment.hour(), moment.minute())
}

/// The local minute (`YYYY-MM-DD HH:MM`) of `ms` at `offset`.
fn watch_local_minute(ms: i64, offset: time::UtcOffset) -> String {
    let moment = time::OffsetDateTime::from_unix_timestamp(ms / 1000)
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH)
        .to_offset(offset);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        moment.year(),
        u8::from(moment.month()),
        moment.day(),
        moment.hour(),
        moment.minute()
    )
}

/// A local minute (`YYYY-MM-DD HH:MM`) as `HH:MM` when it is on the date of `today` (also
/// `YYYY-MM-DD HH:MM`), and in full on another date.
fn watch_short_minute(minute: &str, today: &str) -> String {
    match (minute.get(..10), today.get(..10), minute.get(11..)) {
        (Some(day), Some(this_day), Some(time)) if day == this_day => time.to_string(),
        _ => minute.to_string(),
    }
}

/// The schedule of a cron watch for `list`: `, cron <expr>, next <time>, checked <time>`.
/// The next time is the first match after the local minute of `now_ms`. Empty for a watch
/// at an interval.
fn watch_cron_part(watch: &watch_api::Watch, offset: time::UtcOffset, now_ms: i64) -> String {
    if watch.cron.is_empty() {
        return String::new();
    }
    let today = watch_local_minute(now_ms, offset);
    let mut part = format!(", cron {}", watch.cron);
    let next = crate::cron::Cron::parse(&watch.cron)
        .ok()
        .zip(crate::cron::LocalMinute::parse(&today))
        .and_then(|(cron, now)| cron.next_after(now));
    if let Some(next) = next {
        part.push_str(&format!(
            ", next {}",
            watch_short_minute(&next.to_string(), &today)
        ));
    }
    if !watch.checked_local.is_empty() {
        part.push_str(&format!(
            ", checked {}",
            watch_short_minute(&watch.checked_local, &today)
        ));
    }
    part
}

fn watch_list_line(watch: &watch_api::Watch, offset: time::UtcOffset, now_ms: i64) -> String {
    let cron = watch_cron_part(watch, offset, now_ms);
    let counts = format!(
        "{}, {}, ${:.2}",
        plural(watch.runs, "run", "runs"),
        plural(watch.reports, "report", "reports"),
        watch.cost_usd
    );
    if watch.paused {
        let capped = if watch.pause_how == "budget_day" {
            " (budget)"
        } else {
            ""
        };
        let until = watch
            .paused_until_ms
            .map(|ms| format!(" until {}", watch_time(ms, offset)))
            .unwrap_or_default();
        return format!(
            "{} paused{capped}{until} in {}{cron}, {counts}",
            watch.name, watch.folder
        );
    }
    if watch.running {
        let until = match watch.ends_at_ms {
            Some(ms) => format!("until {}", watch_time(ms, offset)),
            None => "no end".to_string(),
        };
        return format!(
            "{} running in {} {until}{cron}, limit {} s, {counts}",
            watch.name, watch.folder, watch.run_limit_secs
        );
    }
    let why = match watch.stop_reason.as_str() {
        "ended" => "time was up",
        "stopped" => "stopped",
        "daily_budget_spent" => "daily budget spent",
        "runs_failing" => "3 runs in a row failed",
        other => other,
    };
    format!("{} ended ({why}) in {}, {counts}", watch.name, watch.folder)
}

/// The line `remind` prints: `reminder <name> at HH:MM`, with the date before the time
/// when the due day is not today (a cron reminder can be days away), as in `list`.
fn watch_remind_line(name: &str, due_at_ms: i64, offset: time::UtcOffset, now_ms: i64) -> String {
    let today = watch_local_minute(now_ms, offset);
    let due = watch_local_minute(due_at_ms, offset);
    format!("reminder {name} at {}", watch_short_minute(&due, &today))
}

/// The line of a waiting reminder. A cron reminder can be days away, so its due time has
/// the date when it is not today, and the line names its expression.
fn watch_reminder_line(
    reminder: &watch_api::Reminder,
    offset: time::UtcOffset,
    now_ms: i64,
) -> String {
    let due = if reminder.cron.is_empty() {
        watch_time(reminder.due_at_ms, offset)
    } else {
        let today = watch_local_minute(now_ms, offset);
        let due = watch_local_minute(reminder.due_at_ms, offset);
        format!(
            "{}, cron {}",
            watch_short_minute(&due, &today),
            reminder.cron
        )
    };
    format!(
        "reminder {} due {due}, reporting to {}",
        reminder.name,
        if reminder.report_to.is_empty() {
            "self"
        } else {
            &reminder.report_to
        }
    )
}

/// The line for a pause. It names only what the daemon paused: a named watch that the
/// daemon did not pause gives `paused 0`.
fn watch_pause_line(
    name: Option<&str>,
    for_minutes: Option<u32>,
    paused: &watch_api::PauseResponse,
) -> String {
    let mut line = match name {
        Some(name) if paused.paused.iter().any(|done| done == name) => format!("paused {name}"),
        _ => watch_count_line("paused", &paused.paused, &paused.skipped),
    };
    if let Some(minutes) = for_minutes.filter(|_| !paused.paused.is_empty()) {
        line.push_str(&format!(" for {minutes} minutes"));
    }
    line
}

/// The line for a resume. It names only what the daemon resumed: a named watch that the
/// daemon did not resume gives `resumed 0`.
fn watch_resume_line(name: Option<&str>, resumed: &watch_api::ResumeResponse) -> String {
    match name {
        Some(name) if resumed.resumed.iter().any(|done| done == name) => {
            format!("resumed {name}")
        }
        _ => watch_count_line("resumed", &resumed.resumed, &resumed.skipped),
    }
}

/// The count line for `pause --all` and `resume --all`. The daemon skips only the
/// watches that the daily budget caps.
fn watch_count_line(verb: &str, done: &[String], skipped: &[String]) -> String {
    let mut line = format!("{verb} {}", done.len());
    if !skipped.is_empty() {
        line.push_str(&format!(", skipped {} (budget)", skipped.join(", ")));
    }
    line
}

/// The line printed for a refusal from the release service.
fn release_refusal_line(status: &tonic::Status, command: &ReleaseCommand) -> String {
    let ReleaseCommand::Next { product, .. } = command;
    match (status.code(), status.message()) {
        (tonic::Code::Unimplemented, _) => "the Harold daemon does not serve releases".into(),
        (tonic::Code::NotFound, "unknown_product") => {
            format!("unknown product {}", product.as_deref().unwrap_or_default())
        }
        (_, "registry_unreadable") => "the product registry cannot be read".into(),
        _ if status.message().is_empty() => "the Harold daemon refused the call".into(),
        _ => status.message().to_string(),
    }
}

/// Runs a `harold tool release` command. `Ok` is the text to print; `Err` the one line.
pub(crate) async fn release_within(
    address: &str,
    command: &ReleaseCommand,
    wait: Duration,
) -> Result<String, String> {
    call_within(CALL_DEADLINE, wait, async {
        let mut client = ReleaseServiceClient::connect(format!("http://{address}"))
            .await
            .map_err(|_| "the Harold daemon is not reachable".to_string())?;
        let ReleaseCommand::Next { product, json } = command;
        let response = client
            .next(release_api::NextRequest {
                product: product.clone().unwrap_or_default(),
            })
            .await
            .map_err(|status| release_refusal_line(&status, command))?
            .into_inner();
        Ok(if *json {
            crate::tools::release::render::json(&response)
        } else {
            crate::tools::release::render::text(&response)
        })
    })
    .await
}

/// Runs a `harold tool watch` command from `folder`, the caller's working directory.
/// `Ok` is the text to print; `Err` the one line.
pub(crate) async fn watch_within(
    address: &str,
    command: &WatchCommand,
    folder: &std::path::Path,
    offset: time::UtcOffset,
    wait: Duration,
) -> Result<String, String> {
    call_within(CALL_DEADLINE, wait, async {
        let mut client = WatchServiceClient::connect(format!("http://{address}"))
            .await
            .map_err(|_| "the Harold daemon is not reachable".to_string())?;
        let refused = |status: tonic::Status| watch_refusal_line(&status, command);
        match command {
            WatchCommand::Start {
                name,
                skill,
                schedule,
                for_minutes,
                report_to,
                run_budget_usd,
                run_limit_secs,
                every_run,
            } => {
                let (every_minutes, cron, how_often) = match schedule {
                    WatchSchedule::Every(minutes) => {
                        (Some(*minutes), None, format!("every {minutes} minutes"))
                    }
                    WatchSchedule::Cron(cron) => {
                        (None, Some(cron.clone()), format!("on cron {cron}"))
                    }
                };
                let started = client
                    .start_watch(watch_api::StartWatchRequest {
                        name: name.clone(),
                        folder: folder.to_string_lossy().into_owned(),
                        skill: skill.clone(),
                        every_minutes,
                        cron,
                        for_minutes: *for_minutes,
                        report_to: report_to.clone().unwrap_or_default(),
                        run_budget_usd: run_budget_usd
                            .as_deref()
                            .and_then(|text| text.parse().ok())
                            .unwrap_or(0.0),
                        every_run: *every_run,
                        run_limit_secs: run_limit_secs.unwrap_or(0),
                    })
                    .await
                    .map_err(refused)?
                    .into_inner();
                let until = match started.ends_at_ms {
                    Some(ms) => format!("until {}", watch_time(ms, offset)),
                    None => "with no end".to_string(),
                };
                Ok(format!(
                    "started {} in {} {until}, {how_often}, reporting to {}",
                    started.name,
                    folder.display(),
                    report_to
                        .as_deref()
                        .filter(|to| !to.is_empty())
                        .unwrap_or("self")
                ))
            }
            WatchCommand::Stop { name } => {
                client
                    .stop_watch(watch_api::StopWatchRequest { name: name.clone() })
                    .await
                    .map_err(refused)?;
                Ok(format!("stopped {name}"))
            }
            WatchCommand::List => {
                let listed = client
                    .list_watches(watch_api::ListWatchesRequest {})
                    .await
                    .map_err(refused)?
                    .into_inner();
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |since| since.as_millis() as i64);
                let mut lines: Vec<String> = listed
                    .watches
                    .iter()
                    .map(|watch| watch_list_line(watch, offset, now_ms))
                    .collect();
                lines.extend(
                    listed
                        .reminders
                        .iter()
                        .map(|reminder| watch_reminder_line(reminder, offset, now_ms)),
                );
                lines.push(format!("spent today ${:.2}", listed.spent_usd));
                Ok(lines.join("\n"))
            }
            WatchCommand::Pause {
                name,
                all,
                for_minutes,
            } => {
                let paused = client
                    .pause(watch_api::PauseRequest {
                        name: name.clone().unwrap_or_default(),
                        all: *all,
                        for_minutes: *for_minutes,
                        caller: "cli".into(),
                    })
                    .await
                    .map_err(refused)?
                    .into_inner();
                Ok(watch_pause_line(name.as_deref(), *for_minutes, &paused))
            }
            WatchCommand::Resume { name, all } => {
                let resumed = client
                    .resume(watch_api::ResumeRequest {
                        name: name.clone().unwrap_or_default(),
                        all: *all,
                        caller: "cli".into(),
                    })
                    .await
                    .map_err(refused)?
                    .into_inner();
                Ok(watch_resume_line(name.as_deref(), &resumed))
            }
            WatchCommand::Remind {
                name,
                when,
                report_to,
                text,
            } => {
                let reminded = client
                    .remind(watch_api::RemindRequest {
                        name: name.clone(),
                        folder: folder.to_string_lossy().into_owned(),
                        when: Some(match when {
                            RemindWhen::In(minutes) => {
                                watch_api::remind_request::When::InMinutes(*minutes)
                            }
                            RemindWhen::At(time) => {
                                watch_api::remind_request::When::AtLocal(time.clone())
                            }
                            RemindWhen::Cron(cron) => {
                                watch_api::remind_request::When::Cron(cron.clone())
                            }
                        }),
                        report_to: report_to.clone().unwrap_or_default(),
                        text: text.clone(),
                    })
                    .await
                    .map_err(refused)?
                    .into_inner();
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |since| since.as_millis() as i64);
                Ok(watch_remind_line(name, reminded.due_at_ms, offset, now_ms))
            }
        }
    })
    .await
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
                let mut lines: Vec<String> = listed.questions.iter().map(list_line).collect();
                lines.push(format!("{} waiting", listed.waiting));
                Ok(lines.join("\n"))
            }
            ToolCommand::QuestionsShow { id } => {
                let shown = client
                    .show_question(questions::ShowQuestionRequest { id: id.clone() })
                    .await
                    .map_err(refused)?
                    .into_inner();
                Ok(show_lines(&shown))
            }
            ToolCommand::QuestionsChecked => {
                let checked = client
                    .mark_checked(questions::MarkCheckedRequest {})
                    .await
                    .map_err(refused)?
                    .into_inner();
                Ok(format!(
                    "checked at {}",
                    watch_time(checked.checked_at_ms, crate::telemetry::local_offset())
                ))
            }
            // `tool` runs a backlog command through `backlog_within`, a telemetry
            // command through `telemetry_within`, and a watch
            // command through `watch_within`. `main` runs a work command through `work`.
            ToolCommand::Backlog(_)
            | ToolCommand::Telemetry(_)
            | ToolCommand::Watch(_)
            | ToolCommand::Release(_)
            | ToolCommand::Knowledge(_)
            | ToolCommand::Tts(_)
            | ToolCommand::Work(_) => Err(TOOL_USAGE_LINE.into()),
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
        (tonic::Code::PermissionDenied, "no_pane") => {
            "refused: not run in a tmux or Rex pane".into()
        }
        (tonic::Code::Unimplemented, _) => "the Harold daemon does not serve the backlog".into(),
        (_, "") => "the Harold daemon refused the call".into(),
        (_, message) => message.to_string(),
    }
}

/// The line printed for a refusal from the telemetry service: the service's own plain
/// text. The reason word in the `reason` metadata key is for scripts, not for this line.
fn telemetry_refusal_line(status: &tonic::Status) -> String {
    match (status.code(), status.message()) {
        (tonic::Code::Unimplemented, _) => {
            "the Harold daemon does not serve the telemetry tool".into()
        }
        (_, "") => "the Harold daemon refused the call".into(),
        (_, message) => message.to_string(),
    }
}

/// The request a telemetry command sends, with `pane` as the caller's pane.
#[derive(Debug, Clone, PartialEq)]
enum TelemetryRequest {
    Declare(telemetry_api::DeclareRequest),
    Count(telemetry_api::CountRequest),
    Gauge(telemetry_api::GaugeRequest),
    Event(telemetry_api::EventRequest),
    List(telemetry_api::ListRequest),
    Remove(telemetry_api::RemoveRequest),
}

fn telemetry_pairs(attrs: &[(String, String)]) -> Vec<telemetry_api::Pair> {
    attrs
        .iter()
        .map(|(key, value)| telemetry_api::Pair {
            key: key.clone(),
            value: value.clone(),
        })
        .collect()
}

fn telemetry_request(command: &TelemetryCommand, pane: &str) -> TelemetryRequest {
    use TelemetryCommand as C;
    let caller_pane = pane.to_string();
    match command {
        C::Declare {
            name,
            kind,
            meaning,
            attrs,
        } => TelemetryRequest::Declare(telemetry_api::DeclareRequest {
            caller_pane,
            name: name.clone(),
            kind: kind.proto() as i32,
            meaning: meaning.clone(),
            attrs: attrs
                .iter()
                .map(|(key, values)| telemetry_api::Attr {
                    key: key.clone(),
                    values: values.clone(),
                })
                .collect(),
        }),
        C::Count { name, n, attrs } => TelemetryRequest::Count(telemetry_api::CountRequest {
            caller_pane,
            name: name.clone(),
            n: *n,
            attrs: telemetry_pairs(attrs),
        }),
        C::Gauge { name, value, attrs } => TelemetryRequest::Gauge(telemetry_api::GaugeRequest {
            caller_pane,
            name: name.clone(),
            value: value.0,
            attrs: telemetry_pairs(attrs),
        }),
        C::Event { name, attrs } => TelemetryRequest::Event(telemetry_api::EventRequest {
            caller_pane,
            name: name.clone(),
            attrs: telemetry_pairs(attrs),
        }),
        C::List { prefix } => TelemetryRequest::List(telemetry_api::ListRequest {
            caller_pane,
            prefix: prefix.clone().unwrap_or_default(),
        }),
        C::Remove { name } => TelemetryRequest::Remove(telemetry_api::RemoveRequest {
            caller_pane,
            name: name.clone(),
        }),
    }
}

fn telemetry_kind_word(kind: i32) -> &'static str {
    match telemetry_api::Kind::try_from(kind) {
        Ok(telemetry_api::Kind::Counter) => "counter",
        Ok(telemetry_api::Kind::Gauge) => "gauge",
        Ok(telemetry_api::Kind::Event) => "event",
        Ok(telemetry_api::Kind::Unspecified) | Err(_) => "unknown",
    }
}

/// One line for each signal: its name, kind, quoted meaning and `key=values` attributes.
fn telemetry_list_lines(signals: &[telemetry_api::Signal]) -> String {
    if signals.is_empty() {
        return "no signals are declared".into();
    }
    signals
        .iter()
        .map(|signal| {
            let mut line = format!(
                "{} {} \"{}\"",
                signal.name,
                telemetry_kind_word(signal.kind),
                signal.meaning
            );
            for attr in &signal.attrs {
                line.push_str(&format!(" {}={}", attr.key, attr.values.join(",")));
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Run a `harold tool telemetry` command against the daemon at `address`, as `pane`.
/// `Ok` is the text to print; `Err` the one line. An empty `pane` is sent as it is, and
/// the daemon refuses it with its `no_pane` text.
pub(crate) async fn telemetry_within(
    address: &str,
    command: &TelemetryCommand,
    pane: &str,
    wait: Duration,
) -> Result<String, String> {
    call_within(CALL_DEADLINE, wait, async {
        let mut client = TelemetryServiceClient::connect(format!("http://{address}"))
            .await
            .map_err(|_| "the Harold daemon is not reachable".to_string())?;
        let refused = |status: tonic::Status| telemetry_refusal_line(&status);
        match telemetry_request(command, pane) {
            TelemetryRequest::Declare(request) => {
                let name = request.name.clone();
                client.declare(request).await.map_err(refused)?;
                Ok(format!("declared {name}"))
            }
            TelemetryRequest::Count(request) => {
                let line = format!("counted {} by {}", request.name, request.n);
                client.count(request).await.map_err(refused)?;
                Ok(line)
            }
            TelemetryRequest::Gauge(request) => {
                let line = format!("set {} to {}", request.name, request.value);
                client.gauge(request).await.map_err(refused)?;
                Ok(line)
            }
            TelemetryRequest::Event(request) => {
                let name = request.name.clone();
                client.event(request).await.map_err(refused)?;
                Ok(format!("recorded {name}"))
            }
            TelemetryRequest::List(request) => {
                let listed = client.list(request).await.map_err(refused)?.into_inner();
                Ok(telemetry_list_lines(&listed.signals))
            }
            TelemetryRequest::Remove(request) => {
                let name = request.name.clone();
                client.remove(request).await.map_err(refused)?;
                Ok(format!("removed {name}"))
            }
        }
    })
    .await
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

/// How long ago a queue item's text changed: `(45m)` under an hour, `(3h)`
/// under two days, else `(2d)`, rounded down.
fn queue_age(now_ms: i64, updated_at_ms: i64) -> String {
    let minutes = (now_ms - updated_at_ms).max(0) / 60_000;
    match minutes {
        0..=59 => format!("({minutes}m)"),
        60..=2879 => format!("({}h)", minutes / 60),
        _ => format!("({}d)", minutes / 1440),
    }
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
                Some(item) => format!(
                    "Now: {} {} {}",
                    item.id,
                    queue_age(ms(now), item.updated_at_ms),
                    first_line(&item.text)
                ),
                None => "Now: nothing".into(),
            }];
            lines.push("Parked:".into());
            lines.extend(listing.parked.iter().map(|item| {
                format!(
                    "  {}. {} {} {}",
                    item.position,
                    item.id,
                    queue_age(ms(now), item.updated_at_ms),
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

// ---- The work tool: `harold tool work` ----

const WORK_USAGE_LINE: &str = "invalid argument: usage is harold tool work list|show|put|set|decide|mark|link|unlink|check|import|rotate|history|search";

/// A day in milliseconds: `list` names the work records that wait for the user longer.
const WAITING_LONG_MS: i64 = 86_400_000;

/// A `harold tool work` command. `put` reads the work record as JSON from stdin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WorkCommand {
    List {
        all: bool,
    },
    Show {
        name: String,
    },
    Put {
        name: String,
    },
    Set {
        name: String,
        priority: Option<String>,
        status: Option<String>,
        waits_for: Option<String>,
        next: Option<String>,
    },
    Decide {
        name: String,
        mark: String,
        who: String,
        text: String,
    },
    Mark {
        name: String,
        number: u32,
        mark: String,
    },
    Link {
        name: String,
        link: String,
    },
    Unlink {
        name: String,
        link: String,
    },
    Check,
    Import {
        folder: std::path::PathBuf,
        queue: Vec<String>,
        apply: bool,
    },
    RotateDryRun,
    History {
        name: String,
    },
    Search {
        word: String,
    },
}

impl WorkCommand {
    /// How long the command waits for the daemon: import, history and search read
    /// many files.
    pub(crate) fn deadline(&self) -> Duration {
        match self {
            Self::Import { .. } | Self::History { .. } | Self::Search { .. } => BULK_DEADLINE,
            _ => CALL_DEADLINE,
        }
    }
}

/// What a work command prints on stdout, and the code Harold exits with.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct WorkOutput {
    pub(crate) text: String,
    pub(crate) exit_code: i32,
}

impl WorkOutput {
    fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            exit_code: 0,
        }
    }
}

/// Writes `output` to `out` and returns the exit code. An empty text writes
/// nothing, not even a line break: a clean `check` is silent.
pub(crate) fn write_work_output(output: &WorkOutput, out: &mut impl std::io::Write) -> i32 {
    if !output.text.is_empty() {
        // A closed stdout cannot show the text; the exit code still tells.
        let _ = writeln!(out, "{}", output.text);
    }
    output.exit_code
}

fn parse_work(args: &[&str]) -> Result<WorkCommand, String> {
    parse_work_words(args).map_err(|_| WORK_USAGE_LINE.to_string())
}

/// The shared helpers (`word`, `flags`, `number`) give the backlog's usage line;
/// `parse_work` puts the work tool's line in its place.
fn parse_work_words(args: &[&str]) -> Result<WorkCommand, String> {
    use WorkCommand as C;
    let usage = || Err(WORK_USAGE_LINE.to_string());
    match args {
        ["list", rest @ ..] => Ok(C::List {
            all: flags(rest, &["--all"], &[])?.contains_key("--all"),
        }),
        ["show", name] => Ok(C::Show { name: word(name)? }),
        ["put", name] => Ok(C::Put { name: word(name)? }),
        ["set", name, rest @ ..] => {
            let found = flags(
                rest,
                &[],
                &["--priority", "--status", "--waits-for", "--next"],
            )?;
            if found.is_empty() {
                return usage();
            }
            let value = |flag: &str| found.get(flag).copied().flatten().map(String::from);
            Ok(C::Set {
                name: word(name)?,
                priority: value("--priority"),
                status: value("--status"),
                waits_for: value("--waits-for"),
                next: value("--next"),
            })
        }
        ["decide", name, rest @ ..] => {
            let (mut mark, mut who, mut rest) = (None, None, rest);
            while let [flag @ ("--mark" | "--who"), value, more @ ..] = rest {
                let slot = match *flag {
                    "--mark" => &mut mark,
                    _ => &mut who,
                };
                if slot.is_some() || value.starts_with("--") {
                    return usage();
                }
                *slot = Some(value.to_string());
                rest = more;
            }
            let (Some(mark), Some(who)) = (mark, who) else {
                return usage();
            };
            Ok(C::Decide {
                name: word(name)?,
                mark,
                who,
                text: words(rest)?.join(" "),
            })
        }
        ["mark", name, number_arg, mark] => Ok(C::Mark {
            name: word(name)?,
            number: number(number_arg)?,
            mark: word(mark)?,
        }),
        ["link", name, link] => Ok(C::Link {
            name: word(name)?,
            link: word(link)?,
        }),
        ["unlink", name, link] => Ok(C::Unlink {
            name: word(name)?,
            link: word(link)?,
        }),
        ["check"] => Ok(C::Check),
        ["import", folder, rest @ ..] => {
            let found = flags(rest, &["--apply"], &["--queue"])?;
            let queue = found
                .get("--queue")
                .copied()
                .flatten()
                .map(|ids| ids.split(',').map(|id| id.trim().to_string()).collect())
                .unwrap_or_default();
            Ok(C::Import {
                folder: word(folder)?.into(),
                queue,
                apply: found.contains_key("--apply"),
            })
        }
        ["rotate", "--dry-run"] => Ok(C::RotateDryRun),
        ["history", name] => Ok(C::History { name: word(name)? }),
        ["search", search] if !search.trim().is_empty() => Ok(C::Search {
            word: word(search)?,
        }),
        _ => usage(),
    }
}

/// The line printed for a refusal from the work service. It names the reason word.
/// `pane` is the caller's, as the command sent it.
fn work_refusal_line(status: &tonic::Status, pane: &str) -> String {
    use tonic::Code;
    match (status.code(), status.message()) {
        (Code::PermissionDenied, "not_chief") => {
            format!("refused: not_chief: pane {pane} is not the chief's pane")
        }
        (Code::PermissionDenied, "no_pane") => "refused: no_pane: not run in a pane".into(),
        (_, "focus_full") => {
            "refused: focus_full: two open work records are in focus already".into()
        }
        (_, "name_in_use") => {
            "refused: name_in_use: a closed work record of the current file has the name".into()
        }
        (_, "not_found") => {
            "refused: not_found: the work record, decision or link is not there".into()
        }
        (_, "read_only") => "refused: read_only: the work file is read-only".into(),
        (_, "rotation_is_harolds") => {
            "refused: rotation_is_harolds: only Harold rotates the work files".into()
        }
        (Code::InvalidArgument, "invalid name") => {
            "refused: bad_name: a name is 1 to 48 characters of a-z, 0-9 and -".into()
        }
        (Code::Unimplemented, _) => "the Harold daemon does not serve the work records".into(),
        (_, "") => "the Harold daemon refused the call".into(),
        (Code::Unavailable, message) => message.to_string(),
        (_, message) => format!("refused: {message}"),
    }
}

fn decision_from_work_json(value: &serde_json::Value) -> Result<work_api::Decision, String> {
    if !value.is_object() {
        return Err(json_error("decisions must be a list of objects"));
    }
    Ok(work_api::Decision {
        at: json_optional_text(value, "at")?,
        who: json_text(value, "who")?,
        mark: json_text(value, "mark")?,
        text: json_text(value, "text")?,
    })
}

/// Turns `put`'s JSON into a work record named `name`. It only translates; the
/// daemon checks the rules.
fn work_record_from_json(name: &str, text: &str) -> Result<work_api::WorkRecord, String> {
    let value: serde_json::Value = serde_json::from_str(text).map_err(json_error)?;
    if !value.is_object() {
        return Err(json_error("work record must be an object"));
    }
    if value.get("name").is_some() && json_text(&value, "name")? != name {
        return Err(json_error(format!("name must be {name} or left out")));
    }
    let decisions = match value.get("decisions") {
        None => Vec::new(),
        Some(_) => json_list(&value, "decisions")?
            .iter()
            .map(decision_from_work_json)
            .collect::<Result<_, _>>()?,
    };
    let links = match value.get("links") {
        None => Vec::new(),
        Some(_) => json_list(&value, "links")?
            .iter()
            .map(|link| match link {
                serde_json::Value::String(link) => Ok(link.clone()),
                _ => Err(json_error("links must be a list of strings")),
            })
            .collect::<Result<_, _>>()?,
    };
    Ok(work_api::WorkRecord {
        name: name.into(),
        title: json_optional_text(&value, "title")?,
        priority: json_optional_text(&value, "priority")?,
        status: json_optional_text(&value, "status")?,
        waits_for: json_optional_text(&value, "waits_for")?,
        next_step: json_optional_text(&value, "next_step")?,
        decisions,
        links,
        ..Default::default()
    })
}

fn work_line(record: &work_api::WorkRecord) -> String {
    format!(
        "{:<5}  {}  [{}]  next: {}",
        record.priority, record.name, record.status, record.next_step
    )
}

/// The `list` text: one line for each work record in the service's order (focus,
/// then next, then later), then the names of the work records at `waiting for the
/// user` for more than a day at `now_ms`. The CLI works these out from
/// `status_since_ms`: the list response has no field for them.
fn work_list_text(listed: &work_api::ListResponse, now_ms: i64) -> String {
    if listed.records.is_empty() {
        return "no open work records".into();
    }
    let mut lines: Vec<String> = listed.records.iter().map(work_line).collect();
    let waiting: Vec<&str> = listed
        .records
        .iter()
        .filter(|record| record.status == "waiting for the user")
        .filter(|record| now_ms - record.status_since_ms > WAITING_LONG_MS)
        .map(|record| record.name.as_str())
        .collect();
    if !waiting.is_empty() {
        lines.push(format!(
            "waiting for the user for more than a day: {}",
            waiting.join(", ")
        ));
    }
    lines.join("\n")
}

/// One work record with its decisions and links, for `show` and `history`.
fn work_record_text(record: &work_api::WorkRecord) -> String {
    let or_none = |text: &str| match text.is_empty() {
        true => "-".to_string(),
        false => text.to_string(),
    };
    let mut lines = vec![
        format!("{}: {}", record.name, record.title),
        format!("priority: {}", record.priority),
        format!("status: {}", record.status),
        format!("waits for: {}", or_none(&record.waits_for)),
        format!("next step: {}", or_none(&record.next_step)),
    ];
    match record.decisions.is_empty() {
        true => lines.push("decisions: none".into()),
        false => lines.push("decisions:".into()),
    }
    for (index, decision) in record.decisions.iter().enumerate() {
        lines.push(format!(
            "  {}. {}, {}, {}: {}",
            index + 1,
            decision.mark,
            decision.who,
            decision.at,
            decision.text
        ));
    }
    match record.links.is_empty() {
        true => lines.push("links: none".into()),
        false => lines.push("links:".into()),
    }
    lines.extend(record.links.iter().map(|link| format!("  {link}")));
    lines.join("\n")
}

/// The import's text. A dry run with an unplaced line exits 1, as `check` does.
fn work_import_output(imported: &work_api::ImportResponse, apply: bool) -> WorkOutput {
    let verb = match apply {
        true => "stored",
        false => "would store",
    };
    let mut lines: Vec<String> = imported
        .records
        .iter()
        .map(|record| format!("{verb} {}", work_line(record)))
        .collect();
    lines.extend(imported.unplaced.iter().map(|unplaced| {
        let place = match unplaced.line {
            0 => unplaced.file.clone(),
            line => format!("{}:{line}", unplaced.file),
        };
        match unplaced.text.is_empty() {
            true => format!("unplaced {place}: {}", unplaced.reason),
            false => format!("unplaced {place}: {}: {}", unplaced.reason, unplaced.text),
        }
    }));
    if !imported.in_use.is_empty() {
        lines.push(format!("in use: {}", imported.in_use.join(", ")));
    }
    lines.push(format!(
        "{} work records, {} unplaced lines, {} names in use",
        imported.records.len(),
        imported.unplaced.len(),
        imported.in_use.len()
    ));
    if !apply {
        lines.push("dry run: nothing is stored; run again with --apply to store".into());
    }
    WorkOutput {
        text: lines.join("\n"),
        exit_code: i32::from(!apply && !imported.unplaced.is_empty()),
    }
}

/// Run a `harold tool work` command as the caller's pane, now, with its own deadline.
pub(crate) async fn work(
    address: &str,
    command: &WorkCommand,
    stdin: impl std::io::Read,
) -> Result<WorkOutput, String> {
    work_within(
        address,
        command,
        stdin,
        &caller_pane(),
        time::OffsetDateTime::now_utc(),
        command.deadline(),
    )
    .await
}

/// Run a `harold tool work` command against the daemon at `address`, as `pane`, at
/// `now`. `Ok` is what to print and the exit code; `Err` is the one refusal line.
pub(crate) async fn work_within(
    address: &str,
    command: &WorkCommand,
    mut stdin: impl std::io::Read,
    pane: &str,
    now: time::OffsetDateTime,
    wait: Duration,
) -> Result<WorkOutput, String> {
    let record = match command {
        WorkCommand::Put { name } => {
            let mut text = String::new();
            stdin.read_to_string(&mut text).map_err(json_error)?;
            Some(work_record_from_json(name, &text)?)
        }
        _ => None,
    };
    call_within(command.deadline(), wait, async {
        let client = WorkServiceClient::connect(format!("http://{address}"))
            .await
            .map_err(|_| "the Harold daemon is not reachable".to_string())?;
        work_call(client, command, record, pane, ms(now)).await
    })
    .await
}

async fn work_call(
    mut client: WorkServiceClient<tonic::transport::Channel>,
    command: &WorkCommand,
    record: Option<work_api::WorkRecord>,
    pane: &str,
    now_ms: i64,
) -> Result<WorkOutput, String> {
    use WorkCommand as C;
    let caller_pane = pane.to_string();
    let refused = |status: tonic::Status| work_refusal_line(&status, pane);
    let text = match command {
        C::List { all } => {
            let listed = client
                .list(work_api::ListRequest { all: *all })
                .await
                .map_err(refused)?
                .into_inner();
            work_list_text(&listed, now_ms)
        }
        C::Show { name } => {
            let shown = client
                .show(work_api::ShowRequest { name: name.clone() })
                .await
                .map_err(refused)?
                .into_inner();
            work_record_text(&shown.record.unwrap_or_default())
        }
        C::Put { name } => {
            let put = client
                .put(work_api::PutRequest {
                    record,
                    caller_pane,
                })
                .await
                .map_err(refused)?
                .into_inner();
            match put.created {
                true => format!("created {name}"),
                false => format!("replaced {name}"),
            }
        }
        C::Set {
            name,
            priority,
            status,
            waits_for,
            next,
        } => {
            client
                .set(work_api::SetRequest {
                    name: name.clone(),
                    title: None,
                    priority: priority.clone(),
                    status: status.clone(),
                    waits_for: waits_for.clone(),
                    next_step: next.clone(),
                    caller_pane,
                })
                .await
                .map_err(refused)?;
            format!("set {name}")
        }
        C::Decide {
            name,
            mark,
            who,
            text,
        } => {
            let decided = client
                .decide(work_api::DecideRequest {
                    name: name.clone(),
                    mark: mark.clone(),
                    who: who.clone(),
                    text: text.clone(),
                    caller_pane,
                })
                .await
                .map_err(refused)?
                .into_inner();
            format!("added decision {} to {name}", decided.number)
        }
        C::Mark { name, number, mark } => {
            client
                .mark(work_api::MarkRequest {
                    name: name.clone(),
                    number: *number,
                    mark: mark.clone(),
                    caller_pane,
                })
                .await
                .map_err(refused)?;
            format!("marked decision {number} of {name} {mark}")
        }
        C::Link { name, link } => {
            client
                .link(work_api::LinkRequest {
                    name: name.clone(),
                    link: link.clone(),
                    caller_pane,
                })
                .await
                .map_err(refused)?;
            format!("linked {name} to {link}")
        }
        C::Unlink { name, link } => {
            client
                .unlink(work_api::UnlinkRequest {
                    name: name.clone(),
                    link: link.clone(),
                    caller_pane,
                })
                .await
                .map_err(refused)?;
            format!("unlinked {link} from {name}")
        }
        C::Check => {
            let checked = client
                .check(work_api::CheckRequest {})
                .await
                .map_err(refused)?
                .into_inner();
            let lines: Vec<&str> = checked
                .mismatches
                .iter()
                .map(|mismatch| mismatch.line.as_str())
                .collect();
            return Ok(WorkOutput {
                text: lines.join("\n"),
                exit_code: i32::from(!lines.is_empty()),
            });
        }
        C::Import {
            folder,
            queue,
            apply,
        } => {
            // The service needs an absolute path; a relative one is from the caller's folder.
            let folder = std::path::absolute(folder).map_err(|_| "invalid folder".to_string())?;
            let imported = client
                .import(work_api::ImportRequest {
                    folder: folder.to_string_lossy().into_owned(),
                    queue: queue.clone(),
                    apply: *apply,
                    caller_pane,
                })
                .await
                .map_err(refused)?
                .into_inner();
            return Ok(work_import_output(&imported, *apply));
        }
        C::RotateDryRun => {
            let rotation = client
                .rotate(work_api::RotateRequest { dry_run: true })
                .await
                .map_err(refused)?
                .into_inner();
            let mut text = format!(
                "a rotation would copy {} open work records, {} decisions and {} links to {}",
                rotation.open_records, rotation.decisions, rotation.links, rotation.target
            );
            if !rotation.records.is_empty() {
                text.push_str(&format!("\nrecords: {}", rotation.records.join(", ")));
            }
            text
        }
        C::History { name } => {
            let history = client
                .history(work_api::HistoryRequest { name: name.clone() })
                .await
                .map_err(refused)?
                .into_inner();
            let moves = match history.moves.is_empty() {
                true => "moves: none".to_string(),
                false => format!("moves:\n{}", work_moves_text(&history.moves)),
            };
            format!(
                "in {}\n{}\n{}",
                history.file,
                work_record_text(&history.record.unwrap_or_default()),
                moves
            )
        }
        C::Search { word } => {
            let searched = client
                .search(work_api::SearchRequest { word: word.clone() })
                .await
                .map_err(refused)?
                .into_inner();
            match searched.found.is_empty() {
                true => format!("no work record holds {word}"),
                false => searched
                    .found
                    .iter()
                    .map(|found| {
                        let record = found.record.clone().unwrap_or_default();
                        format!(
                            "{}  {}  [{}]  {}",
                            found.file, record.name, record.status, record.title
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            }
        }
    };
    Ok(WorkOutput::text(text))
}

/// The status moves of a work record, one line each, oldest first, in UTC.
/// An empty `from` prints as `created`.
fn work_moves_text(moves: &[work_api::Move]) -> String {
    moves
        .iter()
        .map(|m| {
            let from = match m.from.is_empty() {
                true => "created",
                false => m.from.as_str(),
            };
            format!(
                "{}  {} → {}",
                minute_text(m.at_ms, time::UtcOffset::UTC),
                from,
                m.to
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ---- End of the work tool ----

/// `ms` since the Unix epoch as `2026-10-23 14:05` at `offset`.
fn minute_text(ms: i64, offset: time::UtcOffset) -> String {
    let Ok(at) = time::OffsetDateTime::from_unix_timestamp(ms.div_euclid(1000)) else {
        return "unknown".into();
    };
    let at = at.to_offset(offset);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        at.year(),
        u8::from(at.month()),
        at.day(),
        at.hour(),
        at.minute()
    )
}

/// What a `harold app pair` report says about a pairing file that is not usable.
fn not_set_up_words(reason: &str) -> &'static str {
    match reason {
        "missing" => "the pairing file is missing or cannot be read",
        "no_code" => "the pairing file has no HAROLD_PAIRING_CODE line",
        "too_short" => "the pairing code has fewer than 6 characters",
        "mode" => "the pairing file can be read by others; its mode must be 0600",
        _ => "the pairing file cannot be used",
    }
}

fn app_refusal_line(status: &tonic::Status, command: &AppCommand) -> String {
    match status.code() {
        tonic::Code::Unimplemented => "the Harold daemon does not serve app commands".into(),
        tonic::Code::FailedPrecondition => "the web app is not running".into(),
        tonic::Code::NotFound => match command {
            AppCommand::Unpair { name } => format!("no device is called {name}"),
            _ => "no such device".into(),
        },
        _ => "the Harold daemon refused the command".into(),
    }
}

/// Runs a `harold app` command against the daemon at `address`. `Ok` is the text to print:
/// never the pairing code. `Err` is the one line to print.
pub(crate) async fn app(
    address: &str,
    command: &AppCommand,
    offset: time::UtcOffset,
) -> Result<String, String> {
    call_within(CALL_DEADLINE, CALL_DEADLINE, async {
        let mut client =
            crate::harold::harold_app_client::HaroldAppClient::connect(format!("http://{address}"))
                .await
                .map_err(|_| "the Harold daemon is not reachable".to_string())?;
        let refused = |status: tonic::Status| app_refusal_line(&status, command);
        match command {
            AppCommand::Pair { unlock } => {
                let status = client
                    .pairing_status(crate::harold::PairingStatusRequest {})
                    .await
                    .map_err(refused)?
                    .into_inner();
                let mut lines = Vec::new();
                if *unlock {
                    lines.push("unlocked".to_string());
                }
                if status.set_up {
                    lines.push("pairing is set up".into());
                } else {
                    lines.push(format!(
                        "pairing is not set up: {}",
                        not_set_up_words(&status.reason)
                    ));
                }
                lines.push(format!(
                    "locked: {}",
                    if status.locked { "yes" } else { "no" }
                ));
                lines.push(format!("wrong tries: {} of 5", status.tries));
                Ok(lines.join("\n"))
            }
            AppCommand::Devices => {
                let devices = client
                    .list_devices(crate::harold::ListDevicesRequest {})
                    .await
                    .map_err(refused)?
                    .into_inner()
                    .devices;
                if devices.is_empty() {
                    return Ok("no paired devices".into());
                }
                let lines: Vec<String> = devices
                    .iter()
                    .map(|device| {
                        format!(
                            "{} (paired {}, last seen {})",
                            device.name,
                            minute_text(device.paired_at_ms, offset),
                            minute_text(device.last_seen_ms, offset)
                        )
                    })
                    .collect();
                Ok(lines.join("\n"))
            }
            AppCommand::Unpair { name } => {
                client
                    .unpair_device(crate::harold::UnpairDeviceRequest { name: name.clone() })
                    .await
                    .map_err(refused)?;
                Ok(format!("unpaired {name}"))
            }
        }
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// How long a test waits for a test server; only load can delay the answer.
    const TEST_WAIT: Duration = Duration::from_secs(30);

    fn parse(args: &[&str]) -> Result<Mode, String> {
        Mode::parse(&args.iter().map(|a| a.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn start_control_tower_parses_the_remote_flag() {
        assert_eq!(
            parse(&["start-control-tower", "--remote"]),
            Ok(Mode::StartControlTower(StartControlTowerArgs {
                endpoint: None,
                remote: true,
            }))
        );
        let expected = Ok(Mode::StartControlTower(StartControlTowerArgs {
            endpoint: Some("http://x:1".into()),
            remote: true,
        }));
        assert_eq!(
            parse(&[
                "start-control-tower",
                "--remote",
                "--endpoint",
                "http://x:1"
            ]),
            expected
        );
        assert_eq!(
            parse(&[
                "start-control-tower",
                "--endpoint",
                "http://x:1",
                "--remote"
            ]),
            expected
        );
    }

    #[test]
    fn control_tower_command_passes_remote_first() {
        let folder = tower_folder(true);
        let command = control_tower_command(
            &folder,
            &settings_for("127.0.0.1:50060"),
            &StartControlTowerArgs {
                endpoint: None,
                remote: true,
            },
        )
        .unwrap();
        assert_eq!(
            command_args(&command),
            [
                "--remote",
                "--endpoint",
                "http://127.0.0.1:50060",
                "--host-marker",
                "glyph"
            ]
        );
        std::fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn start_control_tower_parses_with_and_without_an_endpoint() {
        assert_eq!(
            parse(&["start-control-tower"]),
            Ok(Mode::StartControlTower(StartControlTowerArgs {
                endpoint: None,
                remote: false,
            }))
        );
        assert_eq!(
            parse(&["start-control-tower", "--endpoint", "http://127.0.0.1:1"]),
            Ok(Mode::StartControlTower(StartControlTowerArgs {
                endpoint: Some("http://127.0.0.1:1".into()),
                remote: false,
            }))
        );
    }

    #[test]
    fn start_control_tower_refuses_a_missing_value_a_repeat_and_an_unknown_argument() {
        for rest in [
            vec!["--endpoint"],
            vec!["--endpoint", "a", "--endpoint", "b"],
            vec!["--remote", "--remote"],
            vec!["--remote", "extra"],
            vec!["--bogus"],
            vec!["extra"],
        ] {
            let mut args = vec!["start-control-tower"];
            args.extend(rest);
            assert_eq!(parse(&args), Err(START_CONTROL_TOWER_USAGE.to_string()));
        }
    }

    fn tower_folder(with_program: bool) -> std::path::PathBuf {
        let folder = std::env::temp_dir().join(format!("harold-tower-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&folder).unwrap();
        if with_program {
            std::fs::write(folder.join("control-tower"), "").unwrap();
        }
        folder
    }

    fn command_args(command: &std::process::Command) -> Vec<String> {
        command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn control_tower_command_uses_the_settings_address() {
        let folder = tower_folder(true);
        let command = control_tower_command(
            &folder,
            &settings_for("127.0.0.1:50060"),
            &StartControlTowerArgs {
                endpoint: None,
                remote: false,
            },
        )
        .unwrap();
        assert_eq!(command.get_program(), folder.join("control-tower"));
        assert_eq!(
            command_args(&command),
            [
                "--endpoint",
                "http://127.0.0.1:50060",
                "--host-marker",
                "glyph"
            ]
        );
        std::fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn control_tower_command_passes_the_host_marker() {
        let folder = tower_folder(true);
        let mut settings = settings_for("127.0.0.1:50060");
        let args = StartControlTowerArgs {
            endpoint: None,
            remote: false,
        };
        let command = control_tower_command(&folder, &settings, &args).unwrap();
        assert_eq!(
            command_args(&command),
            [
                "--endpoint",
                "http://127.0.0.1:50060",
                "--host-marker",
                "glyph"
            ]
        );
        settings.control_tower.host_marker = "emoji".into();
        let command = control_tower_command(&folder, &settings, &args).unwrap();
        assert_eq!(
            command_args(&command),
            [
                "--endpoint",
                "http://127.0.0.1:50060",
                "--host-marker",
                "emoji"
            ]
        );
        std::fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn control_tower_command_lets_a_given_endpoint_win() {
        let folder = tower_folder(true);
        let command = control_tower_command(
            &folder,
            &settings_for("127.0.0.1:50060"),
            &StartControlTowerArgs {
                endpoint: Some("http://127.0.0.1:9".into()),
                remote: false,
            },
        )
        .unwrap();
        assert_eq!(
            command_args(&command),
            ["--endpoint", "http://127.0.0.1:9", "--host-marker", "glyph"]
        );
        std::fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn control_tower_command_names_the_path_when_the_file_is_missing() {
        let folder = tower_folder(false);
        let error = control_tower_command(
            &folder,
            &settings_for("127.0.0.1:50060"),
            &StartControlTowerArgs {
                endpoint: None,
                remote: false,
            },
        )
        .unwrap_err();
        assert_eq!(
            error,
            format!(
                "harold: no control-tower at {}. Install Harold again.",
                folder.join("control-tower").display()
            )
        );
        std::fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn control_tower_command_uses_only_the_given_folder() {
        let empty = tower_folder(false);
        let other = tower_folder(true);
        let error = control_tower_command(
            &empty,
            &settings_for("127.0.0.1:50060"),
            &StartControlTowerArgs {
                endpoint: None,
                remote: false,
            },
        )
        .unwrap_err();
        assert_eq!(
            error,
            format!(
                "harold: no control-tower at {}. Install Harold again.",
                empty.join("control-tower").display()
            )
        );
        std::fs::remove_dir_all(&empty).unwrap();
        std::fs::remove_dir_all(&other).unwrap();
    }

    #[test]
    fn build_features_parses_alone_only() {
        assert_eq!(parse(&["--build-features"]), Ok(Mode::BuildFeatures));
        assert!(parse(&["--build-features", "--check-config"]).is_err());
    }

    #[test]
    fn knowledge_commands_parse() {
        let tool = |rest: &[&str]| {
            let mut args = vec!["tool", "knowledge"];
            args.extend_from_slice(rest);
            parse(&args)
        };
        assert_eq!(
            tool(&["search", "keeping", "the", "index", "safe"]),
            Ok(Mode::Tool(ToolCommand::Knowledge(
                KnowledgeCommand::Search {
                    query: "keeping the index safe".into(),
                    limit: 10
                }
            )))
        );
        assert_eq!(
            tool(&["search", "buckwal", "--limit", "3"]),
            Ok(Mode::Tool(ToolCommand::Knowledge(
                KnowledgeCommand::Search {
                    query: "buckwal".into(),
                    limit: 3
                }
            )))
        );
        assert_eq!(
            tool(&["recent"]),
            Ok(Mode::Tool(ToolCommand::Knowledge(
                KnowledgeCommand::Recent { limit: 10 }
            )))
        );
        assert_eq!(
            tool(&["related", "references/hyperdx", "--limit", "5"]),
            Ok(Mode::Tool(ToolCommand::Knowledge(
                KnowledgeCommand::Related {
                    name: "references/hyperdx".into(),
                    limit: 5
                }
            )))
        );
        assert_eq!(
            tool(&["links", "a-note"]),
            Ok(Mode::Tool(ToolCommand::Knowledge(
                KnowledgeCommand::Links {
                    name: "a-note".into()
                }
            )))
        );
        assert_eq!(
            tool(&["show", "a-note"]),
            Ok(Mode::Tool(ToolCommand::Knowledge(KnowledgeCommand::Show {
                name: "a-note".into()
            })))
        );
        assert_eq!(
            tool(&["status"]),
            Ok(Mode::Tool(ToolCommand::Knowledge(KnowledgeCommand::Status)))
        );
        assert_eq!(
            tool(&["rebuild"]),
            Ok(Mode::Tool(ToolCommand::Knowledge(
                KnowledgeCommand::Rebuild
            )))
        );
        let knowledge = |command| Ok(Mode::Tool(ToolCommand::Knowledge(command)));
        assert_eq!(
            tool(&["relate", "a", "part_of", "b"]),
            knowledge(KnowledgeCommand::Relate {
                from: "a".into(),
                relation: "part_of".into(),
                to: "b".into()
            })
        );
        assert_eq!(
            tool(&["unrelate", "a", "part_of", "b"]),
            knowledge(KnowledgeCommand::Unrelate {
                from: "a".into(),
                relation: "part_of".into(),
                to: "b".into()
            })
        );
        assert_eq!(
            tool(&["relations", "a"]),
            knowledge(KnowledgeCommand::Relations { name: "a".into() })
        );
        assert_eq!(
            tool(&["graph", "status"]),
            knowledge(KnowledgeCommand::GraphStatus)
        );
        assert_eq!(
            tool(&["graph", "rebuild"]),
            knowledge(KnowledgeCommand::GraphRebuild)
        );
        assert_eq!(
            tool(&["graph", "void", "1291", "--reason", "typo"]),
            knowledge(KnowledgeCommand::GraphVoid {
                sequence: 1291,
                reason: "typo".into()
            })
        );
        for bad in [
            vec!["relate", "a", "part_of"],
            vec!["relations"],
            vec!["graph"],
            vec!["graph", "void", "x", "--reason", "typo"],
            vec!["graph", "void", "5"],
        ] {
            assert_eq!(tool(&bad), Err(KNOWLEDGE_USAGE_LINE.to_string()), "{bad:?}");
        }
        for bad in [
            vec![],
            vec!["search"],
            vec!["search", "x", "--limit", "0"],
            vec!["search", "x", "--limit", "11"],
            vec!["search", "x", "--limit", "many"],
            vec!["related"],
            vec!["links"],
            vec!["show", "a", "b"],
            vec!["status", "now"],
            vec!["forget"],
        ] {
            assert_eq!(tool(&bad), Err(KNOWLEDGE_USAGE_LINE.to_string()), "{bad:?}");
        }
    }

    #[test]
    fn graph_refusals_and_output_are_plain_lines() {
        let relate = KnowledgeCommand::Relate {
            from: "a".into(),
            relation: "part_of".into(),
            to: "b".into(),
        };
        let line = |status: tonic::Status| knowledge_refusal_line(&status, &relate);
        assert!(line(tonic::Status::unavailable("graph_off")).contains("is off"));
        assert!(line(tonic::Status::unavailable("graph_behind")).contains("graph status"));
        assert!(line(tonic::Status::unavailable("graph_rebuilding")).contains("rebuilding"));
        assert!(line(tonic::Status::unavailable("log_failed")).contains("nothing changed"));
        assert!(line(tonic::Status::unavailable("typedb_unreachable")).contains("TypeDB"));
        assert!(line(tonic::Status::failed_precondition("not_refused")).contains("not refused"));
        assert_eq!(
            line(tonic::Status::invalid_argument("invalid relation")),
            "invalid relation"
        );
        assert!(
            line(tonic::Status::not_found("unknown_vault_note")).contains("unknown vault note")
        );
        let rows = [knowledge_api::GraphRelation {
            name: "b".into(),
            title: "B".into(),
            relation: "supersedes".into(),
            outgoing: true,
            missing: true,
        }];
        assert_eq!(
            knowledge_relations_text(&rows),
            "-> supersedes b (missing) B"
        );
        assert_eq!(knowledge_relations_text(&[]), "no relations");
        let status = knowledge_api::GraphStatusResponse {
            enabled: true,
            has_data_position: true,
            data_position: 5,
            has_last_refusal: true,
            last_refusal_sequence: 6,
            last_refusal_reason: "refused".into(),
            ..Default::default()
        };
        let text = knowledge_graph_status_text(&status);
        assert!(text.contains("data 5") && text.contains("schema none"));
        assert!(text.contains("last refusal: record 6 (refused)"));
    }

    #[test]
    fn a_knowledge_hit_prints_score_name_kind_and_description() {
        let hit = knowledge_api::Hit {
            name: "a-note".into(),
            kind: "idea".into(),
            description: "About things".into(),
            score: 0.8123,
            linked: true,
            ..Default::default()
        };
        assert_eq!(
            knowledge_hit_line(&hit, true),
            "0.81 a-note idea (linked) About things"
        );
        assert_eq!(
            knowledge_hit_line(&hit, false),
            "- a-note idea (linked) About things"
        );
    }

    #[test]
    fn knowledge_refusals_are_one_plain_line() {
        let show = KnowledgeCommand::Show {
            name: "nope".into(),
        };
        let line = |status: tonic::Status| knowledge_refusal_line(&status, &show);
        assert_eq!(
            line(tonic::Status::not_found("unknown_vault_note")),
            "unknown vault note nope"
        );
        assert_eq!(
            line(tonic::Status::unavailable("search_resting: failed_to_load")),
            "search is resting: the model didn't load"
        );
        assert_eq!(
            line(tonic::Status::unimplemented("")),
            "the Harold daemon does not serve the knowledge tool"
        );
    }

    #[test]
    fn knowledge_status_names_the_vault_trouble() {
        let status = knowledge_api::StatusResponse {
            vault_notes: 77,
            passages: 108,
            runtime: "llama.cpp".into(),
            model: "m.gguf".into(),
            dimensions: 768,
            worker: "idle".into(),
            vault_unreadable: true,
            ..Default::default()
        };
        let text = knowledge_status_text(&status);
        assert!(text.contains("vault notes: 77"));
        assert!(text.contains("passages: 108"));
        assert!(text.contains("no pass has succeeded yet"));
        assert!(text.contains("searches answer from the last index"));
    }

    #[test]
    fn slaves_and_master_commands_parse() {
        use crate::slave::pairing::{MasterCommand, SlavesCommand};
        assert_eq!(
            parse(&["slaves", "pair", "omarchy"]),
            Ok(Mode::Slaves(SlavesCommand::Pair {
                name: "omarchy".into()
            }))
        );
        assert_eq!(
            parse(&["slaves", "unpair", "omarchy"]),
            Ok(Mode::Slaves(SlavesCommand::Unpair {
                name: "omarchy".into()
            }))
        );
        assert_eq!(
            parse(&["master", "pair", "abc", "--address", "mac:4333"]),
            Ok(Mode::Master(MasterCommand::Pair {
                secret: "abc".into(),
                address: "mac:4333".into()
            }))
        );
        for bad in [
            vec!["slaves"],
            vec!["slaves", "pair"],
            vec!["slaves", "pair", "--all"],
            vec!["slaves", "list"],
            vec!["master", "pair", "abc"],
            vec!["master", "pair", "abc", "--address"],
        ] {
            assert!(parse(&bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn app_commands_parse() {
        use super::AppCommand::*;
        assert_eq!(
            parse(&["app", "pair"]),
            Ok(Mode::App(Pair { unlock: false }))
        );
        assert_eq!(
            parse(&["app", "pair", "--unlock"]),
            Ok(Mode::App(Pair { unlock: true }))
        );
        assert_eq!(parse(&["app", "devices"]), Ok(Mode::App(Devices)));
        assert_eq!(
            parse(&["app", "unpair", "iPhone, paired 23 Oct"]),
            Ok(Mode::App(Unpair {
                name: "iPhone, paired 23 Oct".into()
            }))
        );
        for bad in [
            vec!["app"],
            vec!["app", "pair", "123456"],
            vec!["app", "pair", "--code", "x"],
            vec!["app", "unpair"],
            vec!["app", "unpair", "--all"],
            vec!["app", "devices", "extra"],
        ] {
            assert_eq!(parse(&bad), Err(APP_USAGE_LINE.to_string()), "{bad:?}");
        }
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
    fn accept_maps_each_event_to_working_or_done() {
        assert_eq!(AcceptEvent::TurnStarted.state(), AgentState::Working);
        assert_eq!(AcceptEvent::StillWorking.state(), AgentState::Working);
        assert_eq!(AcceptEvent::TurnCompleted.state(), AgentState::Done);
        assert_eq!(AcceptEvent::SubagentsFinished.state(), AgentState::Done);
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
    fn the_caller_pane_comes_from_tmux_pane_or_rex_block() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| (*value).to_string())
            }
        };
        assert_eq!(caller_pane_from(env(&[("TMUX_PANE", "%47")])), "%47");
        assert_eq!(
            caller_pane_from(env(&[("REX_BLOCK", "block:abc")])),
            "block:abc"
        );
        assert_eq!(
            caller_pane_from(env(&[("TMUX_PANE", ""), ("REX_BLOCK", "block:abc")])),
            "block:abc"
        );
        assert_eq!(caller_pane_from(env(&[])), "");
    }

    #[test]
    fn the_caller_pane_becomes_the_source_id_when_there_is_none() {
        let words = |list: &[&str]| {
            list.iter()
                .map(|word| (*word).to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            with_caller_pane_from(words(&["speak", "hello"]), "block:abc".into()),
            words(&["speak", "--src-id", "block:abc", "hello"])
        );
        assert_eq!(
            with_caller_pane_from(
                words(&["send-message-to-away-device", "--src-id", "%1", "hi"]),
                "block:abc".into()
            ),
            words(&["send-message-to-away-device", "--src-id", "%1", "hi"])
        );
        assert_eq!(
            with_caller_pane_from(words(&["speak", "hello"]), String::new()),
            words(&["speak", "hello"])
        );
        assert_eq!(
            with_caller_pane_from(words(&["accept", "turn-started"]), "%1".into()),
            words(&["accept", "turn-started"])
        );
    }

    #[test]
    fn a_pane_id_of_the_other_multiplexer_is_a_refusal_line() {
        let message = "Harold works with rex panes, not tmux";
        assert_eq!(
            refusal_line(tonic::Code::FailedPrecondition, message, "%47"),
            format!("refused: {message}")
        );
        assert_eq!(
            speak_refusal_line(tonic::Code::FailedPrecondition, message, "%47"),
            format!("refused: {message}")
        );
        assert!(valid_pane_id("block:01a18fe6-c4f7-7b52-a9f2-9682b5c097c7"));
        assert!(valid_pane_id("%47"));
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
    fn speak_unavailable_prints_the_lookup_line_and_internal_keeps_the_store_line() {
        assert_eq!(
            speak_refusal_line(tonic::Code::Unavailable, "agent lookup unavailable", "%47"),
            "the agent lookup is not available now; try again"
        );
        assert_eq!(
            speak_refusal_line(tonic::Code::Internal, "event store write failed", "%47"),
            "the Harold daemon could not store the spoken line"
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
        let usage =
            "invalid argument: usage is harold tool questions put|dismiss|order|list|show|checked";
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
            Err(
                "invalid argument: usage is harold tool questions put|dismiss|order|list|show|checked"
                    .into()
            )
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

    const INSTRUCTIONS_JSON: &str = r#"{
        "id": "I1", "product": "harold", "title": "Do things",
        "steps": [{"text": "one", "detail": "more"}, {"text": "two"}]
    }"#;

    #[test]
    fn put_json_for_instructions_translates_steps() {
        let q = question_from_json(INSTRUCTIONS_JSON).unwrap();
        assert_eq!(
            (&*q.id, &*q.product, &*q.title),
            ("I1", "harold", "Do things")
        );
        assert_eq!(q.steps.len(), 2);
        assert_eq!((&*q.steps[0].text, &*q.steps[0].detail), ("one", "more"));
        assert_eq!((&*q.steps[1].text, &*q.steps[1].detail), ("two", ""));
        assert!(q.decisions.is_empty());
        assert_eq!((&*q.spec, &*q.sha, &*q.link, &*q.intro), ("", "", "", ""));
    }

    #[test]
    fn put_json_with_steps_and_decisions_is_translated_and_left_to_the_daemon_to_refuse() {
        let text = r#"{"id":"I1","product":"p","title":"t","steps":[{"text":"a"}],
            "decisions":[{"ask":"x","options":[]}]}"#;
        let q = question_from_json(text).unwrap();
        assert_eq!(q.steps.len(), 1);
        assert_eq!(q.decisions.len(), 1);
        // Plain questions stay strict.
        let error = question_from_json(r#"{"id":"Q","product":"p"}"#).unwrap_err();
        assert!(error.contains("decisions"), "{error}");
        let error = question_from_json(r#"{"id":"I","product":"p","steps":[5]}"#).unwrap_err();
        assert!(error.contains("step"), "{error}");
    }

    #[test]
    fn list_prints_the_steps_line() {
        let instructions = questions::ListedQuestion {
            question: Some(questions::Question {
                id: "I1".into(),
                product: "harold".into(),
                steps: vec![questions::Step::default(); 4],
                ..Default::default()
            }),
            version: 2,
            ticked: vec![1, 3],
            picks: vec![],
            spec_state: 0,
            spec_page: String::new(),
        };
        assert_eq!(list_line(&instructions), "I1 v2 harold steps 2/4");
        let question = questions::ListedQuestion {
            question: Some(questions::Question {
                id: "Q1".into(),
                product: "harold".into(),
                spec: "dash".into(),
                ..Default::default()
            }),
            version: 1,
            ticked: vec![],
            picks: vec![],
            spec_state: 0,
            spec_page: String::new(),
        };
        assert_eq!(list_line(&question), "Q1 v1 harold dash picks 0/0");
    }

    #[test]
    fn show_parses_with_one_id() {
        assert_eq!(
            parse(&["tool", "questions", "show", "Q43"]),
            Ok(Mode::Tool(ToolCommand::QuestionsShow { id: "Q43".into() }))
        );
        assert!(parse(&["tool", "questions", "show"]).is_err());
        assert!(parse(&["tool", "questions", "show", "a", "b"]).is_err());
    }

    fn option(key: &str, label: &str, free: bool) -> questions::DecisionOption {
        questions::DecisionOption {
            key: key.into(),
            label: label.into(),
            free,
            ..Default::default()
        }
    }

    #[test]
    fn show_prints_the_head_each_pick_free_text_and_no_pick() {
        let decision = |ask: &str| questions::Decision {
            ask: ask.into(),
            options: vec![option("a", "Alpha", false), option("z", "Other", true)],
        };
        let shown = questions::ShowQuestionResponse {
            question: Some(questions::Question {
                id: "Q43".into(),
                product: "harold".into(),
                spec: "watch-jobs".into(),
                decisions: vec![decision("Which?"), decision("How?"), decision("When?")],
                ..Default::default()
            }),
            version: 2,
            state: "waiting".into(),
            reason: String::new(),
            picks: vec![
                questions::Pick {
                    decision: 0,
                    key: "a".into(),
                    text: String::new(),
                },
                questions::Pick {
                    decision: 2,
                    key: "z".into(),
                    text: "on Friday".into(),
                },
            ],
            spec_state: 0,
        };
        assert_eq!(
            show_lines(&shown),
            "Q43 v2 harold watch-jobs waiting\n  1 Which?: a Alpha\n  2 How?: no pick\n  3 When?: z \"on Friday\""
        );
    }

    #[test]
    fn list_line_shows_picks_n_of_m() {
        let decision = questions::Decision::default();
        let listed = questions::ListedQuestion {
            question: Some(questions::Question {
                id: "Q43".into(),
                product: "harold".into(),
                spec: "watch-jobs".into(),
                decisions: vec![decision; 4],
                ..Default::default()
            }),
            version: 1,
            ticked: vec![],
            picks: vec![questions::Pick::default(); 2],
            spec_state: 0,
            spec_page: String::new(),
        };
        assert_eq!(list_line(&listed), "Q43 v1 harold watch-jobs picks 2/4");
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
            Box::pin(async {
                crate::tools::Typing::Typed {
                    insert_pressed: false,
                }
            })
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

    #[test]
    fn checked_parses_and_takes_nothing_more() {
        assert_eq!(
            parse(&["tool", "questions", "checked"]),
            Ok(Mode::Tool(ToolCommand::QuestionsChecked))
        );
        assert!(parse(&["tool", "questions", "checked", "extra"]).is_err());
    }

    #[test]
    fn put_carries_the_spec_file_when_the_json_names_one() {
        let named = SPEC_JSON.replacen(
            "\"sha\"",
            "\"spec_file\": \"/repo/.superpowers/pod/specs/dash.md\", \"sha\"",
            1,
        );
        assert_eq!(
            question_from_json(&named).unwrap().spec_file,
            "/repo/.superpowers/pod/specs/dash.md"
        );
        assert_eq!(question_from_json(SPEC_JSON).unwrap().spec_file, "");
    }

    #[test]
    fn list_marks_a_changed_spec_after_everything_else() {
        let listed = |spec_state, steps: Vec<questions::Step>| questions::ListedQuestion {
            question: Some(questions::Question {
                id: "Q1".into(),
                product: "harold".into(),
                spec: "dash".into(),
                steps,
                ..Default::default()
            }),
            version: 1,
            spec_state: questions::SpecState::from_str_name(spec_state).unwrap() as i32,
            ..Default::default()
        };
        assert_eq!(
            list_line(&listed("SPEC_STATE_CHANGED", vec![])),
            "Q1 v1 harold dash picks 0/0 (changed spec)"
        );
        for state in ["SPEC_STATE_CURRENT", "SPEC_STATE_UNKNOWN"] {
            assert_eq!(
                list_line(&listed(state, vec![])),
                "Q1 v1 harold dash picks 0/0"
            );
        }
        let steps = vec![questions::Step {
            text: "one".into(),
            detail: String::new(),
        }];
        assert_eq!(
            list_line(&listed("SPEC_STATE_CHANGED", steps)),
            "Q1 v1 harold steps 0/1 (changed spec)"
        );
    }

    fn wall_ms() -> i64 {
        (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
    }

    /// A registry of one product, whose repo is `repo`.
    struct OneProduct(std::path::PathBuf);
    impl crate::tools::Products for OneProduct {
        fn products(&self) -> Result<Vec<(String, std::path::PathBuf)>, &'static str> {
            Ok(vec![("harold".into(), self.0.clone())])
        }
    }

    #[tokio::test]
    async fn checked_prints_the_time_and_list_marks_a_spec_that_changed() {
        let folder = std::env::temp_dir().join(format!("harold-cli-tool-{}", uuid::Uuid::new_v4()));
        let specs = folder.join("repo/.superpowers/pod/specs");
        std::fs::create_dir_all(&specs).unwrap();
        let repo = std::fs::canonicalize(folder.join("repo")).unwrap();
        let spec = std::fs::canonicalize(&specs).unwrap().join("dash.md");
        std::fs::write(&spec, b"first").unwrap();
        let sha = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(b"first"));
        let (stop, shutdown) = tokio::sync::watch::channel(());
        let handle = crate::tools::questions::start(crate::tools::ToolContext {
            data_folder: folder.join("data"),
            typing: Some(std::sync::Arc::new(Typist)),
            chief_pane: std::sync::Arc::new(NoChief),
            pane_folder: std::sync::Arc::new(NoFolder),
            products: std::sync::Arc::new(OneProduct(repo)),
            agents: std::sync::Arc::new(crate::tools::tests::NoAgents),
            pods: std::sync::Arc::new(crate::tools::tests::NoPods),
            watch_settings: Default::default(),
            model_environment: Vec::new(),
            cli_path: "claude".into(),
            grpc_addr: None,
            watch_counts: std::sync::Arc::new(
                tokio::sync::watch::channel(crate::tools::WatchCounts::default()).0,
            ),
            pod_registry: None,
            shutdown,
        })
        .await
        .unwrap_or_else(|e| panic!("start failed: {}", e.reason));
        let serving = serve(handle.routes).await;
        let address = serving.address.clone();

        let json = SPEC_JSON.replacen("abc123", &sha[..12], 1).replacen(
            "\"sha\"",
            &format!("\"spec_file\": \"{}\", \"sha\"", spec.display()),
            1,
        );
        let put = tool_within(
            &address,
            &ToolCommand::QuestionsPut,
            json.as_bytes(),
            TEST_WAIT,
        )
        .await;
        let current = tool_within(&address, &ToolCommand::QuestionsList, &b""[..], TEST_WAIT).await;
        std::fs::write(&spec, b"first, then revised").unwrap();
        let changed = tool_within(&address, &ToolCommand::QuestionsList, &b""[..], TEST_WAIT).await;
        let before = watch_time(wall_ms(), crate::telemetry::local_offset());
        let checked = tool_within(
            &address,
            &ToolCommand::QuestionsChecked,
            &b""[..],
            TEST_WAIT,
        )
        .await;
        let after = watch_time(wall_ms(), crate::telemetry::local_offset());
        let outside = tool_within(
            &address,
            &ToolCommand::QuestionsPut,
            json.replacen(&spec.display().to_string(), "/etc/hosts.md", 1)
                .as_bytes(),
            TEST_WAIT,
        )
        .await;

        drop(serving);
        drop(stop);
        let _ = handle.task.await;
        let _ = std::fs::remove_dir_all(&folder);
        assert_eq!(put, Ok("stored q-1 v1".into()));
        assert_eq!(
            current,
            Ok("q-1 v1 harold dash picks 0/1\n1 waiting".into())
        );
        assert_eq!(
            changed,
            Ok("q-1 v1 harold dash picks 0/1 (changed spec)\n1 waiting".into())
        );
        let checked = checked.unwrap();
        assert!(
            [
                format!("checked at {before}"),
                format!("checked at {after}")
            ]
            .contains(&checked),
            "{checked} between {before} and {after}"
        );
        assert_eq!(outside, Err("invalid spec_file".into()));
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
            agents: std::sync::Arc::new(crate::tools::tests::NoAgents),
            pods: std::sync::Arc::new(crate::tools::tests::NoPods),
            watch_settings: Default::default(),
            model_environment: Vec::new(),
            cli_path: "claude".into(),
            grpc_addr: None,
            watch_counts: std::sync::Arc::new(
                tokio::sync::watch::channel(crate::tools::WatchCounts::default()).0,
            ),
            pod_registry: None,
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
            (
                ToolCommand::QuestionsShow { id: "q-1".into() },
                String::new(),
            ),
        ] {
            lines.push(
                tool_within(&address, &command, input.as_bytes(), TEST_WAIT)
                    .await
                    .unwrap(),
            );
        }
        let refused = tool_within(
            &address,
            &ToolCommand::QuestionsPut,
            SPEC_JSON.replace("https://", "ftp://").as_bytes(),
            TEST_WAIT,
        )
        .await
        .unwrap_err();
        let missing = tool_within(
            &address,
            &ToolCommand::QuestionsDismiss {
                id: "q-9".into(),
                reason: "x".into(),
            },
            &b""[..],
            TEST_WAIT,
        )
        .await
        .unwrap_err();

        let unknown = tool_within(
            &address,
            &ToolCommand::QuestionsShow { id: "q-9".into() },
            &b""[..],
            TEST_WAIT,
        )
        .await
        .unwrap_err();

        server.abort();
        accept.abort();
        drop(stop);
        let _ = handle.task.await;
        println!("{lines:#?}\nrefused: {refused}\nmissing: {missing}");
        assert_eq!(unknown, "unknown_question");
        assert_eq!(
            lines[8],
            "q-1 v2 harold dash dismissed\n  1 Which store?: no pick"
        );

        assert_eq!(lines[0], "stored q-1 v1");
        assert_eq!(lines[1], "stored q-1 v2");
        assert_eq!(lines[2], "stored q-2 v1");
        assert_eq!(
            lines[3],
            "q-1 v2 harold dash picks 0/1\nq-2 v1 harold other picks 0/1\n2 waiting"
        );
        assert_eq!(lines[4], "ordered 2");
        assert_eq!(
            lines[5],
            "q-2 v1 harold other picks 0/1\nq-1 v2 harold dash picks 0/1\n2 waiting"
        );
        assert_eq!(lines[6], "dismissed q-1");
        assert_eq!(lines[7], "q-2 v1 harold other picks 0/1\n1 waiting");
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
        crate::settings::settings_with(&format!("[grpc]\nhost = \"{host}\"\nport = {port}"))
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
            Err(
                "invalid argument: usage is harold tool questions put|dismiss|order|list|show|checked"
                    .into()
            )
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
                "refused: not run in a tmux or Rex pane",
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

    fn telemetry_parse(words: &[&str]) -> Result<Mode, String> {
        let mut args = vec!["tool", "telemetry"];
        args.extend_from_slice(words);
        parse(&args)
    }

    fn telemetry_mode(command: TelemetryCommand) -> Result<Mode, String> {
        Ok(Mode::Tool(ToolCommand::Telemetry(command)))
    }

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(key, value)| ((*key).into(), (*value).into()))
            .collect()
    }

    #[test]
    fn tool_telemetry_commands_parse() {
        use TelemetryCommand as C;
        let n = || "harold.release.attempts".to_string();
        for (words, command) in [
            (
                vec![
                    "declare",
                    "harold.release.attempts",
                    "counter",
                    "Release attempts, by result",
                    "--attr",
                    "result=ok,failed",
                ],
                C::Declare {
                    name: n(),
                    kind: TelemetryKind::Counter,
                    meaning: "Release attempts, by result".into(),
                    attrs: vec![("result".into(), vec!["ok".into(), "failed".into()])],
                },
            ),
            (
                vec![
                    "declare",
                    "chief.load",
                    "gauge",
                    "Load",
                    "--attr",
                    "host=a,b",
                    "--attr",
                    "slot=0-3",
                ],
                C::Declare {
                    name: "chief.load".into(),
                    kind: TelemetryKind::Gauge,
                    meaning: "Load".into(),
                    attrs: vec![
                        ("host".into(), vec!["a".into(), "b".into()]),
                        ("slot".into(), vec!["0-3".into()]),
                    ],
                },
            ),
            (
                vec!["declare", "chief.digest", "event", "A digest was sent"],
                C::Declare {
                    name: "chief.digest".into(),
                    kind: TelemetryKind::Event,
                    meaning: "A digest was sent".into(),
                    attrs: vec![],
                },
            ),
            (
                vec!["count", "harold.release.attempts"],
                C::Count {
                    name: n(),
                    n: 1,
                    attrs: vec![],
                },
            ),
            (
                vec![
                    "count",
                    "harold.release.attempts",
                    "3",
                    "--attr",
                    "result=failed",
                ],
                C::Count {
                    name: n(),
                    n: 3,
                    attrs: pairs(&[("result", "failed")]),
                },
            ),
            (
                vec!["count", "harold.release.attempts", "--attr", "result=ok"],
                C::Count {
                    name: n(),
                    n: 1,
                    attrs: pairs(&[("result", "ok")]),
                },
            ),
            (
                vec!["gauge", "chief.load", "-2.5", "--attr", "host=a"],
                C::Gauge {
                    name: "chief.load".into(),
                    value: GaugeValue(-2.5),
                    attrs: pairs(&[("host", "a")]),
                },
            ),
            (
                vec![
                    "event",
                    "chief.digest",
                    "--attr",
                    "items=4",
                    "--attr",
                    "kind=week",
                ],
                C::Event {
                    name: "chief.digest".into(),
                    attrs: pairs(&[("items", "4"), ("kind", "week")]),
                },
            ),
            (vec!["list"], C::List { prefix: None }),
            (
                vec!["list", "harold"],
                C::List {
                    prefix: Some("harold".into()),
                },
            ),
            (
                vec!["remove", "chief.digest"],
                C::Remove {
                    name: "chief.digest".into(),
                },
            ),
        ] {
            assert_eq!(
                telemetry_parse(&words),
                telemetry_mode(command),
                "{words:?}"
            );
        }
    }

    #[test]
    fn a_malformed_telemetry_command_is_refused_with_usage() {
        let usage = "invalid argument: usage is harold tool telemetry declare|count|gauge|event|list|remove";
        assert_eq!(TELEMETRY_USAGE_LINE, usage);
        for words in [
            vec![],
            vec!["record", "chief.x"],
            vec!["declare"],
            vec!["declare", "chief.x", "counter"],
            vec!["declare", "chief.x", "timer", "Meaning"],
            vec!["declare", "--attr", "counter", "Meaning"],
            vec!["declare", "chief.x", "counter", "Meaning", "extra"],
            vec!["declare", "chief.x", "counter", "Meaning", "--attr"],
            vec![
                "declare", "chief.x", "counter", "Meaning", "--attr", "result",
            ],
            vec!["declare", "chief.x", "counter", "Meaning", "--attr", "=ok"],
            vec![
                "declare", "chief.x", "counter", "Meaning", "--attr", "a=1", "--attr", "a=2",
            ],
            vec!["declare", "chief.x", "counter", "Meaning", "--other", "a=1"],
            vec!["count"],
            vec!["count", "chief.x", "x"],
            vec!["count", "chief.x", "-1"],
            vec!["count", "chief.x", "0"],
            vec!["count", "chief.x", "1", "2"],
            vec!["count", "chief.x", "--attr", "a=1", "3"],
            vec!["count", "chief.x", "--attr", "a=1", "--attr", "a=2"],
            vec!["count", "chief.x", "--attr", "a"],
            vec!["gauge", "chief.x"],
            vec!["gauge", "chief.x", "four"],
            vec!["gauge", "chief.x", "1", "2"],
            vec!["event"],
            vec!["event", "chief.x", "extra"],
            vec!["event", "chief.x", "--attr"],
            vec!["list", "a", "b"],
            vec!["list", "--attr", "a=1"],
            vec!["remove"],
            vec!["remove", "chief.x", "extra"],
        ] {
            assert_eq!(telemetry_parse(&words), Err(usage.to_string()), "{words:?}");
        }
    }

    #[test]
    fn the_spec_declare_examples_build_the_right_requests() {
        let request = |words: &[&str]| {
            let Ok(Mode::Tool(ToolCommand::Telemetry(command))) = telemetry_parse(words) else {
                panic!("not a telemetry command: {words:?}");
            };
            telemetry_request(&command, "%2")
        };
        assert_eq!(
            request(&[
                "declare",
                "harold.release.attempts",
                "counter",
                "Release attempts, by result",
                "--attr",
                "result=ok,failed",
            ]),
            TelemetryRequest::Declare(telemetry_api::DeclareRequest {
                caller_pane: "%2".into(),
                name: "harold.release.attempts".into(),
                kind: telemetry_api::Kind::Counter as i32,
                meaning: "Release attempts, by result".into(),
                attrs: vec![telemetry_api::Attr {
                    key: "result".into(),
                    values: vec!["ok".into(), "failed".into()],
                }],
            })
        );
        assert_eq!(
            request(&[
                "declare",
                "chief.digest",
                "event",
                "A digest was sent",
                "--attr",
                "items=0-50",
            ]),
            TelemetryRequest::Declare(telemetry_api::DeclareRequest {
                caller_pane: "%2".into(),
                name: "chief.digest".into(),
                kind: telemetry_api::Kind::Event as i32,
                meaning: "A digest was sent".into(),
                attrs: vec![telemetry_api::Attr {
                    key: "items".into(),
                    values: vec!["0-50".into()],
                }],
            })
        );
    }

    #[test]
    fn every_telemetry_request_carries_the_pane_from_caller_pane_from() {
        use TelemetryCommand as C;
        let pane = caller_pane_from(|name: &str| (name == "TMUX_PANE").then(|| "%47".to_string()));
        assert_eq!(pane, "%47");
        let name = || "chief.x".to_string();
        let attrs = pairs(&[("k", "v")]);
        for (command, expected) in [
            (
                C::Count {
                    name: name(),
                    n: 2,
                    attrs: attrs.clone(),
                },
                TelemetryRequest::Count(telemetry_api::CountRequest {
                    caller_pane: pane.clone(),
                    name: name(),
                    n: 2,
                    attrs: vec![telemetry_api::Pair {
                        key: "k".into(),
                        value: "v".into(),
                    }],
                }),
            ),
            (
                C::Gauge {
                    name: name(),
                    value: GaugeValue(4.5),
                    attrs: vec![],
                },
                TelemetryRequest::Gauge(telemetry_api::GaugeRequest {
                    caller_pane: pane.clone(),
                    name: name(),
                    value: 4.5,
                    attrs: vec![],
                }),
            ),
            (
                C::Event {
                    name: name(),
                    attrs: vec![],
                },
                TelemetryRequest::Event(telemetry_api::EventRequest {
                    caller_pane: pane.clone(),
                    name: name(),
                    attrs: vec![],
                }),
            ),
            (
                C::List {
                    prefix: Some("chief".into()),
                },
                TelemetryRequest::List(telemetry_api::ListRequest {
                    caller_pane: pane.clone(),
                    prefix: "chief".into(),
                }),
            ),
            (
                C::Remove { name: name() },
                TelemetryRequest::Remove(telemetry_api::RemoveRequest {
                    caller_pane: pane.clone(),
                    name: name(),
                }),
            ),
        ] {
            assert_eq!(telemetry_request(&command, &pane), expected, "{command:?}");
        }
    }

    #[test]
    fn a_telemetry_refusal_prints_the_service_text() {
        use tonic::{Code, Status};
        for (status, line) in [
            (
                Status::permission_denied("run this from a pane: no TMUX_PANE or REX_BLOCK"),
                "run this from a pane: no TMUX_PANE or REX_BLOCK",
            ),
            (
                Status::invalid_argument(
                    "harold.release.attempts would have 120 series: the most is 100",
                ),
                "harold.release.attempts would have 120 series: the most is 100",
            ),
            (
                Status::new(Code::Unimplemented, "unknown service"),
                "the Harold daemon does not serve the telemetry tool",
            ),
            (
                Status::new(Code::Internal, ""),
                "the Harold daemon refused the call",
            ),
        ] {
            assert_eq!(telemetry_refusal_line(&status), line);
        }
    }

    #[test]
    fn a_telemetry_list_prints_one_line_for_each_signal() {
        let signal =
            |name: &str, kind: telemetry_api::Kind, meaning: &str, attrs| telemetry_api::Signal {
                name: name.into(),
                kind: kind as i32,
                meaning: meaning.into(),
                attrs,
            };
        let attr = |key: &str, values: &[&str]| telemetry_api::Attr {
            key: key.into(),
            values: values.iter().map(|value| (*value).into()).collect(),
        };
        let lines = telemetry_list_lines(&[
            signal(
                "harold.release.attempts",
                telemetry_api::Kind::Counter,
                "Release attempts, by result",
                vec![attr("result", &["ok", "failed"]), attr("step", &["0-3"])],
            ),
            signal(
                "chief.digest",
                telemetry_api::Kind::Event,
                "A digest was sent",
                vec![],
            ),
        ]);
        assert_eq!(
            lines,
            "harold.release.attempts counter \"Release attempts, by result\" result=ok,failed step=0-3\n\
             chief.digest event \"A digest was sent\""
        );
        assert_eq!(telemetry_list_lines(&[]), "no signals are declared");
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
            agents: std::sync::Arc::new(crate::tools::tests::NoAgents),
            pods: std::sync::Arc::new(crate::tools::tests::NoPods),
            watch_settings: Default::default(),
            model_environment: Vec::new(),
            cli_path: "claude".into(),
            grpc_addr: None,
            watch_counts: std::sync::Arc::new(
                tokio::sync::watch::channel(crate::tools::WatchCounts::default()).0,
            ),
            pod_registry: None,
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
            self.run_at(pane, words, input, time::OffsetDateTime::now_utc())
                .await
        }

        /// Runs the command as if the CLI's clock read `now`.
        async fn run_at(
            &self,
            pane: &str,
            words: &[&str],
            input: &str,
            now: time::OffsetDateTime,
        ) -> Result<String, String> {
            let Ok(Mode::Tool(ToolCommand::Backlog(command))) = backlog_parse(words) else {
                panic!("not a backlog command: {words:?}");
            };
            backlog_within(
                &self.serving.address,
                &command,
                input.as_bytes(),
                pane,
                time::UtcOffset::UTC,
                now,
                TEST_WAIT,
            )
            .await
        }
    }

    /// The telemetry tool over a temp folder, with the same panes as `backlog_server`:
    /// `%40` is in `harold`'s repo, `%51` in `turso`'s, and `%2` is the chief.
    async fn telemetry_server() -> BacklogServer {
        let folder = TempFolder::new();
        let (harold, turso) = (folder.0.join("harold"), folder.0.join("turso"));
        std::fs::create_dir_all(&harold).unwrap();
        std::fs::create_dir_all(&turso).unwrap();
        let (stop, shutdown) = tokio::sync::watch::channel(());
        let handle = crate::tools::telemetry::start(crate::tools::ToolContext {
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
            agents: std::sync::Arc::new(crate::tools::tests::NoAgents),
            pods: std::sync::Arc::new(crate::tools::tests::NoPods),
            watch_settings: Default::default(),
            model_environment: Vec::new(),
            cli_path: "claude".into(),
            grpc_addr: None,
            watch_counts: std::sync::Arc::new(
                tokio::sync::watch::channel(crate::tools::WatchCounts::default()).0,
            ),
            pod_registry: None,
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

    async fn telemetry_run(
        server: &BacklogServer,
        pane: &str,
        words: &[&str],
    ) -> Result<String, String> {
        let Ok(Mode::Tool(ToolCommand::Telemetry(command))) = telemetry_parse(words) else {
            panic!("not a telemetry command: {words:?}");
        };
        telemetry_within(&server.serving.address, &command, pane, TEST_WAIT).await
    }

    #[tokio::test]
    async fn the_chief_and_a_product_pane_record_signals_through_the_cli() {
        let server = telemetry_server().await;
        let run = |pane: &'static str, words: &'static [&'static str]| {
            telemetry_run(&server, pane, words)
        };
        let declare: &[&str] = &[
            "declare",
            "harold.release.attempts",
            "counter",
            "Release attempts, by result",
            "--attr",
            "result=ok,failed",
        ];
        assert_eq!(
            run("%40", declare).await,
            Ok("declared harold.release.attempts".into())
        );
        // The same declaration again is accepted with no change.
        assert_eq!(
            run("%2", declare).await,
            Ok("declared harold.release.attempts".into())
        );
        assert_eq!(
            run(
                "%40",
                &[
                    "count",
                    "harold.release.attempts",
                    "--attr",
                    "result=failed"
                ]
            )
            .await,
            Ok("counted harold.release.attempts by 1".into())
        );
        assert_eq!(
            run(
                "%2",
                &[
                    "declare",
                    "chief.digest",
                    "event",
                    "A digest was sent",
                    "--attr",
                    "items=0-50"
                ]
            )
            .await,
            Ok("declared chief.digest".into())
        );
        assert_eq!(
            run("%2", &["event", "chief.digest", "--attr", "items=4"]).await,
            Ok("recorded chief.digest".into())
        );
        assert_eq!(
            run("%2", &["declare", "chief.load", "gauge", "Load"]).await,
            Ok("declared chief.load".into())
        );
        assert_eq!(
            run("%2", &["gauge", "chief.load", "0.5"]).await,
            Ok("set chief.load to 0.5".into())
        );
        assert_eq!(
            run("%51", &["list", "harold"]).await,
            Ok(
                "harold.release.attempts counter \"Release attempts, by result\" result=ok,failed"
                    .into()
            )
        );
        // A pane that may not use the prefix, a run outside a pane, and a wrong value.
        assert_eq!(
            run(
                "%51",
                &["count", "harold.release.attempts", "--attr", "result=ok"]
            )
            .await,
            Err("harold. is for panes in harold's folder and the chief".into())
        );
        assert_eq!(
            run(
                "",
                &["count", "harold.release.attempts", "--attr", "result=ok"]
            )
            .await,
            Err("run this from a pane: no TMUX_PANE or REX_BLOCK".into())
        );
        assert_eq!(
            run(
                "%40",
                &["count", "harold.release.attempts", "--attr", "result=maybe"]
            )
            .await,
            Err("result=maybe is not declared: use ok or failed".into())
        );
        assert_eq!(
            run("%2", &["remove", "chief.digest"]).await,
            Ok("removed chief.digest".into())
        );
        assert_eq!(
            run("%2", &["event", "chief.digest", "--attr", "items=4"]).await,
            Err("chief.digest is not declared: declare it first".into())
        );
    }

    #[tokio::test]
    async fn a_daemon_with_no_telemetry_tool_gives_the_unimplemented_line() {
        let server = backlog_server().await;
        assert_eq!(
            telemetry_run(&server, "%2", &["list"]).await,
            Err("the Harold daemon does not serve the telemetry tool".into())
        );
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
            "Now: nothing\nParked:\n  1. N3 (0m) third, edited\n  2. N2 (0m) second\n  \
             3. N4 (0m) urgent\n  4. N5 (0m) top\nDone today:\n  N1 first"
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

    #[test]
    fn a_queue_age_rounds_down_to_minutes_hours_or_days() {
        let minute = 60_000;
        for (age_ms, expected) in [
            (-minute, "(0m)"),
            (59 * minute + 59_999, "(59m)"),
            (60 * minute, "(1h)"),
            (47 * 60 * minute + 59 * minute, "(47h)"),
            (48 * 60 * minute, "(2d)"),
            (5 * 24 * 60 * minute + 23 * 60 * minute, "(5d)"),
        ] {
            assert_eq!(queue_age(age_ms, 0), expected, "{age_ms}");
        }
    }

    #[tokio::test]
    async fn queue_list_shows_the_age_of_the_now_and_parked_items_only() {
        let server = backlog_server().await;
        let chief = "%2";
        for (words, input) in [
            (vec!["queue", "add", "--now"], "call design\n"),
            (vec!["queue", "add"], "mic button\n"),
            (vec!["queue", "add"], "finished\n"),
            (vec!["queue", "done", "N3"], ""),
        ] {
            server.run(chief, &words, input).await.unwrap();
        }
        let started = time::OffsetDateTime::now_utc();
        let mut listed = Vec::new();
        for later in [
            time::Duration::minutes(45),
            time::Duration::hours(3),
            time::Duration::days(2),
        ] {
            listed.push(
                server
                    .run_at(
                        chief,
                        &["queue", "list", "--done-since", "2026-01-01"],
                        "",
                        started + later,
                    )
                    .await
                    .unwrap(),
            );
        }
        println!("{}", listed.join("\n--\n"));
        for (listing, age) in listed.iter().zip(["(45m)", "(3h)", "(2d)"]) {
            assert!(
                listing.contains(&format!("Now: N1 {age} call design")),
                "{listing}"
            );
            assert!(
                listing.contains(&format!("  1. N2 {age} mic button")),
                "{listing}"
            );
            assert!(listing.ends_with("  N3 finished"), "{listing}");
        }
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
        assert_eq!(
            no_pane,
            Err("refused: not run in a tmux or Rex pane".into())
        );
        assert_eq!(unknown, Err("unknown_product".into()));
        assert_eq!(dry, Err("unknown_product".into()));
    }

    #[tokio::test]
    async fn queue_add_with_empty_stdin_is_refused() {
        let server = backlog_server().await;
        let empty = server.run("%2", &["queue", "add"], "").await;
        assert_eq!(empty, Err("invalid text".into()));
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
            agents: std::sync::Arc::new(crate::tools::tests::NoAgents),
            pods: std::sync::Arc::new(crate::tools::tests::NoPods),
            watch_settings: Default::default(),
            model_environment: Vec::new(),
            cli_path: "claude".into(),
            grpc_addr: None,
            watch_counts: std::sync::Arc::new(
                tokio::sync::watch::channel(crate::tools::WatchCounts::default()).0,
            ),
            pod_registry: None,
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
            TEST_WAIT,
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
                TEST_WAIT,
            )
            .await
            .unwrap_err();
            assert!(line.starts_with("invalid JSON: "), "{input:?}: {line}");
            assert!(!line.contains('\n'), "{line}");
        }
    }

    // -- harold tool watch --------------------------------------------------------

    fn watch(args: &[&str]) -> Result<Mode, String> {
        let mut all = vec!["tool", "watch"];
        all.extend(args);
        parse(&all)
    }

    #[test]
    fn tool_watch_start_parses_its_flags_in_any_order() {
        let expected = WatchCommand::Start {
            name: "after-release".into(),
            skill: "pod-watch".into(),
            schedule: WatchSchedule::Every(10),
            for_minutes: Some(120),
            report_to: None,
            run_budget_usd: None,
            run_limit_secs: None,
            every_run: false,
        };
        assert_eq!(
            watch(&[
                "start",
                "after-release",
                "--skill",
                "pod-watch",
                "--every",
                "10",
                "--for",
                "120"
            ]),
            Ok(Mode::Tool(ToolCommand::Watch(expected.clone())))
        );
        assert_eq!(
            watch(&[
                "start",
                "after-release",
                "--for",
                "120",
                "--every",
                "10",
                "--skill",
                "pod-watch"
            ]),
            Ok(Mode::Tool(ToolCommand::Watch(expected)))
        );
        assert_eq!(
            watch(&[
                "start",
                "w",
                "--skill",
                "s",
                "--every",
                "5",
                "--for",
                "60",
                "--report-to",
                "chief",
                "--run-budget",
                "0.03",
                "--every-run",
            ]),
            Ok(Mode::Tool(ToolCommand::Watch(WatchCommand::Start {
                name: "w".into(),
                skill: "s".into(),
                schedule: WatchSchedule::Every(5),
                for_minutes: Some(60),
                report_to: Some("chief".into()),
                run_budget_usd: Some("0.03".into()),
                run_limit_secs: None,
                every_run: true,
            })))
        );
        assert_eq!(
            watch(&[
                "start",
                "w",
                "--skill",
                "s",
                "--every",
                "5",
                "--for",
                "60",
                "--run-limit",
                "300",
            ]),
            Ok(Mode::Tool(ToolCommand::Watch(WatchCommand::Start {
                name: "w".into(),
                skill: "s".into(),
                schedule: WatchSchedule::Every(5),
                for_minutes: Some(60),
                report_to: None,
                run_budget_usd: None,
                run_limit_secs: Some(300),
                every_run: false,
            })))
        );
        assert_eq!(
            watch(&["stop", "after-release"]),
            Ok(Mode::Tool(ToolCommand::Watch(WatchCommand::Stop {
                name: "after-release".into()
            })))
        );
        assert_eq!(
            watch(&["list"]),
            Ok(Mode::Tool(ToolCommand::Watch(WatchCommand::List)))
        );
    }

    #[test]
    fn tool_watch_refuses_a_malformed_command() {
        assert!(WATCH_USAGE_LINE.contains("[--run-limit SECONDS]"));
        for args in [
            vec![],
            vec!["start"],
            vec!["start", "w"],
            vec![
                "start", "w", "--skill", "s", "--every", "10", "--for", "5", "--bogus",
            ],
            vec![
                "start", "w", "--skill", "s", "--skill", "t", "--every", "10", "--for", "5",
            ],
            vec!["start", "w", "--skill", "s", "--every", "10", "--for"],
            vec!["stop"],
            vec!["list", "extra"],
        ] {
            assert_eq!(watch(&args), Err(WATCH_USAGE_LINE.into()), "{args:?}");
        }
        assert_eq!(
            watch(&["start", "w", "--skill", "s", "--every", "ten", "--for", "5"]),
            Err("invalid every_minutes".into())
        );
        assert_eq!(
            watch(&["start", "w", "--skill", "s", "--every", "10", "--for", "-5"]),
            Err("invalid for_minutes".into())
        );
        assert_eq!(
            watch(&[
                "start",
                "w",
                "--skill",
                "s",
                "--every",
                "10",
                "--for",
                "5",
                "--run-budget",
                "x"
            ]),
            Err("invalid run_budget_usd".into())
        );
        for limit in ["0", "-5", "x", "1.5", ""] {
            assert_eq!(
                watch(&[
                    "start",
                    "w",
                    "--skill",
                    "s",
                    "--every",
                    "10",
                    "--for",
                    "5",
                    "--run-limit",
                    limit
                ]),
                Err("invalid run_limit_secs".into()),
                "{limit:?}"
            );
        }
        assert_eq!(
            watch(&[
                "start",
                "w",
                "--skill",
                "s",
                "--every",
                "10",
                "--for",
                "5",
                "--run-limit",
                "60",
                "--run-limit",
                "90"
            ]),
            Err(WATCH_USAGE_LINE.into())
        );
        for budget in ["0", "-0.5"] {
            assert_eq!(
                watch(&[
                    "start",
                    "w",
                    "--skill",
                    "s",
                    "--every",
                    "10",
                    "--for",
                    "5",
                    "--run-budget",
                    budget
                ]),
                Err("invalid run_budget_usd".into()),
                "{budget}"
            );
        }
    }

    #[test]
    fn a_watch_refusal_names_what_was_refused() {
        let start = WatchCommand::Start {
            name: "w".into(),
            skill: "nope".into(),
            schedule: WatchSchedule::Every(10),
            for_minutes: Some(5),
            report_to: None,
            run_budget_usd: None,
            run_limit_secs: None,
            every_run: false,
        };
        let stop = WatchCommand::Stop { name: "w".into() };
        for (status, command, line) in [
            (
                tonic::Status::not_found("unknown_skill"),
                &start,
                "unknown skill nope",
            ),
            (
                tonic::Status::failed_precondition("too_many_watches"),
                &start,
                "too many watches",
            ),
            (
                tonic::Status::failed_precondition("daily_budget_spent"),
                &start,
                "daily budget spent",
            ),
            (
                tonic::Status::invalid_argument("invalid report_to"),
                &start,
                "invalid report_to",
            ),
            (
                tonic::Status::not_found("unknown_watch"),
                &stop,
                "unknown watch w",
            ),
            (
                tonic::Status::failed_precondition("not_running"),
                &stop,
                "w is not running",
            ),
            (
                tonic::Status::unimplemented(""),
                &stop,
                "the Harold daemon does not serve watches",
            ),
        ] {
            assert_eq!(watch_refusal_line(&status, command), line);
        }
    }

    #[test]
    fn a_watch_list_line_says_running_or_why_it_ended() {
        let utc = time::UtcOffset::UTC;
        let running = watch_api::Watch {
            name: "after-release".into(),
            folder: "/work/chief-of-pods".into(),
            running: true,
            ends_at_ms: Some(20 * 3_600_000 + 45 * 60_000),
            runs: 3,
            reports: 1,
            cost_usd: 0.03,
            run_limit_secs: 300,
            ..Default::default()
        };
        assert_eq!(
            watch_list_line(&running, utc, 0),
            "after-release running in /work/chief-of-pods until 20:45, limit 300 s, 3 runs, 1 report, $0.03"
        );
        let ended = watch_api::Watch {
            running: false,
            stop_reason: "daily_budget_spent".into(),
            runs: 1,
            reports: 2,
            ..running
        };
        assert_eq!(
            watch_list_line(&ended, utc, 0),
            "after-release ended (daily budget spent) in /work/chief-of-pods, 1 run, 2 reports, $0.03"
        );
    }

    fn watch_command(args: &[&str]) -> Result<WatchCommand, String> {
        match watch(args) {
            Ok(Mode::Tool(ToolCommand::Watch(command))) => Ok(command),
            Ok(other) => panic!("not a watch command: {other:?}"),
            Err(line) => Err(line),
        }
    }

    #[test]
    fn parse_minutes_takes_minutes_or_hours() {
        assert_eq!(parse_minutes("90", "for_minutes"), Ok(90));
        assert_eq!(parse_minutes("90m", "for_minutes"), Ok(90));
        assert_eq!(parse_minutes("2h", "for_minutes"), Ok(120));
        assert_eq!(parse_minutes("0", "for_minutes"), Ok(0));
        for bad in ["h", "2x", "", "-5", "1.5h", "m"] {
            assert_eq!(
                parse_minutes(bad, "for_minutes"),
                Err("invalid for_minutes".into()),
                "{bad:?}"
            );
        }
        assert_eq!(
            parse_minutes("x", "in_minutes"),
            Err("invalid in_minutes".into())
        );
    }

    #[test]
    fn tool_watch_start_may_leave_out_the_end_or_take_hours() {
        let start = |end: &[&str]| {
            let mut args = vec!["start", "w", "--skill", "s", "--every", "5"];
            args.extend(end);
            watch_command(&args)
        };
        let Ok(WatchCommand::Start { for_minutes, .. }) = start(&[]) else {
            panic!("start without --for must parse");
        };
        assert_eq!(for_minutes, None);
        let Ok(WatchCommand::Start { for_minutes, .. }) = start(&["--for", "2h"]) else {
            panic!("start --for 2h must parse");
        };
        assert_eq!(for_minutes, Some(120));
        let Ok(WatchCommand::Start { for_minutes, .. }) = start(&["--for", "90"]) else {
            panic!("start --for 90 must parse");
        };
        assert_eq!(for_minutes, Some(90));
        assert_eq!(start(&["--for", "soon"]), Err("invalid for_minutes".into()));
        assert!(WATCH_USAGE_LINE.contains("[--for TIME]"));
        assert!(WATCH_USAGE_LINE.contains("pause (NAME | --all) [--for TIME]"));
        assert!(WATCH_USAGE_LINE.contains("resume (NAME | --all)"));
        assert!(
            WATCH_USAGE_LINE.contains("remind NAME (--in TIME | --at HH:MM | --cron \"EXPR\")")
        );
        assert!(WATCH_USAGE_LINE.ends_with("[--report-to chief|self] [--] \"TEXT\""));
    }

    #[test]
    fn a_pause_or_resume_line_comes_from_the_response_not_the_request() {
        let names = |list: &[&str]| list.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        let paused = |done: &[&str]| watch_api::PauseResponse {
            paused: names(done),
            skipped: vec![],
        };
        assert_eq!(
            watch_pause_line(Some("pods"), None, &paused(&["pods"])),
            "paused pods"
        );
        assert_eq!(
            watch_pause_line(Some("pods"), Some(30), &paused(&["pods"])),
            "paused pods for 30 minutes"
        );
        // The daemon changed nothing: no claim that pods is paused.
        assert_eq!(
            watch_pause_line(Some("pods"), None, &paused(&[])),
            "paused 0"
        );
        assert_eq!(
            watch_pause_line(Some("pods"), Some(30), &paused(&[])),
            "paused 0"
        );
        assert_eq!(
            watch_pause_line(None, Some(30), &paused(&["a", "b"])),
            "paused 2 for 30 minutes"
        );
        assert_eq!(watch_pause_line(None, Some(30), &paused(&[])), "paused 0");

        let resumed = |done: &[&str]| watch_api::ResumeResponse {
            resumed: names(done),
            skipped: vec![],
        };
        assert_eq!(
            watch_resume_line(Some("pods"), &resumed(&["pods"])),
            "resumed pods"
        );
        assert_eq!(watch_resume_line(Some("pods"), &resumed(&[])), "resumed 0");
        assert_eq!(watch_resume_line(None, &resumed(&["a"])), "resumed 1");
    }

    #[test]
    fn tool_watch_pause_and_resume_parse() {
        assert_eq!(
            watch_command(&["pause", "pods", "--for", "2h"]),
            Ok(WatchCommand::Pause {
                name: Some("pods".into()),
                all: false,
                for_minutes: Some(120)
            })
        );
        assert_eq!(
            watch_command(&["pause", "pods"]),
            Ok(WatchCommand::Pause {
                name: Some("pods".into()),
                all: false,
                for_minutes: None
            })
        );
        assert_eq!(
            watch_command(&["pause", "--all"]),
            Ok(WatchCommand::Pause {
                name: None,
                all: true,
                for_minutes: None
            })
        );
        assert_eq!(
            watch_command(&["pause", "--for", "30m", "--all"]),
            Ok(WatchCommand::Pause {
                name: None,
                all: true,
                for_minutes: Some(30)
            })
        );
        assert_eq!(
            watch_command(&["resume", "pods"]),
            Ok(WatchCommand::Resume {
                name: Some("pods".into()),
                all: false
            })
        );
        assert_eq!(
            watch_command(&["resume", "--all"]),
            Ok(WatchCommand::Resume {
                name: None,
                all: true
            })
        );
        assert_eq!(
            watch_command(&["pause", "pods", "--for", "x"]),
            Err("invalid for_minutes".into())
        );
        for args in [
            vec!["pause"],
            vec!["pause", "pods", "--all"],
            vec!["pause", "--all", "pods"],
            vec!["pause", "pods", "--for"],
            vec!["pause", "pods", "--for", "1h", "--for", "2h"],
            vec!["pause", "--all", "--all"],
            vec!["pause", "pods", "--bogus"],
            vec!["pause", "a", "b"],
            vec!["resume"],
            vec!["resume", "pods", "--all"],
            vec!["resume", "pods", "--for", "1h"],
            vec!["resume", "--all", "--all"],
            vec!["resume", "a", "b"],
        ] {
            assert_eq!(watch(&args), Err(WATCH_USAGE_LINE.into()), "{args:?}");
        }
    }

    #[test]
    fn tool_watch_remind_parses() {
        assert_eq!(
            watch_command(&[
                "remind",
                "release-check",
                "--in",
                "45m",
                "--report-to",
                "chief",
                "check the v0.75.0 release"
            ]),
            Ok(WatchCommand::Remind {
                name: "release-check".into(),
                when: RemindWhen::In(45),
                report_to: Some("chief".into()),
                text: "check the v0.75.0 release".into()
            })
        );
        assert_eq!(
            watch_command(&["remind", "x", "--at", "16:30", "t"]),
            Ok(WatchCommand::Remind {
                name: "x".into(),
                when: RemindWhen::At("16:30".into()),
                report_to: None,
                text: "t".into()
            })
        );
        assert_eq!(
            watch_command(&["remind", "x", "t", "--in", "2h"]),
            Ok(WatchCommand::Remind {
                name: "x".into(),
                when: RemindWhen::In(120),
                report_to: None,
                text: "t".into()
            })
        );
        assert_eq!(
            watch_command(&["remind", "x", "--in", "soon", "t"]),
            Err("invalid in_minutes".into())
        );
        for args in [
            vec!["remind"],
            vec!["remind", "x"],
            vec!["remind", "x", "t"],
            vec!["remind", "x", "--in", "5m"],
            vec!["remind", "x", "--in", "5m", "--at", "16:30", "t"],
            vec!["remind", "x", "--in", "5m", "a", "b"],
            vec!["remind", "x", "--in", "5m", "--in", "6m", "t"],
            vec!["remind", "x", "--at", "16:30", "--at", "17:30", "t"],
            vec![
                "remind",
                "x",
                "--in",
                "5m",
                "--report-to",
                "chief",
                "--report-to",
                "self",
                "t",
            ],
            vec!["remind", "x", "--in", "5m", "--bogus", "t"],
            vec!["remind", "x", "--in"],
            vec!["remind", "x", "--in", "5m", "--report-to"],
        ] {
            assert_eq!(watch(&args), Err(WATCH_USAGE_LINE.into()), "{args:?}");
        }
    }

    #[test]
    fn tool_watch_start_takes_cron_in_place_of_every() {
        assert_eq!(
            watch_command(&[
                "start",
                "standup",
                "--skill",
                "pod-watch",
                "--cron",
                "*/15 9-17 * * mon-fri",
                "--report-to",
                "chief",
            ]),
            Ok(WatchCommand::Start {
                name: "standup".into(),
                skill: "pod-watch".into(),
                schedule: WatchSchedule::Cron("*/15 9-17 * * mon-fri".into()),
                for_minutes: None,
                report_to: Some("chief".into()),
                run_budget_usd: None,
                run_limit_secs: None,
                every_run: false,
            })
        );
        assert!(WATCH_USAGE_LINE.contains("(--every MINUTES | --cron \"EXPR\")"));
        for args in [
            // Both, neither, and a second --cron.
            vec![
                "start",
                "w",
                "--skill",
                "s",
                "--every",
                "10",
                "--cron",
                "0 9 * * *",
            ],
            vec!["start", "w", "--skill", "s", "--for", "5"],
            vec![
                "start",
                "w",
                "--skill",
                "s",
                "--cron",
                "0 9 * * *",
                "--cron",
                "0 10 * * *",
            ],
            vec!["start", "w", "--skill", "s", "--cron"],
        ] {
            assert_eq!(watch(&args), Err(WATCH_USAGE_LINE.into()), "{args:?}");
        }
    }

    #[test]
    fn tool_watch_remind_takes_cron_in_place_of_in_or_at() {
        assert_eq!(
            watch_command(&[
                "remind",
                "digest-check",
                "--cron",
                "30 8 * * mon",
                "read the weekly digest"
            ]),
            Ok(WatchCommand::Remind {
                name: "digest-check".into(),
                when: RemindWhen::Cron("30 8 * * mon".into()),
                report_to: None,
                text: "read the weekly digest".into()
            })
        );
        assert!(
            WATCH_USAGE_LINE.contains("remind NAME (--in TIME | --at HH:MM | --cron \"EXPR\")")
        );
        for args in [
            vec!["remind", "x", "--cron", "30 8 * * mon", "--in", "5m", "t"],
            vec![
                "remind",
                "x",
                "--at",
                "16:30",
                "--cron",
                "30 8 * * mon",
                "t",
            ],
            vec!["remind", "x", "--cron", "30 8 * * mon"],
        ] {
            assert_eq!(watch(&args), Err(WATCH_USAGE_LINE.into()), "{args:?}");
        }
    }

    #[test]
    fn the_remind_confirmation_shows_the_date_when_the_reminder_is_not_today() {
        let utc = time::UtcOffset::UTC;
        // 2026-11-03 10:00 UTC.
        let now_ms = 1_793_700_000_000 - 1_793_700_000_000 % 86_400_000 + 10 * 3_600_000;
        assert_eq!(watch_local_minute(now_ms, utc), "2026-11-03 10:00");
        assert_eq!(
            watch_remind_line("release-check", now_ms + 45 * 60_000, utc, now_ms),
            "reminder release-check at 10:45"
        );
        assert_eq!(
            watch_remind_line(
                "digest-check",
                now_ms + 6 * 86_400_000 - 90 * 60_000,
                utc,
                now_ms
            ),
            "reminder digest-check at 2026-11-09 08:30"
        );
    }

    #[test]
    fn a_cron_refusal_prints_the_daemon_sentence() {
        let start = WatchCommand::Start {
            name: "w".into(),
            skill: "s".into(),
            schedule: WatchSchedule::Cron("* * * * *".into()),
            for_minutes: Some(5),
            report_to: None,
            run_budget_usd: None,
            run_limit_secs: None,
            every_run: false,
        };
        for line in [
            "this schedule runs more often than every 2 minutes",
            "minute 75 is out of range: use 0 to 59",
            "give exactly one of every_minutes and cron",
        ] {
            assert_eq!(
                watch_refusal_line(&tonic::Status::invalid_argument(line), &start),
                line
            );
        }
    }

    #[test]
    fn a_watch_refusal_names_the_new_refusals() {
        let pause_one = WatchCommand::Pause {
            name: Some("pods".into()),
            all: false,
            for_minutes: None,
        };
        let resume_one = WatchCommand::Resume {
            name: Some("pods".into()),
            all: false,
        };
        let resume_all = WatchCommand::Resume {
            name: None,
            all: true,
        };
        let remind = WatchCommand::Remind {
            name: "release-check".into(),
            when: RemindWhen::In(5),
            report_to: None,
            text: "t".into(),
        };
        let start = WatchCommand::Start {
            name: "w".into(),
            skill: "s".into(),
            schedule: WatchSchedule::Every(10),
            for_minutes: None,
            report_to: None,
            run_budget_usd: None,
            run_limit_secs: None,
            every_run: false,
        };
        for (status, command, line) in [
            (
                tonic::Status::failed_precondition("daily_budget_unset"),
                &start,
                "daily budget not set",
            ),
            (
                tonic::Status::failed_precondition("not_paused"),
                &resume_one,
                "pods is not paused",
            ),
            (
                tonic::Status::failed_precondition("budget_day"),
                &resume_one,
                "pods is capped by the daily budget",
            ),
            (
                tonic::Status::failed_precondition("budget_day"),
                &pause_one,
                "pods is capped by the daily budget",
            ),
            (
                tonic::Status::failed_precondition("name_in_use"),
                &remind,
                "name release-check is in use",
            ),
            (
                tonic::Status::failed_precondition("too_many_reminders"),
                &remind,
                "20 reminders are waiting: stop one first",
            ),
            (
                tonic::Status::not_found("unknown_watch"),
                &pause_one,
                "unknown watch pods",
            ),
            (
                tonic::Status::failed_precondition("not_running"),
                &pause_one,
                "pods is not running",
            ),
            (
                tonic::Status::not_found("unknown_watch"),
                &resume_all,
                "unknown watch",
            ),
            (
                tonic::Status::invalid_argument("invalid at_local"),
                &remind,
                "invalid at_local",
            ),
        ] {
            assert_eq!(watch_refusal_line(&status, command), line);
        }
    }

    #[test]
    fn a_watch_list_line_covers_no_end_paused_and_capped() {
        let utc = time::UtcOffset::UTC;
        let at_1630 = 16 * 3_600_000 + 30 * 60_000;
        let open = watch_api::Watch {
            name: "pods".into(),
            folder: "/work/x".into(),
            running: true,
            ends_at_ms: None,
            runs: 3,
            reports: 1,
            cost_usd: 0.03,
            run_limit_secs: 300,
            ..Default::default()
        };
        assert_eq!(
            watch_list_line(&open, utc, 0),
            "pods running in /work/x no end, limit 300 s, 3 runs, 1 report, $0.03"
        );
        let paused = watch_api::Watch {
            running: false,
            paused: true,
            paused_until_ms: Some(at_1630),
            pause_how: "cli".into(),
            ..open.clone()
        };
        assert_eq!(
            watch_list_line(&paused, utc, 0),
            "pods paused until 16:30 in /work/x, 3 runs, 1 report, $0.03"
        );
        let held = watch_api::Watch {
            paused_until_ms: None,
            ..paused.clone()
        };
        assert_eq!(
            watch_list_line(&held, utc, 0),
            "pods paused in /work/x, 3 runs, 1 report, $0.03"
        );
        let capped = watch_api::Watch {
            paused_until_ms: Some(0),
            pause_how: "budget_day".into(),
            ..paused
        };
        assert_eq!(
            watch_list_line(&capped, utc, 0),
            "pods paused (budget) until 00:00 in /work/x, 3 runs, 1 report, $0.03"
        );
        let waiting = watch_api::Reminder {
            name: "release-check".into(),
            report_to: "chief".into(),
            due_at_ms: at_1630,
            cron: String::new(),
        };
        assert_eq!(
            watch_reminder_line(&waiting, utc, 0),
            "reminder release-check due 16:30, reporting to chief"
        );
    }

    /// The instant of `hour:minute:second` UTC on 2026-11-03, a Tuesday.
    fn on_3_november_utc(hour: u8, minute: u8, second: u8) -> i64 {
        time::Date::from_calendar_date(2026, time::Month::November, 3)
            .unwrap()
            .with_hms(hour, minute, second)
            .unwrap()
            .assume_utc()
            .unix_timestamp()
            * 1000
    }

    #[test]
    fn a_cron_watch_list_line_shows_its_cron_next_time_and_checked_minute() {
        let sydney = time::UtcOffset::from_hms(10, 0, 0).unwrap();
        // 14:31:20 in the local zone.
        let now = on_3_november_utc(4, 31, 20);
        let standup = watch_api::Watch {
            name: "standup".into(),
            folder: "/work/x".into(),
            running: true,
            ends_at_ms: None,
            runs: 3,
            reports: 1,
            cost_usd: 0.03,
            run_limit_secs: 300,
            cron: "*/15 9-17 * * mon-fri".into(),
            checked_local: "2026-11-03 14:31".into(),
            ..Default::default()
        };
        assert_eq!(
            watch_list_line(&standup, sydney, now),
            "standup running in /work/x no end, cron */15 9-17 * * mon-fri, next 14:45, \
             checked 14:31, limit 300 s, 3 runs, 1 report, $0.03"
        );
        // After 17:45 the next match is the next day: the date is shown.
        let evening = watch_api::Watch {
            checked_local: "2026-11-03 17:50".into(),
            ..standup.clone()
        };
        assert_eq!(
            watch_list_line(&evening, sydney, on_3_november_utc(7, 50, 0)),
            "standup running in /work/x no end, cron */15 9-17 * * mon-fri, \
             next 2026-11-04 09:00, checked 17:50, limit 300 s, 3 runs, 1 report, $0.03"
        );
        let paused = watch_api::Watch {
            running: false,
            paused: true,
            pause_how: "cli".into(),
            checked_local: "2026-11-02 23:59".into(),
            ..standup.clone()
        };
        assert_eq!(
            watch_list_line(&paused, sydney, now),
            "standup paused in /work/x, cron */15 9-17 * * mon-fri, next 14:45, \
             checked 2026-11-02 23:59, 3 runs, 1 report, $0.03"
        );
        // Not checked yet: no checked minute.
        let fresh = watch_api::Watch {
            checked_local: String::new(),
            ..standup.clone()
        };
        assert_eq!(
            watch_list_line(&fresh, sydney, now),
            "standup running in /work/x no end, cron */15 9-17 * * mon-fri, next 14:45, \
             limit 300 s, 3 runs, 1 report, $0.03"
        );
        let ended = watch_api::Watch {
            running: false,
            stop_reason: "stopped".into(),
            ..standup
        };
        assert_eq!(
            watch_list_line(&ended, sydney, now),
            "standup ended (stopped) in /work/x, 3 runs, 1 report, $0.03"
        );
    }

    #[test]
    fn a_cron_reminder_list_line_shows_its_cron_and_due_date() {
        let sydney = time::UtcOffset::from_hms(10, 0, 0).unwrap();
        let now = on_3_november_utc(4, 31, 20);
        // Monday 2026-11-09 08:30 in the local zone.
        let due = on_3_november_utc(0, 0, 0) + 6 * 86_400_000 - 10 * 3_600_000 + 510 * 60_000;
        let digest = watch_api::Reminder {
            name: "digest-check".into(),
            report_to: "chief".into(),
            due_at_ms: due,
            cron: "30 8 * * mon".into(),
        };
        assert_eq!(
            watch_reminder_line(&digest, sydney, now),
            "reminder digest-check due 2026-11-09 08:30, cron 30 8 * * mon, reporting to chief"
        );
        let today = watch_api::Reminder {
            due_at_ms: on_3_november_utc(5, 0, 0),
            cron: "0 15 * * *".into(),
            ..digest
        };
        assert_eq!(
            watch_reminder_line(&today, sydney, now),
            "reminder digest-check due 15:00, cron 0 15 * * *, reporting to chief"
        );
    }

    #[test]
    fn a_watch_count_line_names_the_capped_watches() {
        let names = |list: &[&str]| list.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        assert_eq!(
            watch_count_line("resumed", &names(&["a"]), &names(&["pods"])),
            "resumed 1, skipped pods (budget)"
        );
        assert_eq!(
            watch_count_line("paused", &names(&["a"]), &names(&["pods"])),
            "paused 1, skipped pods (budget)"
        );
        assert_eq!(
            watch_count_line("paused", &names(&["a", "b"]), &names(&["x", "y"])),
            "paused 2, skipped x, y (budget)"
        );
        assert_eq!(watch_count_line("resumed", &[], &[]), "resumed 0");
    }

    #[test]
    fn tool_watch_remind_takes_text_after_a_double_dash() {
        assert_eq!(
            watch_command(&["remind", "x", "--in", "5m", "--", "-check it"]),
            Ok(WatchCommand::Remind {
                name: "x".into(),
                when: RemindWhen::In(5),
                report_to: None,
                text: "-check it".into()
            })
        );
        for args in [
            vec!["remind", "x", "--in", "5m", "--"],
            vec!["remind", "x", "--in", "5m", "--", "a", "b"],
            vec!["remind", "x", "--in", "5m", "t", "--", "u"],
        ] {
            assert_eq!(watch(&args), Err(WATCH_USAGE_LINE.into()), "{args:?}");
        }
    }

    #[tokio::test]
    async fn tool_watch_pause_resume_and_remind_run_against_the_daemon() {
        let folder = TempFolder::new();
        let work = std::fs::canonicalize(&folder.0).unwrap();
        let skill = work.join(".claude/skills/pod-watch");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(skill.join("SKILL.md"), "Check.\n").unwrap();
        let (serving, _stop) = watch_server(&folder).await;
        let utc = time::UtcOffset::UTC;
        let run = |command: WatchCommand| {
            let (address, work) = (serving.address.clone(), work.clone());
            async move { watch_within(&address, &command, &work, utc, TEST_WAIT).await }
        };
        run(WatchCommand::Start {
            name: "pods".into(),
            skill: "pod-watch".into(),
            schedule: WatchSchedule::Every(10),
            for_minutes: Some(120),
            report_to: Some("chief".into()),
            run_budget_usd: None,
            run_limit_secs: None,
            every_run: false,
        })
        .await
        .unwrap();
        let pause_of = |name: Option<&str>, all, for_minutes| WatchCommand::Pause {
            name: name.map(Into::into),
            all,
            for_minutes,
        };
        let resume = |name: Option<&str>, all| WatchCommand::Resume {
            name: name.map(Into::into),
            all,
        };
        assert_eq!(
            run(pause_of(Some("pods"), false, None)).await,
            Ok("paused pods".into())
        );
        let listed = run(WatchCommand::List).await.unwrap();
        assert!(
            listed.starts_with(&format!("pods paused in {}, ", work.display())),
            "{listed}"
        );
        assert_eq!(
            run(resume(Some("pods"), false)).await,
            Ok("resumed pods".into())
        );
        assert_eq!(
            run(resume(Some("pods"), false)).await,
            Err("pods is not paused".into())
        );
        assert_eq!(
            run(pause_of(Some("pods"), false, Some(30))).await,
            Ok("paused pods for 30 minutes".into())
        );
        assert_eq!(run(resume(None, true)).await, Ok("resumed 1".into()));
        assert_eq!(run(resume(None, true)).await, Ok("resumed 0".into()));
        assert_eq!(run(pause_of(None, true, None)).await, Ok("paused 1".into()));

        let reminder = run(WatchCommand::Remind {
            name: "release-check".into(),
            when: RemindWhen::In(45),
            report_to: Some("chief".into()),
            text: "check".into(),
        })
        .await
        .unwrap();
        assert!(
            reminder.starts_with("reminder release-check at "),
            "{reminder}"
        );
        let listed = run(WatchCommand::List).await.unwrap();
        let lines: Vec<&str> = listed.lines().collect();
        assert!(
            lines[lines.len() - 2].starts_with("reminder release-check due "),
            "{listed}"
        );
        assert!(
            lines[lines.len() - 2].ends_with(", reporting to chief"),
            "{listed}"
        );
        assert!(lines[lines.len() - 1].starts_with("spent today $"));
    }

    /// The watch tool over a temp folder. Claude Code is `/usr/bin/true`, so a run that
    /// starts fails without reaching a model.
    async fn watch_server(folder: &TempFolder) -> (Serving, tokio::sync::watch::Sender<()>) {
        let (stop, shutdown) = tokio::sync::watch::channel(());
        let handle = crate::tools::watch::start(crate::tools::ToolContext {
            data_folder: folder.0.join("data"),
            typing: Some(std::sync::Arc::new(Typist)),
            chief_pane: std::sync::Arc::new(NoChief),
            pane_folder: std::sync::Arc::new(NoFolder),
            products: std::sync::Arc::new(NoProducts),
            agents: std::sync::Arc::new(crate::tools::tests::NoAgents),
            pods: std::sync::Arc::new(crate::tools::tests::NoPods),
            watch_settings: Default::default(),
            model_environment: Vec::new(),
            cli_path: "/usr/bin/true".into(),
            grpc_addr: None,
            watch_counts: std::sync::Arc::new(
                tokio::sync::watch::channel(crate::tools::WatchCounts::default()).0,
            ),
            pod_registry: None,
            shutdown,
        })
        .await
        .unwrap_or_else(|e| panic!("start failed: {}", e.reason));
        (serve(handle.routes).await, stop)
    }

    // -- harold tool release ------------------------------------------------------

    fn release_args(args: &[&str]) -> Result<Mode, String> {
        let mut all = vec!["tool", "release"];
        all.extend_from_slice(args);
        parse(&all)
    }

    fn release_command(product: Option<&str>, json: bool) -> Result<Mode, String> {
        Ok(Mode::Tool(ToolCommand::Release(ReleaseCommand::Next {
            product: product.map(Into::into),
            json,
        })))
    }

    #[test]
    fn tool_release_next_parses_its_arguments() {
        assert_eq!(release_args(&["next"]), release_command(None, false));
        assert_eq!(
            release_args(&["next", "harold"]),
            release_command(Some("harold"), false)
        );
        assert_eq!(
            release_args(&["next", "--json"]),
            release_command(None, true)
        );
        assert_eq!(
            release_args(&["next", "harold", "--json"]),
            release_command(Some("harold"), true)
        );
        assert_eq!(
            release_args(&["next", "--json", "harold"]),
            release_command(Some("harold"), true)
        );
    }

    #[test]
    fn tool_release_next_refuses_a_malformed_command() {
        for args in [
            &["next", "--bad"][..],
            &["next", "a", "b"],
            &["next", "--json", "--json"],
            &["later"],
            &[],
        ] {
            assert_eq!(
                release_args(args),
                Err(RELEASE_USAGE_LINE.into()),
                "{args:?}"
            );
        }
    }

    /// Runs a git command in `repo`, with no network and a fixed identity.
    fn git(repo: &std::path::Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?}");
    }

    /// The release tool over a temp registry. The product `alpha` is a real tiny git repo
    /// with the tag `v0.1.0` on `origin/main`.
    async fn release_server(folder: &TempFolder) -> (Serving, tokio::sync::watch::Sender<()>) {
        let repo = folder.0.join("alpha");
        std::fs::create_dir_all(repo.join(".superpowers/pod/specs")).unwrap();
        git(&repo, &["init", "--quiet", "--initial-branch=main"]);
        git(
            &repo,
            &["commit", "--quiet", "--allow-empty", "-m", "first"],
        );
        git(&repo, &["tag", "v0.1.0"]);
        git(&repo, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        let registry = folder.0.join("products.toml");
        std::fs::write(
            &registry,
            format!(
                "[products.alpha]\nrepo = \"{}\"\nbase = \"main\"\n",
                repo.display()
            ),
        )
        .unwrap();
        let (stop, shutdown) = tokio::sync::watch::channel(());
        let handle = crate::tools::release::start(crate::tools::ToolContext {
            data_folder: folder.0.join("data"),
            typing: Some(std::sync::Arc::new(Typist)),
            chief_pane: std::sync::Arc::new(NoChief),
            pane_folder: std::sync::Arc::new(NoFolder),
            products: std::sync::Arc::new(NoProducts),
            agents: std::sync::Arc::new(crate::tools::tests::NoAgents),
            pods: std::sync::Arc::new(crate::tools::tests::NoPods),
            watch_settings: Default::default(),
            model_environment: Vec::new(),
            grpc_addr: None,
            watch_counts: std::sync::Arc::new(
                tokio::sync::watch::channel(crate::tools::WatchCounts::default()).0,
            ),
            cli_path: "/usr/bin/true".into(),
            pod_registry: Some(registry),
            shutdown,
        })
        .await
        .unwrap_or_else(|e| panic!("start failed: {}", e.reason));
        (serve(handle.routes).await, stop)
    }

    #[tokio::test]
    async fn tool_release_next_runs_against_the_daemon() {
        let folder = TempFolder::new();
        let (serving, _stop) = release_server(&folder).await;
        let run = |product: Option<&str>, json: bool| {
            let address = serving.address.clone();
            let command = ReleaseCommand::Next {
                product: product.map(Into::into),
                json,
            };
            async move { release_within(&address, &command, TEST_WAIT).await }
        };

        let text = run(None, false).await.unwrap();
        assert!(text.starts_with("alpha\n"), "{text}");
        assert!(text.contains("v0.1.0"), "{text}");

        let json = run(Some("alpha"), true).await.unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["products"][0]["name"], "alpha", "{json}");
        assert_eq!(value["products"][0]["release"], "v0.1.0", "{json}");

        assert_eq!(
            run(Some("nope"), false).await,
            Err("unknown product nope".into())
        );
    }

    #[tokio::test]
    async fn tool_release_next_says_when_the_registry_cannot_be_read() {
        let folder = TempFolder::new();
        let (stop, shutdown) = tokio::sync::watch::channel(());
        let handle = crate::tools::release::start(crate::tools::ToolContext {
            data_folder: folder.0.join("data"),
            typing: Some(std::sync::Arc::new(Typist)),
            chief_pane: std::sync::Arc::new(NoChief),
            pane_folder: std::sync::Arc::new(NoFolder),
            products: std::sync::Arc::new(NoProducts),
            agents: std::sync::Arc::new(crate::tools::tests::NoAgents),
            pods: std::sync::Arc::new(crate::tools::tests::NoPods),
            watch_settings: Default::default(),
            model_environment: Vec::new(),
            cli_path: "/usr/bin/true".into(),
            pod_registry: None,
            grpc_addr: None,
            watch_counts: std::sync::Arc::new(
                tokio::sync::watch::channel(crate::tools::WatchCounts::default()).0,
            ),
            shutdown,
        })
        .await
        .unwrap_or_else(|e| panic!("start failed: {}", e.reason));
        let serving = serve(handle.routes).await;
        let command = ReleaseCommand::Next {
            product: None,
            json: false,
        };
        assert_eq!(
            release_within(&serving.address, &command, TEST_WAIT).await,
            Err("the product registry cannot be read".into())
        );
        drop(stop);
    }

    #[tokio::test]
    async fn tool_watch_start_list_and_stop_run_against_the_daemon() {
        let folder = TempFolder::new();
        let work = std::fs::canonicalize(&folder.0).unwrap();
        let skill = work.join(".claude/skills/pod-watch");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(skill.join("SKILL.md"), "Check.\n").unwrap();
        let (serving, _stop) = watch_server(&folder).await;
        let utc = time::UtcOffset::UTC;
        let run = |command: WatchCommand| {
            let (address, work) = (serving.address.clone(), work.clone());
            async move { watch_within(&address, &command, &work, utc, TEST_WAIT).await }
        };
        let start = |skill: &str| WatchCommand::Start {
            name: "after-release".into(),
            skill: skill.into(),
            schedule: WatchSchedule::Every(10),
            for_minutes: Some(120),
            report_to: Some("chief".into()),
            run_budget_usd: None,
            run_limit_secs: None,
            every_run: false,
        };

        let started = run(start("pod-watch")).await.unwrap();
        assert!(
            started.starts_with(&format!(
                "started after-release in {} until ",
                work.display()
            )) && started.ends_with(", every 10 minutes, reporting to chief"),
            "{started}"
        );
        assert_eq!(run(start("nope")).await, Err("unknown skill nope".into()));

        let listed = run(WatchCommand::List).await.unwrap();
        let lines: Vec<&str> = listed.lines().collect();
        assert_eq!(lines.len(), 2, "{listed}");
        assert!(
            lines[0].starts_with(&format!(
                "after-release running in {} until ",
                work.display()
            )),
            "{listed}"
        );
        assert!(lines[1].starts_with("spent today $"), "{listed}");

        let stop = WatchCommand::Stop {
            name: "after-release".into(),
        };
        assert_eq!(run(stop.clone()).await, Ok("stopped after-release".into()));
        assert_eq!(run(stop).await, Err("after-release is not running".into()));
        assert_eq!(
            run(WatchCommand::Stop {
                name: "nope".into()
            })
            .await,
            Err("unknown watch nope".into())
        );

        let cron = |expr: &str| WatchCommand::Start {
            name: "standup".into(),
            skill: "pod-watch".into(),
            schedule: WatchSchedule::Cron(expr.into()),
            for_minutes: Some(120),
            report_to: Some("chief".into()),
            run_budget_usd: None,
            run_limit_secs: None,
            every_run: false,
        };
        let started = run(cron("*/15 9-17 * * mon-fri")).await.unwrap();
        assert!(
            started.ends_with(", on cron */15 9-17 * * mon-fri, reporting to chief"),
            "{started}"
        );
        assert_eq!(
            run(cron("* * * * *")).await,
            Err("this schedule runs more often than every 2 minutes".into())
        );
        assert_eq!(
            run(cron("0 9 * *")).await,
            Err(
                "a cron expression has 5 fields (minute hour day month weekday); this one has 4"
                    .into()
            )
        );
    }

    // -----------------------------------------------------------------------
    // harold tool tts
    // -----------------------------------------------------------------------

    #[test]
    fn tts_commands_parse_and_a_malformed_one_prints_the_usage() {
        let revision = "a".repeat(40);
        assert_eq!(parse_tts(&["models"]), Ok(TtsCommand::Models));
        assert_eq!(
            parse_tts(&["add", &format!("org/name@{revision}")]),
            Ok(TtsCommand::Add {
                repo: "org/name".into(),
                revision: revision.clone()
            })
        );
        assert_eq!(
            parse_tts(&["remove", "org/name"]),
            Ok(TtsCommand::Remove {
                id: "org/name".into()
            })
        );
        assert_eq!(
            parse_tts(&["default", "org/name", "af_heart"]),
            Ok(TtsCommand::Default {
                model: "org/name".into(),
                voice: "af_heart".into()
            })
        );
        assert_eq!(
            parse_tts(&["speak", "All", "green."]),
            Ok(TtsCommand::Speak {
                text: "All green.".into()
            })
        );
        for bad in [
            vec![],
            vec!["models", "extra"],
            vec!["add"],
            vec!["add", "no-revision"],
            vec!["add", "@abc"],
            vec!["add", "org/name@"],
            vec!["remove"],
            vec!["default", "only-a-model"],
            vec!["speak"],
            vec!["nonsense"],
        ] {
            assert_eq!(parse_tts(&bad), Err(TTS_USAGE_LINE.to_string()), "{bad:?}");
        }
        assert_eq!(
            Mode::parse(&["tool".into(), "tts".into(), "models".into()]),
            Ok(Mode::Tool(ToolCommand::Tts(TtsCommand::Models)))
        );
    }

    #[test]
    fn each_tts_refusal_prints_one_plain_line() {
        let line = |code, message: &str| tts_refusal_line(&tonic::Status::new(code, message));
        assert_eq!(
            line(tonic::Code::PermissionDenied, "not_approved"),
            "waiting for approval: the model is not on the approved list (tts-models.toml); the chief has it audited"
        );
        assert_eq!(
            line(tonic::Code::PermissionDenied, "not_local"),
            "refused: the call is not from this machine"
        );
        assert_eq!(
            line(tonic::Code::InvalidArgument, "invalid_revision"),
            "invalid revision: use 40 lowercase hex characters"
        );
        assert_eq!(
            line(tonic::Code::FailedPrecondition, "unsafe_format"),
            "refused: the model has a weight file that can run code (.bin, .pt, .pth or .pkl)"
        );
        assert_eq!(
            line(tonic::Code::FailedPrecondition, "voice_muted"),
            "the voice is muted: nothing was said (unmute it in the control-tower)"
        );
        assert_eq!(
            line(tonic::Code::FailedPrecondition, "default_model"),
            "the default model cannot be removed"
        );
        assert_eq!(
            line(tonic::Code::Unavailable, "model_downloading"),
            "the TTS model is still downloading: try again soon"
        );
        assert_eq!(
            line(tonic::Code::Unimplemented, ""),
            "the Harold daemon does not serve the TTS server"
        );
        assert_eq!(
            line(tonic::Code::Internal, ""),
            "the Harold daemon refused the call"
        );
    }

    #[test]
    fn a_model_line_shows_the_default_mark_state_reason_worker_size_and_voices() {
        let model = tts_api::Model {
            id: "mlx-community/Kokoro-82M-bf16".into(),
            source: "huggingface".into(),
            revision: "a".repeat(40),
            worker: "mlx".into(),
            voices: vec!["af_heart".into(), "am_adam".into()],
            state: tts_api::ModelState::Ready.into(),
            reason: String::new(),
            size_bytes: 389_410_807,
            is_default: true,
            default_voice: "af_heart".into(),
        };
        assert_eq!(
            tts_model_line(&model),
            format!(
                "* mlx-community/Kokoro-82M-bf16 ready mlx huggingface@{} 389 MB, default voice af_heart, voices af_heart am_adam",
                "a".repeat(40)
            )
        );
        let waiting = tts_api::Model {
            is_default: false,
            default_voice: String::new(),
            voices: Vec::new(),
            state: tts_api::ModelState::WaitingForApproval.into(),
            reason: "not_approved".into(),
            size_bytes: 0,
            ..model
        };
        assert!(tts_model_line(&waiting).starts_with(
            "  mlx-community/Kokoro-82M-bf16 waiting_for_approval (not_approved) mlx"
        ));
    }

    mod tts_server {
        use super::*;
        use harold_api::tts::v1::tts_service_server::{TtsService, TtsServiceServer};
        use std::sync::{Arc, Mutex};
        use tokio_stream::wrappers::ReceiverStream;

        /// A TTS service that notes each call and answers as the test set it.
        #[derive(Default)]
        struct Fake {
            calls: Arc<Mutex<Vec<String>>>,
            refuse: Option<tonic::Status>,
        }

        #[tonic::async_trait]
        impl TtsService for Fake {
            type SpeakStream = ReceiverStream<Result<tts_api::AudioChunk, tonic::Status>>;

            async fn speak(
                &self,
                request: tonic::Request<tts_api::SpeakRequest>,
            ) -> Result<tonic::Response<Self::SpeakStream>, tonic::Status> {
                if let Some(status) = &self.refuse {
                    return Err(status.clone());
                }
                let request = request.into_inner();
                self.calls
                    .lock()
                    .unwrap()
                    .push(format!("speak {}", request.text));
                let (sender, receiver) = tokio::sync::mpsc::channel(4);
                sender
                    .send(Ok(tts_api::AudioChunk {
                        sample_rate: 24_000,
                        pcm: vec![1, 0],
                    }))
                    .await
                    .unwrap();
                sender
                    .send(Ok(tts_api::AudioChunk {
                        sample_rate: 0,
                        pcm: vec![2, 0],
                    }))
                    .await
                    .unwrap();
                Ok(tonic::Response::new(ReceiverStream::new(receiver)))
            }

            async fn list_models(
                &self,
                _: tonic::Request<tts_api::ListModelsRequest>,
            ) -> Result<tonic::Response<tts_api::ListModelsResponse>, tonic::Status> {
                if let Some(status) = &self.refuse {
                    return Err(status.clone());
                }
                Ok(tonic::Response::new(tts_api::ListModelsResponse {
                    models: vec![tts_api::Model {
                        id: "org/kokoro".into(),
                        source: "huggingface".into(),
                        revision: "b".repeat(40),
                        worker: "mlx".into(),
                        state: tts_api::ModelState::Ready.into(),
                        size_bytes: 5_000_000,
                        is_default: true,
                        default_voice: "af_heart".into(),
                        ..Default::default()
                    }],
                }))
            }

            async fn add_model(
                &self,
                request: tonic::Request<tts_api::AddModelRequest>,
            ) -> Result<tonic::Response<tts_api::AddModelResponse>, tonic::Status> {
                if let Some(status) = &self.refuse {
                    return Err(status.clone());
                }
                let request = request.into_inner();
                self.calls.lock().unwrap().push(format!(
                    "add {} {}@{}",
                    request.source, request.repo, request.revision
                ));
                Ok(tonic::Response::new(tts_api::AddModelResponse {
                    id: request.repo,
                    state: tts_api::ModelState::Downloading.into(),
                }))
            }

            async fn remove_model(
                &self,
                request: tonic::Request<tts_api::RemoveModelRequest>,
            ) -> Result<tonic::Response<tts_api::RemoveModelResponse>, tonic::Status> {
                if let Some(status) = &self.refuse {
                    return Err(status.clone());
                }
                self.calls
                    .lock()
                    .unwrap()
                    .push(format!("remove {}", request.into_inner().id));
                Ok(tonic::Response::new(tts_api::RemoveModelResponse {}))
            }

            async fn set_default(
                &self,
                request: tonic::Request<tts_api::SetDefaultRequest>,
            ) -> Result<tonic::Response<tts_api::SetDefaultResponse>, tonic::Status> {
                let request = request.into_inner();
                self.calls
                    .lock()
                    .unwrap()
                    .push(format!("default {} {}", request.model, request.voice));
                Ok(tonic::Response::new(tts_api::SetDefaultResponse {}))
            }
        }

        async fn serve(fake: Fake) -> String {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap().to_string();
            let (incoming, connections) = tokio::sync::mpsc::channel(1);
            tokio::spawn(async move {
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
            tokio::spawn(
                tonic::transport::Server::builder()
                    .add_service(TtsServiceServer::new(fake))
                    .serve_with_incoming(tokio_stream::wrappers::ReceiverStream::new(connections)),
            );
            address
        }

        async fn run(address: &str, command: TtsCommand, player: &str) -> Result<String, String> {
            tts_within(address, &command, Duration::from_secs(20), player).await
        }

        #[tokio::test]
        async fn models_add_remove_and_default_print_one_line_each() {
            let fake = Fake::default();
            let calls = fake.calls.clone();
            let address = serve(fake).await;

            assert_eq!(
                run(&address, TtsCommand::Models, "/usr/bin/true").await,
                Ok(format!(
                    "* org/kokoro ready mlx huggingface@{} 5 MB, default voice af_heart",
                    "b".repeat(40)
                ))
            );
            assert_eq!(
                run(
                    &address,
                    TtsCommand::Add {
                        repo: "org/other".into(),
                        revision: "c".repeat(40)
                    },
                    "/usr/bin/true"
                )
                .await,
                Ok("org/other downloading".to_string())
            );
            assert_eq!(
                run(
                    &address,
                    TtsCommand::Remove {
                        id: "org/other".into()
                    },
                    "/usr/bin/true"
                )
                .await,
                Ok("removed org/other".to_string())
            );
            assert_eq!(
                run(
                    &address,
                    TtsCommand::Default {
                        model: "org/kokoro".into(),
                        voice: "am_adam".into()
                    },
                    "/usr/bin/true"
                )
                .await,
                Ok("default is org/kokoro am_adam".to_string())
            );
            assert_eq!(
                *calls.lock().unwrap(),
                [
                    format!("add huggingface org/other@{}", "c".repeat(40)),
                    "remove org/other".to_string(),
                    "default org/kokoro am_adam".to_string(),
                ]
            );
        }

        #[tokio::test]
        async fn a_refusal_prints_its_line() {
            let fake = Fake {
                refuse: Some(tonic::Status::permission_denied("not_approved")),
                ..Fake::default()
            };
            let address = serve(fake).await;

            let refused = run(
                &address,
                TtsCommand::Add {
                    repo: "org/other".into(),
                    revision: "c".repeat(40),
                },
                "/usr/bin/true",
            )
            .await;

            assert_eq!(
                refused,
                Err("waiting for approval: the model is not on the approved list (tts-models.toml); the chief has it audited".to_string())
            );
        }

        #[tokio::test]
        async fn no_daemon_prints_that_it_is_not_reachable() {
            assert_eq!(
                run("127.0.0.1:1", TtsCommand::Models, "/usr/bin/true").await,
                Err("the Harold daemon is not reachable".to_string())
            );
        }

        #[tokio::test]
        async fn speak_plays_the_audio_it_gets_with_the_player() {
            let folder =
                std::env::temp_dir().join(format!("harold-cli-tts-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&folder).unwrap();
            let player = folder.join("player");
            crate::test_support::fake_program::write_fake_program(
                &player,
                crate::test_support::fake_program::Language::Shell,
                "cp \"$1\" \"$0.wav\"",
            );
            let fake = Fake::default();
            let calls = fake.calls.clone();
            let address = serve(fake).await;

            let said = run(
                &address,
                TtsCommand::Speak {
                    text: "All green.".into(),
                },
                &player.display().to_string(),
            )
            .await;

            assert_eq!(said, Ok("said".to_string()));
            assert_eq!(*calls.lock().unwrap(), ["speak All green."]);
            let wav = std::fs::read(folder.join("player.wav")).unwrap();
            assert_eq!(&wav[0..4], b"RIFF");
            assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 24_000);
            assert_eq!(&wav[44..], [1, 0, 2, 0]);
            std::fs::remove_dir_all(&folder).unwrap();
        }

        #[tokio::test]
        async fn speak_fails_with_the_players_name_when_it_fails() {
            let address = serve(Fake::default()).await;

            let said = run(
                &address,
                TtsCommand::Speak { text: "Hi.".into() },
                "/usr/bin/false",
            )
            .await;

            assert_eq!(said, Err("/usr/bin/false failed".to_string()));
        }
    }

    mod get_command {
        use super::*;
        use crate::harold::harold_server::{Harold, HaroldServer};
        use crate::harold::{RexHostIcon, *};
        use tokio_stream::wrappers::ReceiverStream;

        const UNKNOWN: &str = "unknown thing \"bogus\": voice, messaging, version, machine, multiplexer, hosts, chief";

        fn state() -> GetHaroldStateResponse {
            GetHaroldStateResponse {
                tts_muted: Some(true),
                messaging_paused: Some(false),
                release_version: "0.72.0".into(),
                machine_name: "laptop-mac".into(),
                multiplexer: "rex".into(),
                rex_hosts: vec![RexHostIcon {
                    label: "omarchy".into(),
                    host_icon: "arch".into(),
                }],
                machine_host_icon: "mac".into(),
                tts_engine: "server".into(),
                chief_session: "chief".into(),
                ..Default::default()
            }
        }

        #[test]
        fn get_voice_is_off_when_the_voice_mute_is_on() {
            let mut state = state();
            assert_eq!(
                render_get(GetThing::Voice, &state),
                r#"{"state":"off","engine":"server"}"#
            );
            state.tts_muted = Some(false);
            assert_eq!(
                render_get(GetThing::Voice, &state),
                r#"{"state":"on","engine":"server"}"#
            );
            state.tts_muted = None;
            assert_eq!(
                render_get(GetThing::Voice, &state),
                r#"{"state":"on","engine":"server"}"#
            );
        }

        #[test]
        fn get_messaging_is_off_when_the_messaging_pause_is_on() {
            let mut state = state();
            assert_eq!(render_get(GetThing::Messaging, &state), r#"{"state":"on"}"#);
            state.messaging_paused = Some(true);
            assert_eq!(
                render_get(GetThing::Messaging, &state),
                r#"{"state":"off"}"#
            );
            state.messaging_paused = None;
            assert_eq!(render_get(GetThing::Messaging, &state), r#"{"state":"on"}"#);
        }

        #[test]
        fn get_prints_each_other_thing() {
            let mut state = state();
            for (thing, json) in [
                (GetThing::Version, r#"{"release_version":"0.72.0"}"#),
                (
                    GetThing::Machine,
                    r#"{"machine_name":"laptop-mac","host_icon":"mac"}"#,
                ),
                (GetThing::Multiplexer, r#"{"multiplexer":"rex"}"#),
                (
                    GetThing::Hosts,
                    r#"{"hosts":[{"label":"omarchy","host_icon":"arch"}]}"#,
                ),
                (GetThing::Chief, r#"{"chief_session":"chief"}"#),
            ] {
                assert_eq!(render_get(thing, &state), json);
            }
            state.rex_hosts.clear();
            assert_eq!(render_get(GetThing::Hosts, &state), r#"{"hosts":[]}"#);
        }

        #[test]
        fn get_parses_a_thing_and_refuses_the_rest() {
            assert_eq!(parse(&["get", "voice"]), Ok(Mode::Get(GetThing::Voice)));
            assert_eq!(parse(&["get", "chief"]), Ok(Mode::Get(GetThing::Chief)));
            assert_eq!(
                parse(&["get"]),
                Err("unknown thing \"\": voice, messaging, version, machine, multiplexer, hosts, chief".into())
            );
            assert_eq!(parse(&["get", "bogus"]), Err(UNKNOWN.into()));
            assert_eq!(
                parse(&["get", "voice", "x"]),
                Err("unknown thing \"voice x\": voice, messaging, version, machine, multiplexer, hosts, chief".into())
            );
        }

        /// A daemon stand-in that answers `GetHaroldState` with `answer`.
        struct Fake {
            answer: Result<GetHaroldStateResponse, tonic::Status>,
        }

        #[tonic::async_trait]
        impl Harold for Fake {
            type WatchAgentStatesStream = ReceiverStream<Result<AgentStateSnapshot, tonic::Status>>;

            async fn get_harold_state(
                &self,
                _: tonic::Request<GetHaroldStateRequest>,
            ) -> Result<tonic::Response<GetHaroldStateResponse>, tonic::Status> {
                self.answer.clone().map(tonic::Response::new)
            }
            async fn turn_complete(
                &self,
                _: tonic::Request<TurnCompleteRequest>,
            ) -> Result<tonic::Response<TurnCompleteResponse>, tonic::Status> {
                Err(tonic::Status::unimplemented(""))
            }
            async fn report_agent_state(
                &self,
                _: tonic::Request<ReportAgentStateRequest>,
            ) -> Result<tonic::Response<ReportAgentStateResponse>, tonic::Status> {
                Err(tonic::Status::unimplemented(""))
            }
            async fn watch_agent_states(
                &self,
                _: tonic::Request<WatchAgentStatesRequest>,
            ) -> Result<tonic::Response<Self::WatchAgentStatesStream>, tonic::Status> {
                Err(tonic::Status::unimplemented(""))
            }
            async fn set_tts_muted(
                &self,
                _: tonic::Request<SetTtsMutedRequest>,
            ) -> Result<tonic::Response<SetTtsMutedResponse>, tonic::Status> {
                Err(tonic::Status::unimplemented(""))
            }
            async fn get_remote_order(
                &self,
                _: tonic::Request<harold_api::harold::GetRemoteOrderRequest>,
            ) -> Result<tonic::Response<harold_api::harold::GetRemoteOrderResponse>, tonic::Status>
            {
                Ok(tonic::Response::new(Default::default()))
            }

            async fn set_remote_order(
                &self,
                _: tonic::Request<harold_api::harold::SetRemoteOrderRequest>,
            ) -> Result<tonic::Response<harold_api::harold::SetRemoteOrderResponse>, tonic::Status>
            {
                Ok(tonic::Response::new(Default::default()))
            }

            async fn agent_seen(
                &self,
                _: tonic::Request<harold_api::harold::AgentSeenRequest>,
            ) -> Result<tonic::Response<harold_api::harold::AgentSeenResponse>, tonic::Status>
            {
                Ok(tonic::Response::new(Default::default()))
            }

            async fn set_messaging_paused(
                &self,
                _: tonic::Request<SetMessagingPausedRequest>,
            ) -> Result<tonic::Response<SetMessagingPausedResponse>, tonic::Status> {
                Err(tonic::Status::unimplemented(""))
            }
            async fn send_message_to_away_device(
                &self,
                _: tonic::Request<SendMessageToAwayDeviceRequest>,
            ) -> Result<tonic::Response<SendMessageToAwayDeviceResponse>, tonic::Status>
            {
                Err(tonic::Status::unimplemented(""))
            }
            async fn speak_line(
                &self,
                _: tonic::Request<SpeakLineRequest>,
            ) -> Result<tonic::Response<SpeakLineResponse>, tonic::Status> {
                Err(tonic::Status::unimplemented(""))
            }
            async fn call(
                &self,
                _: tonic::Request<CallRequest>,
            ) -> Result<tonic::Response<CallResponse>, tonic::Status> {
                Err(tonic::Status::unimplemented(""))
            }
        }

        async fn serve(answer: Result<GetHaroldStateResponse, tonic::Status>) -> String {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap().to_string();
            let (incoming, connections) = tokio::sync::mpsc::channel(1);
            tokio::spawn(async move {
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
            tokio::spawn(
                tonic::transport::Server::builder()
                    .add_service(HaroldServer::new(Fake { answer }))
                    .serve_with_incoming(ReceiverStream::new(connections)),
            );
            address
        }

        fn failure(line: &str) -> GetFailure {
            GetFailure {
                line: line.into(),
                exit_code: 1,
            }
        }

        #[tokio::test]
        async fn get_prints_the_thing_a_daemon_answers() {
            let address = serve(Ok(state())).await;
            let result = tokio::time::timeout(
                GUARD,
                get_within(&settings_for(&address), GetThing::Voice, CALL_DEADLINE),
            )
            .await
            .expect("get waited past its deadline");
            assert_eq!(result, Ok(r#"{"state":"off","engine":"server"}"#.into()));
        }

        #[tokio::test]
        async fn get_gives_up_on_a_daemon_that_never_answers() {
            let (hung, stand_in) = never_answers().await;
            let wait = Duration::from_millis(100);
            let result = tokio::time::timeout(
                GUARD,
                get_within(&settings_for(&hung), GetThing::Voice, wait),
            )
            .await
            .expect("get waited past its deadline");
            stand_in.abort();
            assert_eq!(result, Err(failure(NOT_ANSWERING)));
        }

        #[tokio::test]
        async fn get_names_a_daemon_that_is_down() {
            let result = tokio::time::timeout(
                GUARD,
                get_within(
                    &settings_for(CLOSED),
                    GetThing::Voice,
                    Duration::from_secs(3600),
                ),
            )
            .await
            .expect("get waited past its deadline");
            assert_eq!(result, Err(failure(NOT_REACHABLE)));
        }

        #[tokio::test]
        async fn get_names_a_daemon_that_is_too_old() {
            let address = serve(Err(tonic::Status::unimplemented(""))).await;
            let result = tokio::time::timeout(
                GUARD,
                get_within(&settings_for(&address), GetThing::Voice, CALL_DEADLINE),
            )
            .await
            .expect("get waited past its deadline");
            assert_eq!(
                result,
                Err(failure("this Harold is too old for harold get"))
            );
        }
    }

    // ---- The work tool: `harold tool work` ----
    mod work_cli {
        use super::*;
        use harold_api::tools::work::v1 as work_api;

        const USAGE: &str = "invalid argument: usage is harold tool work list|show|put|set|decide|mark|link|unlink|check|import|rotate|history|search";

        fn work_parse(words: &[&str]) -> Result<Mode, String> {
            let mut args = vec!["tool", "work"];
            args.extend_from_slice(words);
            parse(&args)
        }

        fn work_mode(command: WorkCommand) -> Result<Mode, String> {
            Ok(Mode::Tool(ToolCommand::Work(command)))
        }

        #[test]
        fn tool_work_commands_parse() {
            use WorkCommand as C;
            let n = || "feature-a".to_string();
            let set = |priority: Option<&str>,
                       status: Option<&str>,
                       waits_for: Option<&str>,
                       next: Option<&str>| C::Set {
                name: n(),
                priority: priority.map(String::from),
                status: status.map(String::from),
                waits_for: waits_for.map(String::from),
                next: next.map(String::from),
            };
            for (words, command) in [
                (vec!["list"], C::List { all: false }),
                (vec!["list", "--all"], C::List { all: true }),
                (vec!["show", "feature-a"], C::Show { name: n() }),
                (vec!["put", "feature-a"], C::Put { name: n() }),
                (
                    vec!["set", "feature-a", "--priority", "focus"],
                    set(Some("focus"), None, None, None),
                ),
                (
                    vec![
                        "set",
                        "feature-a",
                        "--next",
                        "write the plan",
                        "--status",
                        "waiting for the user",
                        "--waits-for",
                        "the user",
                        "--priority",
                        "next",
                    ],
                    set(
                        Some("next"),
                        Some("waiting for the user"),
                        Some("the user"),
                        Some("write the plan"),
                    ),
                ),
                (
                    vec!["set", "feature-a", "--waits-for", ""],
                    set(None, None, Some(""), None),
                ),
                (
                    vec![
                        "decide",
                        "feature-a",
                        "--mark",
                        "agreed",
                        "--who",
                        "the user",
                        "keep the queue",
                    ],
                    C::Decide {
                        name: n(),
                        mark: "agreed".into(),
                        who: "the user".into(),
                        text: "keep the queue".into(),
                    },
                ),
                (
                    vec![
                        "decide",
                        "feature-a",
                        "--who",
                        "the chief",
                        "--mark",
                        "open",
                        "keep",
                        "the",
                        "queue",
                    ],
                    C::Decide {
                        name: n(),
                        mark: "open".into(),
                        who: "the chief".into(),
                        text: "keep the queue".into(),
                    },
                ),
                (
                    vec!["mark", "feature-a", "2", "dropped"],
                    C::Mark {
                        name: n(),
                        number: 2,
                        mark: "dropped".into(),
                    },
                ),
                (
                    vec!["link", "feature-a", "spec:harold/work"],
                    C::Link {
                        name: n(),
                        link: "spec:harold/work".into(),
                    },
                ),
                (
                    vec!["unlink", "feature-a", "question:Q4"],
                    C::Unlink {
                        name: n(),
                        link: "question:Q4".into(),
                    },
                ),
                (vec!["check"], C::Check),
                (
                    vec!["import", "/tmp/chief"],
                    C::Import {
                        folder: "/tmp/chief".into(),
                        queue: Vec::new(),
                        apply: false,
                    },
                ),
                (
                    vec!["import", "/tmp/chief", "--apply", "--queue", "N12,N34"],
                    C::Import {
                        folder: "/tmp/chief".into(),
                        queue: vec!["N12".into(), "N34".into()],
                        apply: true,
                    },
                ),
                (vec!["rotate", "--dry-run"], C::RotateDryRun),
                (vec!["history", "feature-a"], C::History { name: n() }),
                (
                    vec!["search", "queue"],
                    C::Search {
                        word: "queue".into(),
                    },
                ),
            ] {
                assert_eq!(work_parse(&words), work_mode(command), "{words:?}");
            }
        }

        #[test]
        fn a_dry_run_import_prints_a_reason_and_exits_1_with_an_unplaced_line() {
            let unplaced = |file: &str, line: u32, reason: &str, text: &str| work_api::Unplaced {
                file: file.into(),
                line,
                reason: reason.into(),
                text: text.into(),
            };
            let imported = work_api::ImportResponse {
                records: Vec::new(),
                unplaced: vec![
                    unplaced("a.md", 7, "unknown_line", "Owner: the chief"),
                    unplaced("b.md", 0, "not_utf8", ""),
                ],
                in_use: Vec::new(),
            };
            let dry = work_import_output(&imported, false);
            assert_eq!(dry.exit_code, 1);
            assert!(
                dry.text
                    .contains("unplaced a.md:7: unknown_line: Owner: the chief\n"),
                "{}",
                dry.text
            );
            assert!(
                dry.text.contains("unplaced b.md: not_utf8\n"),
                "{}",
                dry.text
            );
            // With apply, the service refuses first, so the exit code is not set here.
            assert_eq!(work_import_output(&imported, true).exit_code, 0);
        }

        #[test]
        fn a_clean_dry_run_import_exits_0() {
            let imported = work_api::ImportResponse::default();
            let dry = work_import_output(&imported, false);
            assert_eq!(dry.exit_code, 0);
            assert!(dry.text.contains("dry run: nothing is stored"));
        }

        #[test]
        fn a_malformed_work_command_is_refused_with_usage() {
            for words in [
                vec![],
                vec!["lists"],
                vec!["list", "--al"],
                vec!["list", "--all", "--all"],
                vec!["show"],
                vec!["show", "a", "b"],
                vec!["put"],
                vec!["set", "feature-a"],
                vec!["set", "feature-a", "--priority"],
                vec!["set", "feature-a", "--colour", "red"],
                vec!["decide", "feature-a", "--mark", "agreed", "text"],
                vec![
                    "decide",
                    "feature-a",
                    "--mark",
                    "agreed",
                    "--who",
                    "the user",
                ],
                vec!["mark", "feature-a", "two", "agreed"],
                vec!["mark", "feature-a", "2"],
                vec!["link", "feature-a"],
                vec!["unlink", "feature-a", "a", "b"],
                vec!["check", "--now"],
                vec!["import"],
                vec!["import", "/tmp/chief", "--queue"],
                vec!["import", "/tmp/chief", "--force"],
                vec!["rotate"],
                vec!["rotate", "--apply"],
                vec!["history"],
                vec!["search"],
                vec!["search", ""],
                vec!["search", "  "],
            ] {
                assert_eq!(work_parse(&words), Err(USAGE.to_string()), "{words:?}");
            }
        }

        #[test]
        fn refusal_lines_name_focus_full_and_not_chief() {
            use tonic::Status;
            for (status, words) in [
                (
                    Status::failed_precondition("focus_full"),
                    vec!["focus_full"],
                ),
                (
                    Status::permission_denied("not_chief"),
                    vec!["not_chief", "%9"],
                ),
                (Status::permission_denied("no_pane"), vec!["no_pane"]),
                (
                    Status::failed_precondition("name_in_use"),
                    vec!["name_in_use"],
                ),
                (Status::not_found("not_found"), vec!["not_found"]),
                (Status::invalid_argument("invalid name"), vec!["bad_name"]),
                (Status::failed_precondition("read_only"), vec!["read_only"]),
                (
                    Status::failed_precondition("rotation_is_harolds"),
                    vec!["rotation_is_harolds"],
                ),
                (
                    Status::invalid_argument("invalid priority"),
                    vec!["invalid priority"],
                ),
                (
                    Status::failed_precondition("focus_full plan-b"),
                    vec!["focus_full plan-b"],
                ),
                (
                    Status::failed_precondition("unplaced_lines 3"),
                    vec!["unplaced_lines 3"],
                ),
                (
                    Status::unimplemented(""),
                    vec!["does not serve the work records"],
                ),
            ] {
                let line = work_refusal_line(&status, "%9");
                for word in words {
                    assert!(line.contains(word), "{line:?} lacks {word:?}");
                }
                assert!(!line.contains('\n'), "{line:?}");
            }
        }

        fn record(name: &str, priority: &str, status: &str, since_ms: i64) -> work_api::WorkRecord {
            work_api::WorkRecord {
                name: name.into(),
                title: format!("Title {name}"),
                priority: priority.into(),
                status: status.into(),
                next_step: format!("next for {name}"),
                status_since_ms: since_ms,
                ..Default::default()
            }
        }

        #[test]
        fn list_names_the_records_waiting_for_the_user_over_a_day() {
            let now_ms = 10 * 86_400_000;
            let listed = work_api::ListResponse {
                records: vec![
                    record("a", "focus", "building", 0),
                    record("b", "next", "waiting for the user", now_ms - 25 * 3_600_000),
                    record("c", "later", "waiting for the user", now_ms - 3_600_000),
                    record("d", "later", "checked by the user", 0),
                ],
                file: "work-2026-11-03.db".into(),
            };
            let text = work_list_text(&listed, now_ms);
            let lines: Vec<&str> = text.lines().collect();
            assert!(
                lines[0].contains("focus") && lines[0].contains('a'),
                "{text}"
            );
            assert!(lines[0].contains("building") && lines[0].contains("next for a"));
            assert!(lines[1].contains("next") && lines[1].contains("waiting for the user"));
            assert_eq!(
                lines.last().copied(),
                Some("waiting for the user for more than a day: b"),
                "{text}"
            );
            let calm = work_api::ListResponse {
                records: vec![record(
                    "c",
                    "later",
                    "waiting for the user",
                    now_ms - 3_600_000,
                )],
                file: String::new(),
            };
            assert!(!work_list_text(&calm, now_ms).contains("more than a day"));
            let empty = work_api::ListResponse::default();
            assert_eq!(work_list_text(&empty, now_ms), "no open work records");
        }

        #[test]
        fn moves_text_shows_each_move_oldest_first() {
            let mv = |from: &str, to: &str, at_ms: i64| work_api::Move {
                from: from.into(),
                to: to.into(),
                at_ms,
            };
            let moves = vec![
                mv("", "idea", 1_793_697_000_000),
                mv("imported", "spec drafted", 1_793_697_060_000),
                mv("spec drafted", "architect checked", 1_793_697_120_000),
            ];
            assert_eq!(
                work_moves_text(&moves),
                "2026-11-03 09:10  created → idea\n\
                 2026-11-03 09:11  imported → spec drafted\n\
                 2026-11-03 09:12  spec drafted → architect checked"
            );
        }

        /// The work tool over a temp folder; `%2` is the chief.
        struct WorkServer {
            serving: Serving,
            _folder: TempFolder,
            _stop: tokio::sync::watch::Sender<()>,
        }

        async fn work_server() -> WorkServer {
            let folder = TempFolder::new();
            let (stop, shutdown) = tokio::sync::watch::channel(());
            let handle = crate::tools::work::start(crate::tools::ToolContext {
                data_folder: folder.0.join("data"),
                typing: None,
                chief_pane: std::sync::Arc::new(ChiefAt("%2")),
                pane_folder: std::sync::Arc::new(Folders(Vec::new())),
                products: std::sync::Arc::new(Registry(Vec::new())),
                agents: std::sync::Arc::new(crate::tools::tests::NoAgents),
                pods: std::sync::Arc::new(crate::tools::tests::NoPods),
                watch_settings: Default::default(),
                model_environment: Vec::new(),
                cli_path: "agent".into(),
                pod_registry: None,
                grpc_addr: None,
                watch_counts: std::sync::Arc::new(
                    tokio::sync::watch::channel(crate::tools::WatchCounts::default()).0,
                ),
                shutdown,
            })
            .await
            .unwrap_or_else(|e| panic!("start failed: {}", e.reason));
            WorkServer {
                serving: serve(handle.routes).await,
                _folder: folder,
                _stop: stop,
            }
        }

        impl WorkServer {
            async fn run(
                &self,
                pane: &str,
                words: &[&str],
                input: &str,
            ) -> Result<WorkOutput, String> {
                let Ok(Mode::Tool(ToolCommand::Work(command))) = work_parse(words) else {
                    panic!("not a work command: {words:?}");
                };
                work_within(
                    &self.serving.address,
                    &command,
                    input.as_bytes(),
                    pane,
                    time::OffsetDateTime::now_utc(),
                    TEST_WAIT,
                )
                .await
            }
        }

        /// What Harold prints on stdout for `output`, and its exit code.
        fn printed(output: &WorkOutput) -> (String, i32) {
            let mut out = Vec::new();
            let code = write_work_output(output, &mut out);
            (String::from_utf8(out).unwrap(), code)
        }

        #[tokio::test]
        async fn check_prints_nothing_and_exits_0_when_clean() {
            let server = work_server().await;
            let json =
                r#"{"title":"Work A","priority":"focus","status":"building","next_step":"build"}"#;
            server.run("%2", &["put", "work-a"], json).await.unwrap();
            let output = server.run("%40", &["check"], "").await.unwrap();
            assert_eq!(printed(&output), (String::new(), 0));
        }

        #[tokio::test]
        async fn check_prints_lines_and_exits_1() {
            let server = work_server().await;
            let json = r#"{"title":"Work A","priority":"focus","status":"building","next_step":"build","links":["spec:nowhere/a"]}"#;
            server.run("%2", &["put", "work-a"], json).await.unwrap();
            let json = r#"{"title":"Work B","priority":"next","status":"idea","next_step":"think","links":["spec:nowhere/b"]}"#;
            server.run("%2", &["put", "work-b"], json).await.unwrap();
            let output = server.run("%40", &["check"], "").await.unwrap();
            let (text, code) = printed(&output);
            assert_eq!(code, 1);
            let lines: Vec<&str> = text.lines().collect();
            assert_eq!(lines.len(), 2, "{text}");
            assert!(lines[0].starts_with("work-a ") && lines[0].contains("nowhere/a"));
            assert!(lines[1].starts_with("work-b ") && lines[1].contains("nowhere/b"));
        }

        #[tokio::test]
        async fn the_chief_keeps_work_records_through_the_cli() {
            let server = work_server().await;
            let focus = |name: &str| {
                format!(
                    r#"{{"title":"Title {name}","priority":"focus","status":"idea","next_step":"start {name}"}}"#
                )
            };
            let text = |result: Result<WorkOutput, String>| result.map(|output| output.text);
            assert_eq!(
                text(server.run("%2", &["put", "a"], &focus("a")).await),
                Ok("created a".into())
            );
            server.run("%2", &["put", "b"], &focus("b")).await.unwrap();
            let third = server.run("%2", &["put", "c"], &focus("c")).await;
            assert!(
                matches!(&third, Err(line) if line.contains("focus_full")),
                "{third:?}"
            );
            let other = server.run("%40", &["put", "d"], &focus("d")).await;
            assert!(
                matches!(&other, Err(line) if line.contains("not_chief") && line.contains("%40")),
                "{other:?}"
            );
            let set = server
                .run(
                    "%2",
                    &["set", "b", "--priority", "next", "--status", "approved"],
                    "",
                )
                .await;
            assert_eq!(text(set), Ok("set b".into()));
            let decided = server
                .run(
                    "%2",
                    &[
                        "decide", "a", "--mark", "open", "--who", "the user", "ship it",
                    ],
                    "",
                )
                .await;
            assert_eq!(text(decided), Ok("added decision 1 to a".into()));
            server
                .run("%2", &["mark", "a", "1", "agreed"], "")
                .await
                .unwrap();
            server
                .run("%2", &["link", "a", "note:keep it small"], "")
                .await
                .unwrap();
            let shown = server.run("%40", &["show", "a"], "").await.unwrap().text;
            assert!(shown.contains("1. agreed, the user"), "{shown}");
            assert!(shown.contains("note:keep it small"), "{shown}");
            let listed = server.run("%40", &["list"], "").await.unwrap().text;
            let first: Vec<&str> = listed
                .lines()
                .map(|line| line.split_whitespace().nth(1).unwrap_or(""))
                .collect();
            assert_eq!(first, vec!["a", "b"], "{listed}");
            let missing = server.run("%40", &["show", "zz"], "").await;
            assert!(
                matches!(&missing, Err(line) if line.contains("not_found")),
                "{missing:?}"
            );
            let rotate = server
                .run("%40", &["rotate", "--dry-run"], "")
                .await
                .unwrap()
                .text;
            assert!(rotate.contains("2 open work records"), "{rotate}");
            let found = server
                .run("%40", &["search", "Title"], "")
                .await
                .unwrap()
                .text;
            assert!(
                found.contains(" a ") || found.contains(" a\t") || found.contains("  a"),
                "{found}"
            );
        }

        #[tokio::test]
        async fn import_says_plainly_that_a_dry_run_stores_nothing() {
            let server = work_server().await;
            let folder = TempFolder::new();
            std::fs::write(
                folder.0.join("plan-a.md"),
                "# plan-a: Plan A\n\nPriority: next\nStatus: idea\nNext step: write it\nA stray line\n",
            )
            .unwrap();
            let path = folder.0.to_string_lossy().to_string();
            let output = server.run("%2", &["import", &path], "").await.unwrap();
            let text = output.text;
            assert!(text.contains("plan-a"), "{text}");
            assert!(text.contains("plan-a.md:6"), "{text}");
            assert!(text.contains("dry run"), "{text}");
            let listed = server.run("%40", &["list"], "").await.unwrap().text;
            assert_eq!(listed, "no open work records");
        }
    }
}
