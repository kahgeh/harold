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
use harold_api::tools::knowledge::v1 as knowledge_api;
use harold_api::tools::knowledge::v1::knowledge_service_client::KnowledgeServiceClient;
use harold_api::tools::questions::v1 as questions;
use harold_api::tools::questions::v1::question_service_client::QuestionServiceClient;
use harold_api::tools::watch::v1 as watch_api;
use harold_api::tools::watch::v1::watch_service_client::WatchServiceClient;

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
    App(AppCommand),
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
    "invalid argument: usage is harold tool questions put|dismiss|order|list|show|checked";
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
    QuestionsShow { id: String },
    QuestionsChecked,
    Backlog(BacklogCommand),
    Watch(WatchCommand),
    Knowledge(KnowledgeCommand),
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

const WATCH_USAGE_LINE: &str = "invalid argument: usage is harold tool watch start NAME --skill SKILL --every MINUTES --for MINUTES [--report-to chief|self] [--run-budget USD] [--every-run] | stop NAME | list";

/// A `harold tool watch` command. The numbers stay text here: the daemon checks the
/// ranges, and the CLI only needs them to be numbers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WatchCommand {
    Start {
        name: String,
        skill: String,
        every_minutes: u32,
        for_minutes: u32,
        report_to: Option<String>,
        run_budget_usd: Option<String>,
        every_run: bool,
    },
    Stop {
        name: String,
    },
    List,
}

fn parse_watch(args: &[&str]) -> Result<WatchCommand, String> {
    match args {
        ["list"] => Ok(WatchCommand::List),
        ["stop", name] => Ok(WatchCommand::Stop {
            name: (*name).into(),
        }),
        ["start", name, flags @ ..] => parse_watch_start(name, flags),
        _ => Err(WATCH_USAGE_LINE.into()),
    }
}

