//! The two graph logs. Each is one OpenData log in its own folder, with no retention: the
//! logs are the graph's source. `RecordLog` is what the graph writer needs of one, so tests
//! can use a log in memory whose sequences jump.

use std::path::Path;
use std::time::Duration;

use opendata_common::storage::config::{LocalObjectStoreConfig, SlateDbStorageConfig};
use opendata_common::{ObjectStoreConfig, StorageConfig};
use opendata_log::{Config, LogDb, LogRead, ReadVisibility, Record};

const APPEND_QUEUE_WAIT: Duration = Duration::from_secs(5);
const SETTINGS_FILE: &str = "log-settings.toml";
const SETTINGS: &str =
    "# Written by Harold each time it opens the log. SlateDB's defaults apply.\n";
const PREFIX: &str = "graph";

/// The log could not be written or read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LogError(pub String);

impl std::fmt::Display for LogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn log_error(error: impl std::fmt::Display) -> LogError {
    LogError(error.to_string())
}

#[tonic::async_trait]
pub(crate) trait RecordLog: Send + Sync {
    /// Appends one record, flushes it and returns its sequence.
    async fn append(&self, value: Vec<u8>) -> Result<u64, LogError>;
    /// Every flushed record with a sequence greater than `after` (all with `None`), oldest first.
    async fn read_after(&self, after: Option<u64>) -> Result<Vec<(u64, Vec<u8>)>, LogError>;
}

/// The sequence of the last record, if the log holds any.
pub(crate) async fn end_sequence(log: &dyn RecordLog) -> Result<Option<u64>, LogError> {
    Ok(log
        .read_after(None)
        .await?
        .last()
        .map(|(sequence, _)| *sequence))
}

pub(crate) struct OpenDataLog {
    log: LogDb,
    key: &'static [u8],
}

fn make_private_folder(path: &Path) -> Result<(), LogError> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(log_error)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(log_error)
}

fn utf8(path: &Path) -> Result<String, LogError> {
    path.to_str()
        .map(str::to_string)
        .ok_or_else(|| LogError(format!("log path is not UTF-8: {}", path.display())))
}

impl OpenDataLog {
    /// Opens the log in `folder` (mode 0700), local filesystem only, at an absolute path.
    pub(crate) async fn open(folder: &Path, key: &'static [u8]) -> Result<Self, LogError> {
        make_private_folder(folder)?;
        let root = std::fs::canonicalize(folder).map_err(log_error)?;
        let log_dir = root.join("log");
        make_private_folder(&log_dir)?;
        let settings = root.join(SETTINGS_FILE);
        std::fs::write(&settings, SETTINGS).map_err(log_error)?;
        let config = Config {
            storage: StorageConfig::SlateDb(SlateDbStorageConfig {
                path: PREFIX.into(),
                object_store: ObjectStoreConfig::Local(LocalObjectStoreConfig {
                    path: utf8(&log_dir)?,
                }),
                settings_path: Some(utf8(&settings)?),
                block_cache: None,
                meta_cache: None,
            }),
            read_visibility: ReadVisibility::Remote,
            ..Config::default()
        };
        let log = LogDb::open(config).await.map_err(log_error)?;
        Ok(Self { log, key })
    }
}

#[tonic::async_trait]
impl RecordLog for OpenDataLog {
    async fn append(&self, value: Vec<u8>) -> Result<u64, LogError> {
        let output = self
            .log
            .append_timeout(
                vec![Record {
                    key: self.key.into(),
                    value: value.into(),
                }],
                APPEND_QUEUE_WAIT,
            )
            .await
            .map_err(log_error)?;
        self.log.flush().await.map_err(log_error)?;
        Ok(output.start_sequence)
    }

    async fn read_after(&self, after: Option<u64>) -> Result<Vec<(u64, Vec<u8>)>, LogError> {
        let from = after.map_or(0, |sequence| sequence.saturating_add(1));
        let mut entries = self
            .log
            .scan(self.key.into(), from..)
            .await
            .map_err(log_error)?;
        let mut records = Vec::new();
        while let Some(entry) = entries.next().await.map_err(log_error)? {
            records.push((entry.sequence, entry.value.to_vec()));
        }
        Ok(records)
    }
}

#[cfg(test)]
#[path = "log_tests.rs"]
pub(crate) mod tests;
