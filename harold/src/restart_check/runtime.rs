//! The restart check at run time: the real record, the multiplexer, the session files, the
//! receipt and the delivery of each FAIL. The checks themselves are in the sibling files.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tracing::{info, warn};

use super::{
    Agent, IdentityInput, LinkPorts, Outcome, PaneView, Recipient, Report, Resolved, Roster,
    SocketProbe, ViewError, check_identity, check_links, pane_in_file, plan_links, probe_socket,
    read_file_claim, receipt_path, render, socket_path_in, write_receipt,
};
use crate::pod_roles::PodRoles;
use crate::tools::job::process::ProcessPort;

/// The pod folder of an agent whose working folder is `folder`.
const POD_FOLDER: &str = ".superpowers/pod";
/// The file in a build worker's worktree that holds its pane.
const WORKER_PANE_FILE: &str = ".superpowers/baton/agent-pane";
/// The file in the pod folder that holds the panes of pod-coordination and the PnEM.
const POD_PANES_FILE: &str = "agent-panes";

/// The local time as `YYYY-MM-DD HH:MM` and as `YYYYMMDD-HHMM`.
pub(crate) trait TimeSource: Send + Sync {
    fn now(&self) -> (String, String);
}

/// Types FAIL lines and sends the phone line.
#[tonic::async_trait]
pub(crate) trait Deliver: Send + Sync {
    /// Types one line into the pane when its agent is idle. `false` when nothing was typed.
    async fn type_line(&self, pane: &str, text: &str) -> bool;
    /// Sends one line to the away device, from the agent in `source_pane`.
    async fn phone_line(&self, source_pane: &str, text: &str) -> bool;
}

/// A finished check.
pub(crate) struct Finished {
    pub(crate) text: String,
    pub(crate) fails: u32,
}

/// An agent that started again and waits for its first turn to end.
struct Pending {
    agent: Agent,
    agent_pid: u32,
}

pub(crate) struct Checker {
    roles: Arc<PodRoles>,
    processes: Arc<dyn ProcessPort>,
    view: Arc<dyn PaneView>,
    sessions: PathBuf,
    clock: Arc<dyn TimeSource>,
    deliver: Arc<dyn Deliver>,
    /// The restarts that wait, by the new session id.
    pending: Mutex<HashMap<String, Pending>>,
}

impl Checker {
    pub(crate) fn new(
        roles: Arc<PodRoles>,
        processes: Arc<dyn ProcessPort>,
        view: Arc<dyn PaneView>,
        sessions: PathBuf,
        clock: Arc<dyn TimeSource>,
        deliver: Arc<dyn Deliver>,
    ) -> Self {
        Self {
            roles,
            processes,
            view,
            sessions,
            clock,
            deliver,
            pending: Mutex::new(HashMap::new()),
        }
    }

    fn pending(&self) -> std::sync::MutexGuard<'_, HashMap<String, Pending>> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Notes a role start. A restart is a start with `startup` or `resume` as its source, in
    /// a role that held another session before. The check runs at the end of the first turn.
    pub(crate) fn note_start(
        &self,
        agent: &Agent,
        before_session: Option<String>,
        new_session: &str,
        agent_pid: u32,
        source: &str,
    ) {
        let restart = matches!(source, "startup" | "resume")
            && before_session.is_some_and(|before| before != new_session);
        if restart {
            self.pending().insert(
                new_session.to_owned(),
                Pending {
                    agent: agent.clone(),
                    agent_pid,
                },
            );
        }
    }

    /// Runs the check of a restart that waits for this session, once.
    pub(crate) async fn on_turn_end(&self, pane: &str, session: &str) {
        let Some(pending) = self.pending().remove(session) else {
            return;
        };
        self.run_at(&pending.agent, pane, pending.agent_pid).await;
    }

    /// Runs the check against the record: the pane and the pid that Harold holds.
    pub(crate) async fn run(&self, agent: &Agent) -> Option<Finished> {
        let (product, role, spec) = agent.record_key();
        let held = self.roles.get_spec(&product, role, &spec)?;
        self.run_at(agent, &held.pane_id, held.agent_pid).await
    }

