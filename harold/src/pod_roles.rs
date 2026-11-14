//! The pod role record: which agent holds each pod role now (ADR for the spec
//! pod-role-record). A start hook of the agent reports each start through the RPC
//! `PodAgentStarted`. The newest agent for each (product, role) keeps the role. The record
//! is the file `pod-roles.json` in the pod tool's data folder.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::tools::job::process::ProcessPort;

/// The name of the record file.
pub(crate) const RECORD_FILE: &str = "pod-roles.json";

/// The product of the chief: the chief belongs to no product.
const CHIEF_PRODUCT: &str = "-";
/// The pod roles: the values of `role` on `pod.agents` that an agent holds for a pod.
const POD_ROLES: [&str; 3] = ["chief", "pnem", "pod_coordination"];
/// The role value of a build worker. A build worker has a spec id; a pod role has none.
pub(crate) const BUILD_WORKER: &str = "build_worker";
/// The longest spec id a record accepts.
const SPEC_LIMIT: usize = 64;
/// The longest product name a record accepts.
const PRODUCT_LIMIT: usize = 64;

/// One start that the hook reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PodStart {
    pub(crate) role: String,
    pub(crate) product: String,
    pub(crate) pane_id: String,
    pub(crate) session_id: String,
    pub(crate) transcript_path: String,
    pub(crate) folder: String,
    pub(crate) agent_pid: u32,
    pub(crate) start_file: String,
    /// The hook's `source`: `startup`, `resume`, `clear` or `compact`.
    pub(crate) source: String,
    /// The spec id of a build worker. Empty for a pod role.
    pub(crate) spec: String,
}

/// The agent that holds one pod role.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RoleRecord {
    pub(crate) pane_id: String,
    pub(crate) session_id: String,
    pub(crate) agent_pid: u32,
    /// The start time of `agent_pid` in seconds since the epoch, 0 when it was not known.
    pub(crate) agent_proc_start: i64,
    pub(crate) folder: String,
    pub(crate) transcript_path: String,
    pub(crate) start_file: String,
    pub(crate) started_at_ms: i64,
}

/// Why `record` wrote nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecordError {
    UnknownRole,
    BadProduct,
    /// A build worker with no spec id, or a spec id that is not valid.
    BadSpec,
    /// `agent_pid` is 0: no process has it.
    BadPid,
}

/// The records, kept in memory and in `pod-roles.json`.
pub(crate) struct PodRoles {
    path: PathBuf,
    processes: Arc<dyn ProcessPort>,
    records: Mutex<BTreeMap<String, RoleRecord>>,
}

/// The key of a record. A pod role has no spec id and keeps the key `product/role`.
fn key(product: &str, role: &str, spec: &str) -> String {
    if spec.is_empty() {
        format!("{product}/{role}")
    } else {
        format!("{product}/{role}/{spec}")
    }
}

