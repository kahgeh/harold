//! The `harold tool pod` command line: what it parses, and the text it prints. The calls go
//! through `cli.rs`, as for the other tools.

use harold_api::tools::pod::v1 as pb;

pub(crate) const USAGE_LINE: &str =
    "invalid argument: usage is harold tool pod who <product> <role> [--json]";

/// A `harold tool pod` command. The daemon checks the values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PodCommand {
    Who {
        product: String,
        role: String,
        json: bool,
    },
}

pub(crate) fn parse(args: &[&str]) -> Result<PodCommand, String> {
    let usage = || USAGE_LINE.to_string();
    match args {
        ["who", product, role] => Ok(PodCommand::Who {
            product: (*product).into(),
            role: (*role).into(),
            json: false,
        }),
        ["who", product, role, "--json"] => Ok(PodCommand::Who {
            product: (*product).into(),
            role: (*role).into(),
            json: true,
        }),
        _ => Err(usage()),
    }
}

/// The text to print: the session name, or with `json` the name and the whole record.
pub(crate) fn render(answer: &pb::WhoResponse, json: bool) -> String {
    if !json {
        return answer.session_name.clone();
    }
    let record = answer.record.clone().unwrap_or_default();
    serde_json::json!({
        "session_name": answer.session_name,
        "record": {
            "pane_id": record.pane_id,
            "session_id": record.session_id,
            "agent_pid": record.agent_pid,
            "agent_proc_start": record.agent_proc_start,
            "folder": record.folder,
            "transcript_path": record.transcript_path,
            "start_file": record.start_file,
            "started_at_ms": record.started_at_ms,
        },
    })
    .to_string()
}

/// The one refusal line for a status of the pod service.
pub(crate) fn refusal_line(status: &tonic::Status) -> String {
    match (status.code(), status.message()) {
        (tonic::Code::Unimplemented, _) => "the Harold daemon does not serve the pod tool".into(),
        (tonic::Code::InvalidArgument, "bad_role") => {
            "bad_role: the role must be chief, pnem or pod_coordination".into()
        }
        _ if status.message().is_empty() => "the Harold daemon refused the call".into(),
        _ => status.message().to_string(),
    }
}

#[cfg(test)]
#[path = "command_tests.rs"]
mod tests;
