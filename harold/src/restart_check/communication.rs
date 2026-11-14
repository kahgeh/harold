//! The communication check. Harold, not a model, tests each agent that a role talks to, in
//! both directions. It sends no message: a message starts a turn and costs tokens. The test
//! proves the address and the delivery point, not that the agent reads the message.

use std::os::unix::net::UnixStream;
use std::path::Path;

use super::{Agent, FailLine, Group, LinkRow, LinkTable, Outcome, ViewError};

/// How a link delivers a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Delivery {
    /// A cross-session message: the receiver's messaging socket accepts a connection.
    SendMessage,
    /// A typed line into the receiver's pane: the pane holds the receiver's agent pid.
    Pane,
}

/// The agents that run now, from the pod role record.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Roster {
    /// The products whose pod runs.
    pub(crate) pods: Vec<String>,
    /// The build workers that run: product and spec id.
    pub(crate) workers: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlanRow {
    pub(crate) label: Vec<String>,
    pub(crate) peer: Agent,
    pub(crate) delivery: Delivery,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LinkPlan {
    pub(crate) header: Vec<String>,
    pub(crate) rows: Vec<PlanRow>,
}

fn words(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| (*part).to_owned()).collect()
}

/// The agents that `agent` talks to. The chief: each running pod's PnEM and pod-coordination.
/// A PnEM: the chief and its pod-coordination. Pod-coordination: the chief, its PnEM and each
/// running build worker. A build worker: its pod-coordination. A pod role talks through
/// `SendMessage`; a build worker and its pod-coordination talk through the pane.
pub(crate) fn plan_links(agent: &Agent, roster: &Roster) -> LinkPlan {
    let row = |label: Vec<String>, peer: Agent, delivery| PlanRow {
        label,
        peer,
        delivery,
    };
    let send = Delivery::SendMessage;
    match agent {
        Agent::Chief => LinkPlan {
            header: words(&["pod", "agent", "chief -> agent", "agent -> chief"]),
            rows: roster
                .pods
                .iter()
                .flat_map(|product| {
                    [
                        row(
                            words(&[product, "pnem"]),
                            Agent::Pnem(product.clone()),
                            send,
                        ),
                        row(
                            words(&[product, "pod-coordination"]),
                            Agent::PodCoordination(product.clone()),
                            send,
                        ),
                    ]
                })
                .collect(),
        },
        Agent::Pnem(product) => LinkPlan {
            header: words(&["", "pnem -> agent", "agent -> pnem"]),
            rows: vec![
                row(words(&["chief"]), Agent::Chief, send),
                row(
                    vec![format!("pod-coordination {product}")],
                    Agent::PodCoordination(product.clone()),
                    send,
                ),
            ],
        },
        Agent::PodCoordination(product) => {
            let mut rows = vec![
                row(words(&["chief"]), Agent::Chief, send),
                row(
                    vec![format!("pnem {product}")],
                    Agent::Pnem(product.clone()),
                    send,
                ),
            ];
            rows.extend(
                roster
                    .workers
                    .iter()
                    .filter(|(worker_product, _)| worker_product == product)
                    .map(|(worker_product, spec)| {
                        let worker = Agent::BuildWorker {
                            product: worker_product.clone(),
                            spec: spec.clone(),
                        };
                        row(vec![worker.title()], worker, Delivery::Pane)
                    }),
            );
            LinkPlan {
                header: words(&["", "coordination -> agent", "agent -> coordination"]),
                rows,
            }
        }
        Agent::BuildWorker { product, .. } => LinkPlan {
            header: words(&["", "worker -> agent", "agent -> worker"]),
            rows: vec![row(
                vec![format!("pod-coordination {product}")],
                Agent::PodCoordination(product.clone()),
                Delivery::Pane,
            )],
        },
    }
}

/// What the sender's lookup gives for the receiver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Resolved {
    pub(crate) pane_id: String,
    pub(crate) agent_pid: u32,
}

/// The result of a connection to a messaging socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SocketProbe {
    Accepts,
    /// A path that is set, with nothing on disk.
    Missing,
    /// The path exists and refuses a connection.
    Refused,
    /// The session file has no socket path, or its shape changed.
    Unknown,
}

pub(crate) trait LinkPorts {
    /// The lookup that the sender uses: `harold tool pod who` for a pod role, and the
    /// `agent-pane` file for a build worker. `Err` holds the reason word.
    fn lookup(&self, sender: &Agent, receiver: &Agent) -> Result<Resolved, &'static str>;
    /// The agent pid that runs now for the receiver, or `None`.
    fn running(&self, receiver: &Agent) -> Option<u32>;
    /// A connection to the messaging socket of the agent with this pid.
    fn socket(&self, pid: u32) -> SocketProbe;
    /// The agent pid that the multiplexer shows in the pane.
    fn pane_agent_pid(&self, pane: &str) -> Result<Option<u32>, ViewError>;
}

