//! Counts starts that follow an unclean exit, so launchd's `KeepAlive` cannot restart a
//! crashing Harold for ever.
//!
//! The crash file sits beside the store. Its first line is `clean`, `running` or `gave-up`;
//! each later line is the Unix second of a recent start that followed an unclean exit. A
//! start finds `running` when the run before it did not shut down cleanly. `haroldctl start`
//! deletes the file, and `haroldctl ensure` leaves Harold off while it says `gave-up`.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Unclean starts within [`WINDOW`] that make Harold give up.
pub(crate) const LIMIT: usize = 3;
pub(crate) const WINDOW: Duration = Duration::from_secs(600);

const CLEAN: &str = "clean";
const RUNNING: &str = "running";
const GAVE_UP: &str = "gave-up";

/// The crash file for the store at `store_path`: `<bundle>/data/crashes`.
pub(crate) fn crash_file(store_path: &Path) -> PathBuf {
    store_path
        .parent()
        .unwrap_or(Path::new("."))
        .join("crashes")
}

pub(crate) enum Start {
    Run(CrashGuard),
    /// Harold gave up after a crash loop, now or at an earlier start.
    Refuse,
}

/// Held by a running Harold; [`CrashGuard::mark_clean`] records a clean shutdown.
pub(crate) struct CrashGuard {
    file: PathBuf,
}

impl CrashGuard {
    /// Marks the run clean, which also forgets the unclean starts before it.
    pub(crate) fn mark_clean(self) -> std::io::Result<()> {
        write(&self.file, CLEAN, &[])
    }
}

/// Reads the crash file at a start, counts this start if the last run did not exit cleanly,
/// and marks this run as running. A missing or unreadable-as-text file counts as clean; lines
/// that are not times, or are outside the last [`WINDOW`], are dropped.
pub(crate) fn begin(file: &Path, now: SystemTime) -> std::io::Result<Start> {
    let contents = match std::fs::read(file) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error),
    };
    let mut lines = contents.lines();
    let state = lines.next().map(str::trim).unwrap_or(CLEAN);
    if state == GAVE_UP {
        return Ok(Start::Refuse);
    }
    let now = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let since = now.saturating_sub(WINDOW.as_secs());
    let mut times: Vec<u64> = lines
        .filter_map(|line| line.trim().parse().ok())
        .filter(|time| (since..=now).contains(time))
        .collect();
    if state == RUNNING {
        times.push(now);
        if times.len() >= LIMIT {
            write(file, GAVE_UP, &times)?;
            return Ok(Start::Refuse);
        }
    }
    write(file, RUNNING, &times)?;
    Ok(Start::Run(CrashGuard {
        file: file.to_path_buf(),
    }))
}

/// Writes the file whole: to a sibling first, then renamed over it.
fn write(file: &Path, state: &str, times: &[u64]) -> std::io::Result<()> {
    let mut contents = format!("{state}\n");
    for time in times {
        contents.push_str(&format!("{time}\n"));
    }
    let pending = file.with_extension("pending");
    let mut output = std::fs::File::create(&pending)?;
    output.write_all(contents.as_bytes())?;
    output.sync_all()?;
    std::fs::rename(&pending, file)
}

#[cfg(test)]
#[path = "crash_guard_tests.rs"]
mod tests;
