//! The identity check: Harold's record of the agent against where the agent runs now.

use std::path::Path;

use super::{Agent, FailLine, Group, IdentityRow, Outcome};
use crate::pod_roles::PodRoles;

/// The pane and the agent pid that Harold holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Held {
    pub(crate) pane_id: String,
    pub(crate) agent_pid: u32,
}

/// The pod role record, as far as the check needs it.
pub(crate) trait RecordPort {
    fn held(&self, agent: &Agent) -> Option<Held>;
    /// Sets the pane and the agent pid of the record of `agent`.
    fn correct(&self, agent: &Agent, pane: &str, pid: u32);
}

impl RecordPort for PodRoles {
    fn held(&self, agent: &Agent) -> Option<Held> {
        let (product, role, spec) = agent.record_key();
        let record = self.get_spec(&product, role, &spec)?;
        Some(Held {
            pane_id: record.pane_id,
            agent_pid: record.agent_pid,
        })
    }

    fn correct(&self, agent: &Agent, pane: &str, pid: u32) {
        let (product, role, spec) = agent.record_key();
        PodRoles::correct(self, &product, role, &spec, pane, pid);
    }
}

/// The multiplexer could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ViewError;

/// The multiplexer's view of a pane.
pub(crate) trait PaneView: Send + Sync {
    /// The agent pid of the pane: `Ok(None)` when the pane does not exist or holds no agent.
    fn agent_pid(&self, pane: &str) -> Result<Option<u32>, ViewError>;
    /// The label of the multiplexer session that holds the pane, when it is known.
    fn session_label(&self, _pane: &str) -> Option<String> {
        None
    }
}

/// What a file of the pod names for the agent. Harold never writes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileClaim {
    /// `agent-panes` or `agent-pane`.
    pub(crate) name: String,
    pub(crate) pane: Option<String>,
}

pub(crate) struct IdentityInput<'a> {
    pub(crate) agent: &'a Agent,
    /// The pane in the hook report.
    pub(crate) hook_pane: &'a str,
    /// The hook's parent pid: the agent.
    pub(crate) hook_pid: u32,
    pub(crate) file: Option<FileClaim>,
}

pub(crate) struct IdentityResult {
    pub(crate) row: IdentityRow,
    pub(crate) fails: Vec<FailLine>,
}

fn fail(subject: &str, detail: String, text: String) -> IdentityResult {
    IdentityResult {
        row: IdentityRow {
            outcome: Outcome::Fail,
            subject: subject.into(),
            detail,
        },
        fails: vec![FailLine {
            group: Group::Identity,
            text,
        }],
    }
}

/// Checks the identity. A mismatch in the record is corrected from the multiplexer's view and
/// checked again. A file that Harold does not own is only read.
pub(crate) fn check_identity(
    input: &IdentityInput<'_>,
    record: &dyn RecordPort,
    view: &dyn PaneView,
) -> IdentityResult {
    let subject = input.agent.title();
    let pane = input.hook_pane;
    let runs_here = match view.agent_pid(pane) {
        Err(ViewError) => {
            return IdentityResult {
                row: IdentityRow {
                    outcome: Outcome::NotChecked,
                    subject,
                    detail: "multiplexer_unreachable".into(),
                },
                fails: Vec::new(),
            };
        }
        Ok(found) => found == Some(input.hook_pid),
    };
    if !runs_here {
        let detail = format!(
            "pane {pane} does not hold the agent (pid {})",
            input.hook_pid
        );
        let text = format!("{subject}: {detail}; Harold changes no record");
        return fail(&subject, detail, text);
    }
    let Some(held) = record.held(input.agent) else {
        let detail = "no_record: Harold has no record of this agent".to_owned();
        let text = format!("{subject}: {detail}");
        return fail(&subject, detail, text);
    };
    let matches = |held: &Held| held.pane_id == pane && held.agent_pid == input.hook_pid;
    let mut fixed = None;
    if !matches(&held) {
        record.correct(input.agent, pane, input.hook_pid);
        let again = record.held(input.agent);
        if !again.as_ref().is_some_and(matches) {
            let detail = format!(
                "Harold has pane {}; the agent now runs in pane {pane}",
                held.pane_id
            );
            let text = format!(
                "{subject}: Harold corrected its record to pane {pane}, and the second check still differs"
            );
            return fail(&subject, detail, text);
        }
        fixed = Some(held.pane_id);
    }
    if let Some(file) = input
        .file
        .as_ref()
        .filter(|file| file.pane.as_deref() != Some(pane))
    {
        let named = file.pane.as_deref().unwrap_or("no pane");
        let detail = format!("{} names {named}; the agent runs in pane {pane}", file.name);
        let text = format!(
            "{subject}: the file {} names {named}, not {pane}; its owner fixes it",
            file.name
        );
        return fail(&subject, detail, text);
    }
    match fixed {
        Some(old) => IdentityResult {
            row: IdentityRow {
                outcome: Outcome::Fixed,
                subject,
                detail: format!("Harold had pane {old}; its record now says pane {pane}"),
            },
            fails: Vec::new(),
        },
        None => IdentityResult {
            row: IdentityRow {
                outcome: Outcome::Ok,
                subject,
                detail: format!("pane {pane}"),
            },
            fails: Vec::new(),
        },
    }
}

/// The pane that the text of an `agent-panes` file names for the agent: line one is the
/// pod-coordination, line two the PnEM. The `agent-pane` file of a build worker holds its pane
/// in line one. Each line gives its first word.
pub(crate) fn pane_in_file(text: &str, agent: &Agent) -> Option<String> {
    let line = match agent {
        Agent::PodCoordination(_) | Agent::BuildWorker { .. } => 0,
        Agent::Pnem(_) => 1,
        Agent::Chief => return None,
    };
    text.lines()
        .filter_map(|line| line.split_whitespace().next())
        .nth(line)
        .map(str::to_owned)
}

/// Reads the file at `path` for the agent. `None` when the file is missing. It never writes.
pub(crate) fn read_file_claim(path: &Path, agent: &Agent) -> Option<FileClaim> {
    let text = std::fs::read_to_string(path).ok()?;
    let name = path.file_name()?.to_string_lossy().into_owned();
    Some(FileClaim {
        name,
        pane: pane_in_file(&text, agent),
    })
}

#[cfg(test)]
#[path = "identity_tests.rs"]
mod tests;