    /// Runs the check for an agent that the hook reports in `pane` with `pid`. `None` when
    /// Harold has no record of the agent.
    pub(crate) async fn run_at(&self, agent: &Agent, pane: &str, pid: u32) -> Option<Finished> {
        let (report, folder) = self.build(agent, pane, pid)?;
        let text = render(&report);
        let (_, stamp) = self.clock.now();
        if let Some(folder) = folder {
            let path = receipt_path(&folder, agent, &stamp);
            if write_receipt(&path, &text).is_err() {
                warn!(role = %agent.record_key().1, reason = "receipt_not_written", "restart check receipt");
            }
        }
        log_rows(agent, &report);
        let fails = u32::try_from(report.fails.len()).unwrap_or(u32::MAX);
        self.send(agent, &report).await;
        Some(Finished { text, fails })
    }

    /// The report, and the folder for the receipt.
    fn build(&self, agent: &Agent, pane: &str, pid: u32) -> Option<(Report, Option<PathBuf>)> {
        let (product, role, spec) = agent.record_key();
        let held = self.roles.get_spec(&product, role, &spec)?;
        let own_folder = Path::new(&held.folder);
        let file = match agent {
            Agent::Chief => None,
            Agent::Pnem(_) | Agent::PodCoordination(_) => {
                read_file_claim(&own_folder.join(POD_FOLDER).join(POD_PANES_FILE), agent)
            }
            Agent::BuildWorker { .. } => read_file_claim(&own_folder.join(WORKER_PANE_FILE), agent),
        };
        let identity = check_identity(
            &IdentityInput {
                agent,
                hook_pane: pane,
                hook_pid: pid,
                file,
            },
            &*self.roles,
            &*self.view,
        );
        let links = Links { checker: self };
        let plan = plan_links(agent, &self.roster());
        let result = check_links(agent, &plan, &links);
        let (time, _) = self.clock.now();
        let mut fails = identity.fails;
        fails.extend(result.fails);
        let report = Report {
            agent: agent.clone(),
            session: self
                .view
                .session_label(pane)
                .unwrap_or_else(|| "unknown".into()),
            pane: short_pane(pane),
            time,
            identity: identity.row,
            communication: result.table,
            fails,
            not_checked: result.not_checked,
        };
        Some((report, self.receipt_folder(agent, own_folder)))
    }

    /// The folder that holds `restarts/`: the pod folder of the agent. A build worker's
    /// receipt goes in the pod folder of its pod-coordination. The chief's folder is its own
    /// working folder, and `receipt_path` adds the pod folder for it.
    fn receipt_folder(&self, agent: &Agent, own: &Path) -> Option<PathBuf> {
        match agent {
            Agent::Chief => Some(own.to_path_buf()),
            Agent::Pnem(_) | Agent::PodCoordination(_) => Some(own.join(POD_FOLDER)),
            Agent::BuildWorker { product, .. } => {
                let coordination = self.roles.get(product, "pod_coordination")?;
                Some(Path::new(&coordination.folder).join(POD_FOLDER))
            }
        }
    }

    /// The agents that run now, from the record.
    fn roster(&self) -> Roster {
        let mut roster = Roster::default();
        for (product, role, spec, record) in self.roles.entries() {
            if self.processes.start_time(record.agent_pid).is_none() {
                continue;
            }
            match role.as_str() {
                "pnem" => {
                    if !roster.pods.contains(&product) {
                        roster.pods.push(product);
                    }
                }
                crate::pod_roles::BUILD_WORKER => roster.workers.push((product, spec)),
                _ => {}
            }
        }
        roster.pods.sort();
        roster.workers.sort();
        roster
    }

