//! The restart receipt file: `restarts/<role>-<YYYYMMDD-HHMM>.md` in the pod folder.

use std::io;
use std::path::{Path, PathBuf};

use super::Agent;

/// The path of the receipt. `folder` is the pod folder, and for the chief the chief folder.
/// The chief's receipt goes in `.superpowers/pod/restarts/` of that folder.
pub(crate) fn receipt_path(folder: &Path, agent: &Agent, stamp: &str) -> PathBuf {
    let (directory, name) = match agent {
        Agent::Chief => (
            folder.join(".superpowers/pod/restarts"),
            format!("chief-{stamp}.md"),
        ),
        Agent::Pnem(_) => (folder.join("restarts"), format!("pnem-{stamp}.md")),
        Agent::PodCoordination(_) => (
            folder.join("restarts"),
            format!("pod_coordination-{stamp}.md"),
        ),
        Agent::BuildWorker { spec, .. } => {
            let part: String = spec
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
                .collect();
            (folder.join("restarts"), format!("worker-{part}-{stamp}.md"))
        }
    };
    directory.join(name)
}

/// Writes the text through a temporary file and a rename. A second run in the same minute
/// replaces the file.
pub(crate) fn write_receipt(path: &Path, text: &str) -> io::Result<()> {
    if let Some(directory) = path.parent() {
        std::fs::create_dir_all(directory)?;
    }
    let temporary = path.with_extension("md.tmp");
    std::fs::write(&temporary, text)?;
    std::fs::rename(&temporary, path)
}

#[cfg(test)]
#[path = "receipt_tests.rs"]
mod tests;
