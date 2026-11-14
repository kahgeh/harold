//! The restart check (spec agent-restart). After a restart Harold checks that the agent is
//! whole and writes the result to the restart receipt file. Stage 1 checks identity and
//! communication.

mod communication;
mod identity;
mod real;
mod receipt;
mod render;
mod runtime;

pub(crate) use communication::{
    LinkPorts, Resolved, Roster, SocketProbe, check_links, plan_links, probe_socket, socket_path_in,
};
pub(crate) use identity::{
    IdentityInput, PaneView, ViewError, check_identity, pane_in_file, read_file_claim,
};
pub(crate) use real::{CheckTurnEnds, GrpcPhone, InventoryView, PaneDeliver, SystemClock};
pub(crate) use receipt::{receipt_path, write_receipt};
pub(crate) use render::{clean_name, render};
pub(crate) use runtime::{Checker, Deliver, TimeSource};

/// The agent that was checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Agent {
    Chief,
    /// The PnEM of a product.
    Pnem(String),
    /// The pod-coordination of a product.
    PodCoordination(String),
    BuildWorker {
        product: String,
        spec: String,
    },
}

impl Agent {
    /// The key of the agent in the pod role record: product, role and spec id. The chief's
    /// product is `-`.
    pub(crate) fn record_key(&self) -> (String, &'static str, String) {
        match self {
            Self::Chief => ("-".into(), "chief", String::new()),
            Self::Pnem(product) => (product.clone(), "pnem", String::new()),
            Self::PodCoordination(product) => (product.clone(), "pod_coordination", String::new()),
            Self::BuildWorker { product, spec } => (
                product.clone(),
                crate::pod_roles::BUILD_WORKER,
                spec.clone(),
            ),
        }
    }
}

impl Agent {
    /// The agent of a role value, a product and a spec id. `None` for a role that is not one
    /// of the four.
    pub(crate) fn from_role(role: &str, product: &str, spec: &str) -> Option<Self> {
        match role {
            "chief" => Some(Self::Chief),
            "pnem" => Some(Self::Pnem(product.to_owned())),
            "pod_coordination" => Some(Self::PodCoordination(product.to_owned())),
            crate::pod_roles::BUILD_WORKER => Some(Self::BuildWorker {
                product: product.to_owned(),
                spec: spec.to_owned(),
            }),
            _ => None,
        }
    }
}

/// Who gets each FAIL line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Recipient {
    Chief,
    PodCoordination,
}

impl Agent {
    /// The chief gets the FAIL of the chief, a PnEM and a pod-coordination. Pod-coordination
    /// gets the FAIL of its build workers.
    pub(crate) fn recipient(&self) -> Recipient {
        match self {
            Self::BuildWorker { .. } => Recipient::PodCoordination,
            _ => Recipient::Chief,
        }
    }

    /// The name of the agent in the first line of the output. The word is the one that the
    /// user agreed in the mocks.
    pub(crate) fn title(&self) -> String {
        match self {
            Self::Chief => "chief".into(),
            Self::Pnem(product) => format!("pnem {product}"),
            Self::PodCoordination(product) => format!("pod-coordination {product}"),
            Self::BuildWorker { spec, .. } => format!("worker {spec}"),
        }
    }
}

/// The result of one check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    Ok,
    /// A mismatch that Harold corrected, and the second check passed.
    Fixed,
    Fail,
    /// Harold cannot make the check. The line gives the reason.
    NotChecked,
}

/// The groups of stage 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Group {
    Identity,
    Communication,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IdentityRow {
    pub(crate) outcome: Outcome,
    pub(crate) subject: String,
    pub(crate) detail: String,
}

/// One row of the communication table: one agent, one result for each direction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LinkRow {
    /// The cells before the two direction columns.
    pub(crate) label: Vec<String>,
    pub(crate) outgoing: Outcome,
    pub(crate) incoming: Outcome,
    /// The reason word of each direction, empty for `ok`.
    pub(crate) outgoing_reason: &'static str,
    pub(crate) incoming_reason: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LinkTable {
    /// One cell for each label column, then the two direction names.
    pub(crate) header: Vec<String>,
    pub(crate) rows: Vec<LinkRow>,
}

/// One line under a table: what failed and who fixes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FailLine {
    pub(crate) group: Group,
    pub(crate) text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Report {
    pub(crate) agent: Agent,
    /// The Rex session label.
    pub(crate) session: String,
    /// The short pane id.
    pub(crate) pane: String,
    /// The local time, `YYYY-MM-DD HH:MM`.
    pub(crate) time: String,
    pub(crate) identity: IdentityRow,
    pub(crate) communication: LinkTable,
    pub(crate) fails: Vec<FailLine>,
    /// One line for each check that Harold cannot make, with its reason.
    pub(crate) not_checked: Vec<String>,
}

use std::sync::Arc;
#[cfg(not(test))]
use std::sync::Mutex;

#[cfg(not(test))]
static INSTALLED: Mutex<Option<Arc<Checker>>> = Mutex::new(None);

#[cfg(test)]
thread_local! {
    static INSTALLED: std::cell::RefCell<Option<Arc<Checker>>> = const { std::cell::RefCell::new(None) };
}

/// Makes `checker` the check that the RPC `Check`, the role start and the turn end use.
pub(crate) fn install(checker: Arc<Checker>) {
    #[cfg(not(test))]
    {
        *INSTALLED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(checker);
    }
    #[cfg(test)]
    INSTALLED.with(|slot| *slot.borrow_mut() = Some(checker));
}

/// The installed check, or `None` when Harold did not start it.
pub(crate) fn installed() -> Option<Arc<Checker>> {
    #[cfg(not(test))]
    {
        INSTALLED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    #[cfg(test)]
    INSTALLED.with(|slot| slot.borrow().clone())
}

/// The session id that the record holds for a role now, before a new start replaces it.
pub(crate) fn session_before(role: &str, product: &str, spec: &str) -> Option<String> {
    let product = if role == "chief" { "-" } else { product };
    installed()?.session_of(product, role, spec)
}

/// Tells the check about a role start. See `Checker::note_start`.
pub(crate) fn note_role_start(
    role: &str,
    product: &str,
    spec: &str,
    before: Option<String>,
    new_session: &str,
    agent_pid: u32,
    source: &str,
) {
    let (Some(checker), Some(agent)) = (installed(), Agent::from_role(role, product, spec)) else {
        return;
    };
    checker.note_start(&agent, before, new_session, agent_pid, source);
}