    /// Sends each FAIL to the agent that fixes it, as one line. The chief's own FAIL goes to
    /// the phone as one line.
    async fn send(&self, agent: &Agent, report: &Report) {
        if report.fails.is_empty() {
            return;
        }
        let line = one_line(&delivery_line(report));
        let (product, _, _) = agent.record_key();
        let (recipient_pane, to_phone) = match (agent, agent.recipient()) {
            (Agent::Chief, _) => (self.roles.get("-", "chief").map(|r| r.pane_id), true),
            (_, Recipient::Chief) => (self.roles.get("-", "chief").map(|r| r.pane_id), false),
            (_, Recipient::PodCoordination) => (
                self.roles
                    .get(&product, "pod_coordination")
                    .map(|r| r.pane_id),
                false,
            ),
        };
        let Some(pane) = recipient_pane.filter(|pane| !pane.is_empty()) else {
            warn!(reason = "no_recipient", "restart check not delivered");
            return;
        };
        let delivered = if to_phone {
            self.deliver.phone_line(&pane, &line).await
        } else {
            self.deliver.type_line(&pane, &line).await
        };
        info!(
            role = %agent.record_key().1,
            channel = if to_phone { "phone" } else { "pane" },
            delivered,
            "restart check delivered"
        );
    }
}

fn short_pane(pane: &str) -> String {
    let id = pane.rsplit(':').next().unwrap_or(pane);
    id.trim_start_matches('%')
        .chars()
        .filter(|c| *c != '-')
        .take(6)
        .collect()
}

fn one_line(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

fn delivery_line(report: &Report) -> String {
    let lines: Vec<&str> = report.fails.iter().map(|f| f.text.as_str()).collect();
    format!(
        "restart check {}: {}; result: {} FAIL",
        report.agent.title(),
        lines.join("; "),
        report.fails.len()
    )
}

fn log_rows(agent: &Agent, report: &Report) {
    let (product, role, _) = agent.record_key();
    let word = |outcome: Outcome| match outcome {
        Outcome::Ok => "ok",
        Outcome::Fixed => "fixed",
        Outcome::Fail => "fail",
        Outcome::NotChecked => "not_checked",
    };
    info!(
        role, pod.product = %product, group = "identity", check = "identity",
        result = word(report.identity.outcome), reason = "", "restart check result"
    );
    for row in &report.communication.rows {
        let check = row.label.join(" ");
        for result in [row.outgoing, row.incoming] {
            info!(
                role, pod.product = %product, group = "communication", check = %check,
                result = word(result), reason = "", "restart check result"
            );
        }
    }
}

/// The lookups and tests of the links, over the real record, view and session files.
struct Links<'a> {
    checker: &'a Checker,
}

impl LinkPorts for Links<'_> {
    fn lookup(&self, _sender: &Agent, receiver: &Agent) -> Result<Resolved, &'static str> {
        let checker = self.checker;
        let (product, role, spec) = receiver.record_key();
        let record = checker
            .roles
            .get_spec(&product, role, &spec)
            .ok_or("no_record")?;
        if checker.processes.start_time(record.agent_pid).is_none() {
            return Err("agent_gone");
        }
        if matches!(receiver, Agent::BuildWorker { .. }) {
            let file = Path::new(&record.folder).join(WORKER_PANE_FILE);
            let text = std::fs::read_to_string(file).map_err(|_| "agent_pane_file")?;
            let pane = pane_in_file(&text, receiver).ok_or("agent_pane_file")?;
            return match checker.view.agent_pid(&pane) {
                Ok(Some(agent_pid)) => Ok(Resolved {
                    pane_id: pane,
                    agent_pid,
                }),
                Ok(None) => Err("pane_no_agent"),
                Err(ViewError) => Err("multiplexer_unreachable"),
            };
        }
        Ok(Resolved {
            pane_id: record.pane_id,
            agent_pid: record.agent_pid,
        })
    }

    fn running(&self, receiver: &Agent) -> Option<u32> {
        let (product, role, spec) = receiver.record_key();
        self.checker
            .roles
            .get_spec(&product, role, &spec)
            .map(|record| record.agent_pid)
    }

    fn socket(&self, pid: u32) -> SocketProbe {
        let Ok(text) = std::fs::read_to_string(self.checker.sessions.join(format!("{pid}.json")))
        else {
            return SocketProbe::Unknown;
        };
        let path = socket_path_in(&text);
        probe_socket(path.as_deref().map(Path::new))
    }

    fn pane_agent_pid(&self, pane: &str) -> Result<Option<u32>, ViewError> {
        self.checker.view.agent_pid(pane)
    }
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;
