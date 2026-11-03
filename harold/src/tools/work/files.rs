//! The folder of work files, and the reads across them.

use std::path::{Path, PathBuf};

use super::schema::Record;
use super::store::{Move, StoreError, WorkFile};

const PREFIX: &str = "work-";
const SUFFIX: &str = ".db";

/// The work files in one folder. The newest name is the current file.
#[derive(Debug, Clone)]
pub(super) struct WorkFiles {
    folder: PathBuf,
}

impl WorkFiles {
    pub(super) fn new(folder: PathBuf) -> Self {
        Self { folder }
    }

    pub(super) fn folder(&self) -> &Path {
        &self.folder
    }

    /// The name of the newest work file. Temporary and other files do not count.
    pub(super) fn current_name(&self) -> Option<String> {
        self.names_newest_first().into_iter().next()
    }

    /// The path of the work file that starts on `day` (`YYYY-MM-DD`).
    pub(super) fn file_path(&self, day: &str) -> PathBuf {
        self.folder.join(format!("{PREFIX}{day}{SUFFIX}"))
    }

    /// The paths of all work files, newest first.
    pub(super) fn list_newest_first(&self) -> Vec<PathBuf> {
        self.names_newest_first()
            .into_iter()
            .map(|name| self.folder.join(name))
            .collect()
    }

    /// The record of `name` from the first file that holds it, with that
    /// file's name and its moves, oldest first. The files are read newest
    /// first.
    pub(super) async fn history(
        &self,
        name: &str,
    ) -> Result<Option<(String, Record, Vec<Move>)>, StoreError> {
        for path in self.list_newest_first() {
            let file = WorkFile::open_read_only(&path).await?;
            if let Some(record) = file.get(name).await? {
                let moves = file.moves(name).await?;
                return Ok(Some((file_name_of(&path), record, moves)));
            }
        }
        Ok(None)
    }

    /// The records that hold `word` in the name, the title, the next step or a
    /// decision text. The match is plain text and ignores case. The files are
    /// read newest first, and the first file that holds a name gives its record,
    /// even when an older version of it does not match.
    pub(super) async fn search(&self, word: &str) -> Result<Vec<(String, Record)>, StoreError> {
        let needle = word.to_lowercase();
        let mut seen = std::collections::HashSet::new();
        let mut found = Vec::new();
        for path in self.list_newest_first() {
            let file = WorkFile::open_read_only(&path).await?;
            let file_name = file_name_of(&path);
            for record in file.list(true).await? {
                if seen.insert(record.name.clone()) && matches(&record, &needle) {
                    found.push((file_name.clone(), record));
                }
            }
        }
        Ok(found)
    }

    fn names_newest_first(&self) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(&self.folder) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.path().is_file())
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| is_work_file_name(name))
            .collect();
        // The names hold a zero-padded date, so the text order is the date order.
        names.sort_unstable_by(|a, b| b.cmp(a));
        names
    }
}

fn file_name_of(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string()
}

fn matches(record: &Record, needle: &str) -> bool {
    let has = |text: &str| text.to_lowercase().contains(needle);
    has(&record.name)
        || has(&record.title)
        || has(&record.next_step)
        || record.decisions.iter().any(|d| has(&d.text))
}

/// True for `work-YYYY-MM-DD.db` with a real month and day number.
fn is_work_file_name(name: &str) -> bool {
    let Some(day) = name
        .strip_prefix(PREFIX)
        .and_then(|rest| rest.strip_suffix(SUFFIX))
    else {
        return false;
    };
    let parts: Vec<&str> = day.split('-').collect();
    let [year, month, date] = parts[..] else {
        return false;
    };
    let digits =
        |text: &str, len: usize| text.len() == len && text.bytes().all(|b| b.is_ascii_digit());
    if !(digits(year, 4) && digits(month, 2) && digits(date, 2)) {
        return false;
    }
    matches!(month.parse::<u8>(), Ok(1..=12)) && matches!(date.parse::<u8>(), Ok(1..=31))
}

/// The UTC date of a time in milliseconds since the epoch, as `YYYY-MM-DD`.
pub(super) fn day_of(ms: i64) -> String {
    let date = time::OffsetDateTime::from_unix_timestamp(ms.div_euclid(1000))
        .map(|t| t.date())
        .unwrap_or(time::Date::MIN);
    format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        u8::from(date.month()),
        date.day()
    )
}

#[cfg(test)]
#[path = "files_tests.rs"]
mod tests;