pub(crate) struct LinksResult {
    pub(crate) table: LinkTable,
    /// One line for each direction that failed.
    pub(crate) fails: Vec<FailLine>,
    /// One line for each direction that Harold cannot check, with its reason.
    pub(crate) not_checked: Vec<String>,
}

fn direction(
    sender: &Agent,
    receiver: &Agent,
    delivery: Delivery,
    ports: &dyn LinkPorts,
) -> (Outcome, &'static str) {
    let resolved = match ports.lookup(sender, receiver) {
        Ok(resolved) => resolved,
        Err(reason) => return (Outcome::Fail, reason),
    };
    if ports.running(receiver) != Some(resolved.agent_pid) {
        return (Outcome::Fail, "old_agent");
    }
    match delivery {
        Delivery::SendMessage => match ports.socket(resolved.agent_pid) {
            SocketProbe::Accepts => (Outcome::Ok, ""),
            SocketProbe::Missing => (Outcome::Fail, "socket_missing"),
            SocketProbe::Refused => (Outcome::Fail, "socket_refused"),
            SocketProbe::Unknown => (Outcome::NotChecked, "socket_unknown"),
        },
        Delivery::Pane => match ports.pane_agent_pid(&resolved.pane_id) {
            Ok(Some(pid)) if pid == resolved.agent_pid => (Outcome::Ok, ""),
            Ok(_) => (Outcome::Fail, "pane_holds_other_agent"),
            Err(ViewError) => (Outcome::NotChecked, "multiplexer_unreachable"),
        },
    }
}

/// What a reason word means, and who fixes it, in plain words for a FAIL line.
fn explain(reason: &str) -> &'static str {
    match reason {
        "no_record" => {
            "Harold has no record of the receiver; the receiver starts again with its role set"
        }
        "agent_gone" => "the process of the receiver ended; the receiver starts again",
        "agent_pane_file" => "the agent-pane file is missing or empty; pod-coordination writes it",
        "pane_no_agent" => "the pane holds no agent; pod-coordination fixes the agent-pane file",
        "old_agent" => "the address names an old agent; the sender looks the receiver up again",
        "socket_missing" => "the messaging socket is missing; the receiver starts again",
        "socket_refused" => "the messaging socket refuses a connection; the receiver starts again",
        "pane_holds_other_agent" => {
            "the pane holds another agent; the owner of the pane file fixes it"
        }
        _ => "the owner of the receiver fixes it",
    }
}

/// Tests both directions of each row of the plan.
pub(crate) fn check_links(me: &Agent, plan: &LinkPlan, ports: &dyn LinkPorts) -> LinksResult {
    let mut rows = Vec::new();
    let mut fails = Vec::new();
    let mut not_checked = Vec::new();
    for planned in &plan.rows {
        let mut results = [me, &planned.peer].map(|_| (Outcome::Ok, ""));
        for (slot, (sender, receiver)) in [(me, &planned.peer), (&planned.peer, me)]
            .into_iter()
            .enumerate()
        {
            let (outcome, reason) = direction(sender, receiver, planned.delivery, ports);
            results[slot] = (outcome, reason);
            let link = format!("{} -> {}: {reason}", sender.title(), receiver.title());
            match outcome {
                Outcome::Fail => fails.push(FailLine {
                    group: Group::Communication,
                    text: format!("{link}: {}", explain(reason)),
                }),
                Outcome::NotChecked => not_checked.push(link),
                _ => {}
            }
        }
        rows.push(LinkRow {
            label: planned.label.clone(),
            outgoing: results[0].0,
            incoming: results[1].0,
            outgoing_reason: results[0].1,
            incoming_reason: results[1].1,
        });
    }
    LinksResult {
        table: LinkTable {
            header: plan.header.clone(),
            rows,
        },
        fails,
        not_checked,
    }
}

/// The messaging socket path in the text of an agent's session file, or `None` when the key
/// is missing or is not a string.
pub(crate) fn socket_path_in(text: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    value
        .get("messagingSocketPath")?
        .as_str()
        .map(str::to_owned)
}

/// Connects to the socket and closes it at once. It writes no byte.
pub(crate) fn probe_socket(path: Option<&Path>) -> SocketProbe {
    let Some(path) = path else {
        return SocketProbe::Unknown;
    };
    if !path.exists() {
        return SocketProbe::Missing;
    }
    match UnixStream::connect(path) {
        Ok(stream) => {
            drop(stream);
            SocketProbe::Accepts
        }
        Err(_) => SocketProbe::Refused,
    }
}

#[cfg(test)]
#[path = "communication_tests.rs"]
mod tests;