impl PodRoles {
    /// Reads the record file in `folder`. A file that is missing or is not valid reads as no
    /// record.
    pub(crate) fn open(folder: &Path, processes: Arc<dyn ProcessPort>) -> Self {
        let path = folder.join(RECORD_FILE);
        let records = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Self {
            path,
            processes,
            records: Mutex::new(records),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, RoleRecord>> {
        self.records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The record of one role, or `None`.
    pub(crate) fn get(&self, product: &str, role: &str) -> Option<RoleRecord> {
        self.get_spec(product, role, "")
    }

    /// The record of one build worker (role `build_worker`), or `None`.
    pub(crate) fn get_spec(&self, product: &str, role: &str, spec: &str) -> Option<RoleRecord> {
        self.lock().get(&key(product, role, spec)).cloned()
    }

    /// Records one start and says whether it replaced an older agent. A `clear` or `compact`
    /// source only updates the session id of an existing record, not its transcript path. Logs `pod role recorded`
    /// for a full record.
    pub(crate) fn record(&self, start: PodStart, now_ms: i64) -> Result<bool, RecordError> {
        let worker = start.role == BUILD_WORKER;
        if !worker && !POD_ROLES.contains(&start.role.as_str()) {
            return Err(RecordError::UnknownRole);
        }
        if start.agent_pid == 0 {
            return Err(RecordError::BadPid);
        }
        let product = if start.role == "chief" {
            CHIEF_PRODUCT
        } else {
            valid_product(&start.product)?
        };
        let spec = if worker { valid_spec(&start.spec)? } else { "" };
        let mut records = self.lock();
        let slot = key(product, &start.role, spec);
        if matches!(start.source.as_str(), "clear" | "compact") {
            if let Some(existing) = records.get_mut(&slot) {
                existing.session_id = start.session_id;
                self.save(&records);
            }
            return Ok(false);
        }
        let replaced = records
            .get(&slot)
            .is_some_and(|existing| existing.agent_pid != start.agent_pid);
        info!(
            product = %product, role = %start.role, spec.id = %spec, pane_id = %start.pane_id,
            session.id = %start.session_id, replaced, "pod role recorded"
        );
        records.insert(
            slot,
            RoleRecord {
                agent_proc_start: self.processes.start_time(start.agent_pid).unwrap_or(0),
                pane_id: start.pane_id,
                session_id: start.session_id,
                agent_pid: start.agent_pid,
                folder: start.folder,
                transcript_path: start.transcript_path,
                start_file: start.start_file,
                started_at_ms: now_ms,
            },
        );
        self.save(&records);
        Ok(replaced)
    }

    /// Writes the file through a temporary file and a rename. A failed write is logged with a
    /// fixed reason; the record stays in memory.
    fn save(&self, records: &BTreeMap<String, RoleRecord>) {
        let temporary = self.path.with_extension("json.tmp");
        let written = serde_json::to_vec(records)
            .map_err(std::io::Error::other)
            .and_then(|bytes| std::fs::write(&temporary, bytes))
            .and_then(|()| std::fs::rename(&temporary, &self.path));
        if written.is_err() {
            warn!(reason = "write_failed", "pod role record not saved");
        }
    }
}

fn valid_spec(spec: &str) -> Result<&str, RecordError> {
    let valid = !spec.is_empty()
        && spec.len() <= SPEC_LIMIT
        && spec
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if valid {
        Ok(spec)
    } else {
        Err(RecordError::BadSpec)
    }
}

fn valid_product(product: &str) -> Result<&str, RecordError> {
    let valid = !product.is_empty()
        && product.len() <= PRODUCT_LIMIT
        && product
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if valid {
        Ok(product)
    } else {
        Err(RecordError::BadProduct)
    }
}

#[cfg(not(test))]
static INSTALLED: Mutex<Option<Arc<PodRoles>>> = Mutex::new(None);

#[cfg(test)]
thread_local! {
    static INSTALLED: std::cell::RefCell<Option<Arc<PodRoles>>> = const { std::cell::RefCell::new(None) };
}

/// Makes `roles` the record that `chief_pane` and the RPC handler use. The pod tool calls it
/// at its start. In a test the value belongs to the test's own thread.
pub(crate) fn install(roles: Arc<PodRoles>) {
    #[cfg(not(test))]
    {
        *INSTALLED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(roles);
    }
    #[cfg(test)]
    INSTALLED.with(|slot| *slot.borrow_mut() = Some(roles));
}

/// Clears the installed record. For tests.
#[cfg(test)]
pub(crate) fn reset() {
    INSTALLED.with(|slot| *slot.borrow_mut() = None);
}

fn installed() -> Option<Arc<PodRoles>> {
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

/// The pane of the chief in the record, or `None` when there is no record or its pane id is
/// empty.
pub(crate) fn chief_pane() -> Option<String> {
    let record = installed()?.get(CHIEF_PRODUCT, "chief")?;
    (!record.pane_id.is_empty()).then_some(record.pane_id)
}

/// Records one start in the installed record. `None` when the pod tool did not start.
pub(crate) fn record_start(start: PodStart, now_ms: i64) -> Option<Result<bool, RecordError>> {
    Some(installed()?.record(start, now_ms))
}

#[cfg(test)]
#[path = "pod_roles_tests.rs"]
mod tests;