fn parse_watch_start(name: &str, flags: &[&str]) -> Result<WatchCommand, String> {
    let (mut skill, mut every, mut minutes) = (None, None, None);
    let (mut report_to, mut budget) = (None, None);
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
            "--for" => &mut minutes,
            "--report-to" => &mut report_to,
            "--run-budget" => &mut budget,
            _ => return Err(WATCH_USAGE_LINE.into()),
        };
        let value = rest.next().ok_or_else(|| WATCH_USAGE_LINE.to_string())?;
        if slot.replace((*value).to_string()).is_some() {
            return Err(WATCH_USAGE_LINE.into());
        }
    }
    let number = |value: Option<String>, field: &str| -> Result<u32, String> {
        value
            .ok_or_else(|| WATCH_USAGE_LINE.to_string())?
            .parse::<u32>()
            .map_err(|_| format!("invalid {field}"))
    };
    if let Some(text) = &budget
        && !text
            .parse::<f64>()
            .is_ok_and(|budget| budget.is_finite() && budget > 0.0)
    {
        return Err("invalid run_budget_usd".into());
    }
    Ok(WatchCommand::Start {
        name: name.into(),
        skill: skill.ok_or_else(|| WATCH_USAGE_LINE.to_string())?,
        every_minutes: number(every, "every_minutes")?,
        for_minutes: number(minutes, "for_minutes")?,
        report_to,
        run_budget_usd: budget,
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

fn parse_tool(args: &[&str]) -> Result<ToolCommand, String> {
    match args {
        ["backlog", rest @ ..] => parse_backlog(rest).map(ToolCommand::Backlog),
        ["watch", rest @ ..] => parse_watch(rest).map(ToolCommand::Watch),
        ["knowledge", rest @ ..] => parse_knowledge(rest).map(ToolCommand::Knowledge),
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
            ["accept", rest @ ..] => parse_accept(rest).map(Self::Accept),
            [AWAY_MESSAGE_COMMAND, rest @ ..] => {
                parse_away_message(rest).map(Self::SendMessageToAwayDevice)
            }
            [SPEAK_COMMAND, rest @ ..] => {
                parse_source_and_text(rest, SPEAK_ARGUMENTS, SPOKEN_LENGTH_LINE).map(Self::Speak)
            }
            [TOOL_COMMAND, rest @ ..] => parse_tool(rest).map(Self::Tool),
            [APP_COMMAND, rest @ ..] => parse_app(rest).map(Self::App),
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
    if let ToolCommand::Knowledge(knowledge) = command {
        return knowledge_within(address, knowledge, KNOWLEDGE_DEADLINE).await;
    }
    tool_within(address, command, stdin, CALL_DEADLINE).await
}

/// The line printed for a refusal from the watch service. `skill` and `name` are the
/// caller's.
fn watch_refusal_line(status: &tonic::Status, command: &WatchCommand) -> String {
    let subject = match command {
        WatchCommand::Start { name, .. } | WatchCommand::Stop { name } => name.as_str(),
        WatchCommand::List => "",
    };
    match (status.code(), status.message()) {
        (tonic::Code::Unimplemented, _) => "the Harold daemon does not serve watches".into(),
        (tonic::Code::NotFound, "unknown_skill") => match command {
            WatchCommand::Start { skill, .. } => format!("unknown skill {skill}"),
            _ => "unknown skill".into(),
        },
        (tonic::Code::NotFound, "unknown_watch") => format!("unknown watch {subject}"),
        (tonic::Code::FailedPrecondition, "not_running") => format!("{subject} is not running"),
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

fn watch_list_line(watch: &watch_api::Watch, offset: time::UtcOffset) -> String {
    let counts = format!(
        "{}, {}, ${:.2}",
        plural(watch.runs, "run", "runs"),
        plural(watch.reports, "report", "reports"),
        watch.cost_usd
    );
    if watch.running {
        return format!(
            "{} running in {} until {}, {counts}",
            watch.name,
            watch.folder,
            watch_time(watch.ends_at_ms, offset)
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
                every_minutes,
                for_minutes,
                report_to,
                run_budget_usd,
                every_run,
            } => {
                let started = client
                    .start_watch(watch_api::StartWatchRequest {
                        name: name.clone(),
                        folder: folder.to_string_lossy().into_owned(),
                        skill: skill.clone(),
                        every_minutes: *every_minutes,
                        for_minutes: *for_minutes,
                        report_to: report_to.clone().unwrap_or_default(),
                        run_budget_usd: run_budget_usd
                            .as_deref()
                            .and_then(|text| text.parse().ok())
                            .unwrap_or(0.0),
                        every_run: *every_run,
                    })
                    .await
                    .map_err(refused)?
                    .into_inner();
                Ok(format!(
                    "started {} in {} until {}, every {every_minutes} minutes, reporting to {}",
                    started.name,
                    folder.display(),
                    watch_time(started.ends_at_ms, offset),
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
                let mut lines: Vec<String> = listed
                    .watches
                    .iter()
                    .map(|watch| watch_list_line(watch, offset))
                    .collect();
                lines.push(format!("spent today ${:.2}", listed.spent_usd));
                Ok(lines.join("\n"))
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
            // `tool` runs a backlog command through `backlog_within`, and a watch
            // command through `watch_within`.
            ToolCommand::Backlog(_) | ToolCommand::Watch(_) | ToolCommand::Knowledge(_) => {
                Err(TOOL_USAGE_LINE.into())
            }
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

    fn parse(args: &[&str]) -> Result<Mode, String> {
        Mode::parse(&args.iter().map(|a| a.to_string()).collect::<Vec<_>>())
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
        let put = tool(&address, &ToolCommand::QuestionsPut, json.as_bytes()).await;
        let current = tool(&address, &ToolCommand::QuestionsList, &b""[..]).await;
        std::fs::write(&spec, b"first, then revised").unwrap();
        let changed = tool(&address, &ToolCommand::QuestionsList, &b""[..]).await;
        let before = watch_time(wall_ms(), crate::telemetry::local_offset());
        let checked = tool(&address, &ToolCommand::QuestionsChecked, &b""[..]).await;
        let after = watch_time(wall_ms(), crate::telemetry::local_offset());
        let outside = tool(
            &address,
            &ToolCommand::QuestionsPut,
            json.replacen(&spec.display().to_string(), "/etc/hosts.md", 1)
                .as_bytes(),
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

        let unknown = tool(
            &address,
            &ToolCommand::QuestionsShow { id: "q-9".into() },
            &b""[..],
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
            agents: std::sync::Arc::new(crate::tools::tests::NoAgents),
            pods: std::sync::Arc::new(crate::tools::tests::NoPods),
            watch_settings: Default::default(),
            model_environment: Vec::new(),
            cli_path: "claude".into(),
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
        assert_eq!(no_pane, Err("refused: not run in a tmux pane".into()));
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
            every_minutes: 10,
            for_minutes: 120,
            report_to: None,
            run_budget_usd: None,
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
                every_minutes: 5,
                for_minutes: 60,
                report_to: Some("chief".into()),
                run_budget_usd: Some("0.03".into()),
                every_run: true,
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
        for args in [
            vec![],
            vec!["start"],
            vec!["start", "w"],
            vec!["start", "w", "--skill", "s", "--every", "10"],
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
            every_minutes: 10,
            for_minutes: 5,
            report_to: None,
            run_budget_usd: None,
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
            ends_at_ms: 20 * 3_600_000 + 45 * 60_000,
            runs: 3,
            reports: 1,
            cost_usd: 0.03,
            ..Default::default()
        };
        assert_eq!(
            watch_list_line(&running, utc),
            "after-release running in /work/chief-of-pods until 20:45, 3 runs, 1 report, $0.03"
        );
        let ended = watch_api::Watch {
            running: false,
            stop_reason: "daily_budget_spent".into(),
            runs: 1,
            reports: 2,
            ..running
        };
        assert_eq!(
            watch_list_line(&ended, utc),
            "after-release ended (daily budget spent) in /work/chief-of-pods, 1 run, 2 reports, $0.03"
        );
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
            shutdown,
        })
        .await
        .unwrap_or_else(|e| panic!("start failed: {}", e.reason));
        (serve(handle.routes).await, stop)
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
            async move { watch_within(&address, &command, &work, utc, CALL_DEADLINE).await }
        };
        let start = |skill: &str| WatchCommand::Start {
            name: "after-release".into(),
            skill: skill.into(),
            every_minutes: 10,
            for_minutes: 120,
            report_to: Some("chief".into()),
            run_budget_usd: None,
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
    }
}
