//! The agent's own session file, `<folder>/<pid>.json`. It is the agent program's file, not
//! Harold's (ADR 0004: a tool may read an agent's files). Harold reads only `pid`,
//! `procStart` and `name`. Any other shape is an error, never a wrong name.

use std::path::PathBuf;

use serde_json::Value;

/// The folder of the session files, under the agent program's configuration folder. The one
/// place that names it.
const SESSION_FOLDER: &str = ".claude/sessions";

/// The session files could not give a name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Unreadable;

/// Reads the session files of one folder.
pub(super) struct SessionFiles {
    folder: PathBuf,
}

/// The default folder: `SESSION_FOLDER` in the home folder.
pub(super) fn default_folder() -> PathBuf {
    std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(SESSION_FOLDER))
        .unwrap_or_default()
}

impl SessionFiles {
    pub(super) fn new(folder: PathBuf) -> Self {
        Self { folder }
    }

    /// The session name of the agent with process id `pid`. The file must hold `pid` (equal
    /// to `pid`), `procStart` (a string) and `name` (a string that is not empty).
    pub(super) fn name(&self, pid: u32) -> Result<String, Unreadable> {
        let bytes =
            std::fs::read(self.folder.join(format!("{pid}.json"))).map_err(|_| Unreadable)?;
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| Unreadable)?;
        let fields = value.as_object().ok_or(Unreadable)?;
        let same_pid = fields.get("pid").and_then(Value::as_u64) == Some(u64::from(pid));
        let started = fields.get("procStart").is_some_and(Value::is_string);
        let name = fields
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty());
        match (same_pid && started, name) {
            (true, Some(name)) => Ok(name.to_owned()),
            _ => Err(Unreadable),
        }
    }
}
