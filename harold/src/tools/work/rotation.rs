//! The rotation: Harold moves the open work records to a new work file every
//! `rotation_days` days, after a count check. The rotation does no logging and
//! no telemetry. The caller logs from the `Outcome`.
//!
//! The `Rotator` holds the one writable handle, to the current file. Writes go
//! through `Rotator::current`, and a rotation locks that handle, so no write
//! reaches the old file during or after the copy.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::{RwLock, RwLockReadGuard};

use super::files::{WorkFiles, day_of};
use super::schema::Record;
use super::store::{Counts, StoreError, WorkFile};

/// The time source. The tests use a fake clock, so they do not sleep.
pub(super) trait Clock: Send + Sync {
    fn now_ms(&self) -> i64;
}

/// The real clock.
pub(super) struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        let nanos = time::OffsetDateTime::now_utc().unix_timestamp_nanos();
        i64::try_from(nanos / 1_000_000).unwrap_or(i64::MAX)
    }
}

/// A test seam: it gets each open record before the copy, and the copy
/// stores what it returns. `None` drops the record.
pub(super) type CopyHook = Box<dyn Fn(Record) -> Option<Record> + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FailReason {
    /// The new file does not hold the same counts as the old file.
    CountMismatch,
    /// A read of the old file or a write of the new file failed.
    WriteFailed,
    /// The rename of the new file failed.
    RenameFailed,
}

impl FailReason {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            FailReason::CountMismatch => "count_mismatch",
            FailReason::WriteFailed => "write_failed",
            FailReason::RenameFailed => "rename_failed",
        }
    }
}

/// What one `tick` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Outcome {
    /// The rotation is off: `rotation_days` is 0.
    Off,
    /// The current file is not old enough, or there is no work file.
    NotDue,
    /// The open records moved from `file` to `new_file`.
    Done {
        file: String,
        new_file: String,
        counts: Counts,
    },
    /// The rotation failed. `file` stays current and unchanged. `first` is true
    /// only for the first failure of `file`. `since_ms` is the time of that
    /// first failure.
    Failed {
        file: String,
        reason: FailReason,
        first: bool,
        since_ms: i64,
    },
}

/// What a rotation would copy now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DryRun {
    pub(super) counts: Counts,
    /// The names of the open records.
    pub(super) records: Vec<String>,
    /// The name of the new file.
    pub(super) target: String,
}

pub(super) struct Rotator {
    files: WorkFiles,
    clock: Arc<dyn Clock>,
    /// The writable handle to the current file, opened at first use.
    current: RwLock<Option<WorkFile>>,
    /// The file that fails to rotate, and when its failure began. A restart
    /// forgets it.
    failure: std::sync::Mutex<Option<(String, i64)>>,
    copy_hook: std::sync::Mutex<Option<CopyHook>>,
}

impl Rotator {
    pub(super) fn new(files: WorkFiles, clock: Arc<dyn Clock>) -> Self {
        Self {
            files,
            clock,
            current: RwLock::new(None),
            failure: std::sync::Mutex::new(None),
            copy_hook: std::sync::Mutex::new(None),
        }
    }

    #[cfg(test)]
    pub(super) fn set_copy_hook(&self, hook: Option<CopyHook>) {
        *self.lock_hook() = hook;
    }

