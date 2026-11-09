//! The keep-awake files. While a job of this Harold runs, Harold writes the file
//! `harold-job-<name>` in the folder that `[job] keep_awake_folder` names, and writes it
//! again every `keep_awake_every_s` seconds. The idle-suspend of the machine reads the
//! folder and does not suspend while a file in it is fresh. The folder belongs to the
//! idle-suspend: Harold writes and deletes only its own `harold-job-*` files in it.

use std::collections::{HashMap, HashSet};
use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::Mutex;

use super::obs;
use crate::settings::JobSettings;

/// The start of the name of every file that Harold writes.
const PREFIX: &str = "harold-job-";

/// The shortest and the longest time between two writes, in seconds. The idle-suspend
/// window is 45 minutes, so the interval stays well inside it.
const EVERY_MIN_S: u64 = 10;
const EVERY_MAX_S: u64 = 20 * 60;

#[derive(Default)]
struct Written {
    /// The jobs that have their file now, with the time of the last write.
    at_ms: HashMap<String, i64>,
    /// The jobs whose write failed: the failure is logged once for each job.
    failed: HashSet<String>,
}

pub(crate) struct KeepAwake {
    folder: PathBuf,
    /// The folder as the setting names it. Logs show this text, not the expanded path.
    folder_text: String,
    every_ms: i64,
    written: Mutex<Written>,
}

/// The reason word for a failed write: an error kind, never a path or message text.
fn reason_of(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::NotFound => "not_found",
        ErrorKind::PermissionDenied => "permission_denied",
        _ => "other",
    }
}

impl KeepAwake {
    /// `None` when the setting names no folder: Harold writes nothing then.
    pub(crate) fn new(settings: &JobSettings) -> Option<Self> {
        let path = settings.keep_awake_path()?;
        let every_s = settings.keep_awake_every_s.clamp(EVERY_MIN_S, EVERY_MAX_S);
        Some(Self {
            folder: path,
            folder_text: settings.keep_awake_folder.trim().to_string(),
            every_ms: i64::try_from(every_s * 1_000).unwrap_or(i64::MAX),
            written: Mutex::new(Written::default()),
        })
    }

    fn file(&self, name: &str) -> PathBuf {
        self.folder.join(format!("{PREFIX}{name}"))
    }

    fn written(&self) -> std::sync::MutexGuard<'_, Written> {
        self.written
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Whether the job has its file now.
    pub(crate) fn has_file(&self, name: &str) -> bool {
        self.written().at_ms.contains_key(name)
    }

    /// Writes the file of a job that was just added, when it has none.
    pub(crate) fn begin(&self, name: &str, now_ms: i64) {
        if !self.has_file(name) {
            self.write(name, now_ms);
        }
    }

    /// Writes the file of each running job at once, and again when `keep_awake_every_s`
    /// has passed. Deletes the file of each tracked job that is not in `running`.
    pub(crate) fn tick(&self, running: &[String], now_ms: i64) {
        let gone: Vec<String> = self
            .written()
            .at_ms
            .keys()
            .filter(|name| !running.contains(name))
            .cloned()
            .collect();
        for name in gone {
            self.clear(&name);
        }
        for name in running {
            let due = {
                let written = self.written();
                written
                    .at_ms
                    .get(name)
                    .is_none_or(|last| now_ms - last >= self.every_ms)
            };
            if due {
                self.write(name, now_ms);
            }
        }
    }

    fn write(&self, name: &str, now_ms: i64) {
        let first = !self.has_file(name);
        match std::fs::write(self.file(name), now_ms.to_string()) {
            Ok(()) => {
                self.written().at_ms.insert(name.to_string(), now_ms);
                if first {
                    obs::keeps_machine_awake(name, &self.folder_text);
                }
            }
            Err(error) => {
                let first_failure = self.written().failed.insert(name.to_string());
                if first_failure {
                    obs::keep_awake_failed(name, &self.folder_text, reason_of(error.kind()));
                }
            }
        }
    }

    /// Deletes the file of a job that ended or was removed.
    pub(crate) fn clear(&self, name: &str) {
        let mut written = self.written();
        written.at_ms.remove(name);
        written.failed.remove(name);
        drop(written);
        // The file may not exist: a write failed, or another Harold run left no file.
        let _ = std::fs::remove_file(self.file(name));
    }

    /// The start sweep, for the result of reading the running jobs. When the read failed
    /// the list of running jobs is not known, so the sweep does nothing.
    pub(crate) fn sweep_if_read<E>(&self, running: Result<Vec<String>, E>) {
        if let Ok(names) = running {
            self.sweep(&names);
        }
    }

    /// At start: deletes each `harold-job-*` file that names no running job. It leaves
    /// every other file and every folder alone. A link is deleted, never followed.
    pub(crate) fn sweep(&self, running: &[String]) {
        let Ok(entries) = std::fs::read_dir(&self.folder) else {
            return;
        };
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let Some(job) = file_name.to_str().and_then(|n| n.strip_prefix(PREFIX)) else {
                continue;
            };
            let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
            if is_dir || running.iter().any(|name| name == job) {
                continue;
            }
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
#[path = "awake_tests.rs"]
mod tests;
