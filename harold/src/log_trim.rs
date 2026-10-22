//! Keeps `harold.log` to the last 24 hours, in place, when launchd started Harold.
//!
//! launchd opens the file in append mode for standard output and standard error, so every
//! writer, child programs included, writes at the file's end wherever that is. The trim
//! reads the file, writes the kept part at offset 0 and sets the length to it: the inode
//! stays, so `tail -F` keeps following. Harold's own writers are held from the read to the
//! set-length; nothing may be logged while they are held.

use std::fs::{File, OpenOptions};
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::os::fd::AsFd as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::watch;
use tokio::task::JoinHandle;

const KEEP: Duration = Duration::from_secs(24 * 3600);
const EVERY: Duration = Duration::from_secs(3600);
const LOG_NAME: &str = "harold.log";

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Trimmed {
    Nothing,
    Dropped { kept_bytes: u64, dropped_bytes: u64 },
}

pub(crate) type Outcome = Result<Trimmed, Failure>;

#[derive(Debug)]
pub(crate) struct Failure {
    /// `read_failed`, `write_failed` or `not_the_log`.
    pub(crate) reason: &'static str,
    pub(crate) error: String,
}

impl Failure {
    fn new(reason: &'static str, error: impl std::fmt::Display) -> Self {
        Self {
            reason,
            error: error.to_string(),
        }
    }
}

/// Trims at once and then every hour, until `shutdown` closes.
pub(crate) fn start(mut shutdown: watch::Receiver<()>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut every = tokio::time::interval(EVERY);
        every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                _ = shutdown.changed() => break,
                _ = every.tick() => {}
            }
            let Ok(outcome) = tokio::task::spawn_blocking(trim_own_log).await else {
                continue;
            };
            // The writers are free again here, so logging is safe.
            match outcome {
                Ok(Trimmed::Nothing) => {}
                Ok(Trimmed::Dropped {
                    kept_bytes,
                    dropped_bytes,
                }) => tracing::info!(kept_bytes, dropped_bytes, "harold.log trimmed"),
                Err(failure) => tracing::warn!(
                    reason = failure.reason,
                    error = %failure.error,
                    "harold.log not trimmed"
                ),
            }
        }
    })
}

fn trim_own_log() -> Outcome {
    let path: PathBuf = std::env::current_exe()
        .map_err(|error| Failure::new("not_the_log", error))?
        .parent()
        .ok_or_else(|| Failure::new("not_the_log", "the program has no folder"))?
        .join(LOG_NAME);
    // The open handle, not `/dev/fd/1`: by path, macOS reports another device number.
    let standard_output = File::from(
        std::io::stdout()
            .as_fd()
            .try_clone_to_owned()
            .map_err(|error| Failure::new("not_the_log", error))?,
    );
    trim(&path, &standard_output, SystemTime::now(), |run| {
        let _stdout = std::io::stdout().lock();
        let _stderr = std::io::stderr().lock();
        run()
    })
}

/// Drops the lines of `path` older than 24 hours before `now`, when `path` is the same
/// file as the open handle `same_as`. `hold` runs the trim's read-to-set-length step with the writers held.
pub(crate) fn trim(
    path: &Path,
    same_as: &File,
    now: SystemTime,
    hold: impl FnOnce(&mut dyn FnMut() -> Outcome) -> Outcome,
) -> Outcome {
    let on_disk = std::fs::metadata(path).map_err(|error| Failure::new("not_the_log", error))?;
    let open = same_as
        .metadata()
        .map_err(|error| Failure::new("not_the_log", error))?;
    if (on_disk.dev(), on_disk.ino()) != (open.dev(), open.ino()) {
        return Err(Failure::new(
            "not_the_log",
            "the path is not standard output's file",
        ));
    }
    let cutoff = now
        .checked_sub(KEEP)
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |since| since.as_secs());
    hold(&mut || rewrite(path, cutoff))
}

fn rewrite(path: &Path, cutoff: u64) -> Outcome {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|error| Failure::new("write_failed", error))?;
    let mut contents = Vec::new();
    file.read_to_end(&mut contents)
        .map_err(|error| Failure::new("read_failed", error))?;
    let cut = cut_offset(&contents, cutoff);
    if cut == 0 {
        return Ok(Trimmed::Nothing);
    }
    let kept = &contents[cut..];
    let write = |file: &mut std::fs::File| {
        file.seek(SeekFrom::Start(0))?;
        file.write_all(kept)?;
        file.set_len(kept.len() as u64)
    };
    write(&mut file).map_err(|error| Failure::new("write_failed", error))?;
    Ok(Trimmed::Dropped {
        kept_bytes: kept.len() as u64,
        dropped_bytes: cut as u64,
    })
}

/// The offset of the first timed line at or after `cutoff` (Unix seconds); 0 when nothing
/// is to be dropped. A line without a time belongs to the timed line before it. A file with
/// no such line is left whole.
fn cut_offset(contents: &[u8], cutoff: u64) -> usize {
    let mut offset = 0;
    for line in contents.split_inclusive(|&byte| byte == b'\n') {
        if line_time(line).is_some_and(|time| time >= cutoff) {
            return offset;
        }
        offset += line.len();
    }
    0
}

/// The Unix second a line starts with (`2026-10-11T10:44:57.069324Z`), after ANSI codes.
fn line_time(line: &[u8]) -> Option<u64> {
    let mut rest = line;
    while let [0x1b, b'[', tail @ ..] = rest {
        let end = tail.iter().position(|byte| (0x40..=0x7e).contains(byte))?;
        rest = &tail[end + 1..];
    }
    // The time ends at its `Z`; colour codes or a space may follow it.
    let end = rest.iter().position(|&byte| byte == b'Z')?;
    let text = std::str::from_utf8(&rest[..end]).ok()?;
    let (date_time, fraction) = text.split_once('.').unwrap_or((text, ""));
    if !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let (date, time) = date_time.split_once('T')?;
    let number = |part: &str, width: usize| {
        (part.len() == width && part.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| part.parse::<i64>().ok())
            .flatten()
    };
    let mut date = date.split('-');
    let (year, month, day) = (
        number(date.next()?, 4)?,
        number(date.next()?, 2)?,
        number(date.next()?, 2)?,
    );
    let mut time = time.split(':');
    let (hour, minute, second) = (
        number(time.next()?, 2)?,
        number(time.next()?, 2)?,
        number(time.next()?, 2)?,
    );
    if date.next().is_some() || time.next().is_some() {
        return None;
    }
    let seconds = days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second;
    u64::try_from(seconds).ok()
}

/// Days from 1970-01-01 to the given proleptic Gregorian date.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let shifted_month = (month + 9) % 12;
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
#[path = "log_trim_tests.rs"]
mod tests;