    /// The writable current file. With no work file, it makes the file of
    /// today. Hold the guard for the whole write: a rotation waits for it.
    pub(super) async fn current(&self) -> Result<RwLockReadGuard<'_, WorkFile>, StoreError> {
        let held = self.current.read().await;
        if let Ok(file) = RwLockReadGuard::try_map(held, Option::as_ref) {
            return Ok(file);
        }
        let mut held = self.current.write().await;
        if held.is_none() {
            let path = match self.files.current_name() {
                Some(name) => self.files.folder().join(name),
                None => self.files.file_path(&day_of(self.clock.now_ms())),
            };
            *held = Some(WorkFile::open(&path).await?);
        }
        RwLockReadGuard::try_map(held.downgrade(), Option::as_ref)
            .map_err(|_| StoreError::Data("no current work file".into()))
    }

    /// Opens a work file for reads. Every write to it is refused. Writes go
    /// through `current`.
    pub(super) async fn read_file(&self, name: &str) -> Result<WorkFile, StoreError> {
        WorkFile::open_read_only(&self.files.folder().join(name)).await
    }

    /// Rotates the current file when it is `rotation_days` days old or older.
    /// `0` turns the rotation off.
    pub(super) async fn tick(&self, rotation_days: u32) -> Outcome {
        if rotation_days == 0 {
            // No more retries, so a failure that began before ends here.
            *self.lock_failure() = None;
            return Outcome::Off;
        }
        let Some(file) = self.files.current_name() else {
            return Outcome::NotDue;
        };
        let today = day_of(self.clock.now_ms());
        let (Some(start), Some(now)) = (start_date(&file), date_of(&today)) else {
            return Outcome::NotDue;
        };
        if (now - start).whole_days() < i64::from(rotation_days) {
            return Outcome::NotDue;
        }
        self.rotate(&file, &today).await
    }

    /// What a rotation would copy now. It writes nothing.
    pub(super) async fn dry_run(&self) -> Result<DryRun, StoreError> {
        let target = file_name_of(&self.files.file_path(&day_of(self.clock.now_ms())));
        let Some(file) = self.files.current_name() else {
            return Ok(DryRun {
                counts: Counts::default(),
                records: Vec::new(),
                target,
            });
        };
        let held = self.current.read().await;
        let opened;
        let source = match held.as_ref().filter(|f| f.file_name() == file) {
            Some(source) => source,
            None => {
                opened = self.read_file(&file).await?;
                &opened
            }
        };
        let records = source
            .open_records()
            .await?
            .into_iter()
            .map(|record| record.name)
            .collect();
        Ok(DryRun {
            counts: source.counts().await?,
            records,
            target,
        })
    }

    /// Moves the open records of `file` to the file of `target_day`. A target
    /// that is not newer than `file` is refused with `NotDue`, and nothing
    /// changes.
    async fn rotate(&self, file: &str, target_day: &str) -> Outcome {
        let target = self.files.file_path(target_day);
        let new_file = file_name_of(&target);
        if new_file.as_str() <= file {
            return Outcome::NotDue;
        }
        let mut held = self.current.write().await;
        // A rotation that ended while this one waited for the lock made a new
        // current file. Then `file` is old, and there is nothing to do.
        if self.files.current_name().as_deref() != Some(file) {
            return Outcome::NotDue;
        }
        if held.as_ref().is_some_and(|f| f.file_name() != file) {
            *held = None;
        }
        match self.copy(held.as_ref(), file, &target).await {
            Ok(counts) => {
                // Later writes open the new file. The old handle closes, and
                // its log goes into the file.
                if let Some(old) = held.take() {
                    let _ = old.close().await;
                }
                *self.lock_failure() = None;
                Outcome::Done {
                    file: file.to_string(),
                    new_file,
                    counts,
                }
            }
            Err(reason) => self.fail(file, reason),
        }
    }

    /// Writes the open records to the `.tmp` file, compares the counts, and
    /// renames it to `target` only when they match. The old file is only read.
    async fn copy(
        &self,
        held: Option<&WorkFile>,
        file: &str,
        target: &Path,
    ) -> Result<Counts, FailReason> {
        let opened;
        let old = match held {
            Some(old) => old,
            None => {
                opened = self
                    .read_file(file)
                    .await
                    .map_err(|_| FailReason::WriteFailed)?;
                &opened
            }
        };
        let records = old
            .open_records()
            .await
            .map_err(|_| FailReason::WriteFailed)?;
        let old_counts = old.counts().await.map_err(|_| FailReason::WriteFailed)?;
        let records = self.through_hook(records);

        let tmp = with_suffix(target, ".tmp");
        remove_tmp(&tmp);
        let checked = async {
            write_copy(&tmp, old, &records)
                .await
                .map_err(|_| FailReason::WriteFailed)?;
            let new_counts = count_closed(&tmp)
                .await
                .map_err(|_| FailReason::WriteFailed)?;
            if new_counts != old_counts {
                return Err(FailReason::CountMismatch);
            }
            if target.exists() || std::fs::rename(&tmp, target).is_err() {
                return Err(FailReason::RenameFailed);
            }
            Ok(new_counts)
        }
        .await;
        remove_tmp(&tmp);
        checked
    }

    fn through_hook(&self, records: Vec<Record>) -> Vec<Record> {
        let hook = self.lock_hook();
        match hook.as_ref() {
            Some(hook) => records.into_iter().filter_map(hook).collect(),
            None => records,
        }
    }

    fn fail(&self, file: &str, reason: FailReason) -> Outcome {
        let now = self.clock.now_ms();
        let mut failure = self.lock_failure();
        let (first, since_ms) = match failure.as_ref() {
            Some((name, since)) if name == file => (false, *since),
            _ => {
                *failure = Some((file.to_string(), now));
                (true, now)
            }
        };
        Outcome::Failed {
            file: file.to_string(),
            reason,
            first,
            since_ms,
        }
    }

    fn lock_failure(&self) -> std::sync::MutexGuard<'_, Option<(String, i64)>> {
        self.failure.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lock_hook(&self) -> std::sync::MutexGuard<'_, Option<CopyHook>> {
        self.copy_hook.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Writes the records, with their moves from `old`, to a new file at `tmp`,
/// then closes it, so that the file alone holds every row.
async fn write_copy(tmp: &Path, old: &WorkFile, records: &[Record]) -> Result<(), StoreError> {
    let new = WorkFile::open(tmp).await?;
    for record in records {
        let moves = old.moves(&record.name).await?;
        new.insert_copy(record, &moves)
            .await?
            .map_err(|refusal| StoreError::Data(refusal.reason().into()))?;
    }
    new.close().await
}

/// The counts of the closed file at `tmp`, read from a new open. A log file
/// that still holds rows is an error, because the rename does not move it.
async fn count_closed(tmp: &Path) -> Result<Counts, StoreError> {
    let counts = WorkFile::open_read_only(tmp).await?.counts().await?;
    let wal = with_suffix(tmp, "-wal");
    if std::fs::metadata(&wal).is_ok_and(|m| m.len() > 0) {
        return Err(StoreError::Data(format!("{} is not empty", wal.display())));
    }
    Ok(counts)
}

/// Removes the `.tmp` file and its log files, if they are there.
fn remove_tmp(tmp: &Path) {
    for path in [
        tmp.to_path_buf(),
        with_suffix(tmp, "-wal"),
        with_suffix(tmp, "-shm"),
    ] {
        let _ = std::fs::remove_file(path);
    }
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut text = path.as_os_str().to_os_string();
    text.push(suffix);
    PathBuf::from(text)
}

fn file_name_of(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string()
}

/// The start day of a work file, from its name `work-YYYY-MM-DD.db`.
fn start_date(file: &str) -> Option<time::Date> {
    date_of(file.strip_prefix("work-")?.strip_suffix(".db")?)
}

/// The date of `YYYY-MM-DD`.
fn date_of(day: &str) -> Option<time::Date> {
    let mut parts = day.split('-');
    let year: i32 = parts.next()?.parse().ok()?;
    let month: u8 = parts.next()?.parse().ok()?;
    let date: u8 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    let month = time::Month::try_from(month).ok()?;
    time::Date::from_calendar_date(year, month, date).ok()
}

#[cfg(test)]
#[path = "rotation_tests.rs"]
mod tests;
