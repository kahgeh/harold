//! Harold's event stream: one OpenData log at `<store>/log/`, each event one record under
//! the key `harold/main`, its value Harold's JSON envelope. The record's sequence plus one
//! is the event's version, so 0 stays "before any event".

use std::path::Path;
use std::time::Duration;

use opendata_common::storage::config::{LocalObjectStoreConfig, SlateDbStorageConfig};
use opendata_common::{ObjectStoreConfig, StorageConfig};
use opendata_log::{AppendError, Config, LogDb, LogRead, ReadVisibility, Record};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tracing::warn;
use uuid::Uuid;

use super::error::{Result, StoreError};

const KEY: &[u8] = b"harold/main";
const ACTOR: &str = "system:harold";
const LOG_DIRECTORY: &str = "log";
/// SlateDB's prefix inside the log directory.
const LOG_PREFIX: &str = "harold";
pub(super) const LOG_SETTINGS_FILE: &str = "log-settings.toml";
/// Harold's own SlateDB settings: none, so SlateDB's defaults apply. Written at every open,
/// so SlateDB never reads `SlateDb.*` from the working directory or `SLATEDB_*` variables.
const LOG_SETTINGS: &str =
    "# Written by Harold each time it opens the store. SlateDB's defaults apply.\n";
/// How long an append waits for room in the log's write queue.
const APPEND_QUEUE_WAIT: Duration = Duration::from_secs(5);
const UNDECODABLE: &str = "UndecodableEvent";

/// An event's place in the stream. Versions only rise, but they have gaps: never assume
/// the next one is this one plus 1. 0 is the start, before any event.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct EventVersion(u64);

impl EventVersion {
    pub const fn start() -> Self {
        Self(0)
    }

    pub fn new(value: u64) -> Result<Self> {
        if value == 0 || i64::try_from(value).is_err() {
            return Err(StoreError::Data(format!(
                "event versions run from 1 to {}, got {value}",
                i64::MAX
            )));
        }
        Ok(Self(value))
    }

    pub fn get(self) -> u64 {
        self.0
    }

    pub fn is_start(self) -> bool {
        self.0 == 0
    }

    /// The value a `harold-state.db` version column holds. `new` keeps every version
    /// within `i64`.
    pub fn to_column(self) -> i64 {
        self.0 as i64
    }

    /// Reads a version column; 0 is the start.
    pub fn from_column(value: i64) -> Result<Self> {
        if value == 0 {
            return Ok(Self::start());
        }
        let value = u64::try_from(value)
            .map_err(|_| StoreError::Data(format!("negative event version {value}")))?;
        Self::new(value)
    }
}

impl std::fmt::Display for EventVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone)]
pub struct NewEvent {
    pub r#type: String,
    pub payload: serde_json::Value,
}

impl NewEvent {
    pub fn new(r#type: impl Into<String>, payload: serde_json::Value) -> Self {
        Self {
            r#type: r#type.into(),
            payload,
        }
    }
}

#[derive(Debug, Clone)]
pub struct EventEnvelope {
    pub id: Uuid,
    pub r#type: String,
    pub payload: serde_json::Value,
    pub version: EventVersion,
    pub created_at: OffsetDateTime,
    pub trace_id: Option<String>,
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "part of the stored envelope; only tests read it yet"
        )
    )]
    pub span_id: Option<String>,
    #[expect(
        dead_code,
        reason = "part of the stored envelope; nothing reads it yet"
    )]
    pub actor: String,
}

#[derive(Debug, Clone)]
pub struct AppendResult {
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "part of an append's result; only tests read it yet"
        )
    )]
    pub first_version: EventVersion,
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "part of an append's result; only tests read it yet"
        )
    )]
    pub last_version: EventVersion,
    pub events: Vec<EventEnvelope>,
}

impl AppendResult {
    fn from_events(events: Vec<EventEnvelope>) -> Self {
        let first_version = events
            .first()
            .map_or(EventVersion::start(), |event| event.version);
        let last_version = events
            .last()
            .map_or(EventVersion::start(), |event| event.version);
        Self {
            first_version,
            last_version,
            events,
        }
    }
}

impl EventVersion {
    fn from_sequence(sequence: u64) -> Result<Self> {
        let value = sequence
            .checked_add(1)
            .ok_or_else(|| StoreError::Data(format!("log sequence {sequence} is too large")))?;
        Self::new(value)
    }

    /// The log sequence of a version that is not the start.
    fn sequence(self) -> u64 {
        self.0 - 1
    }
}

/// The envelope as stored: what the `events` crate kept, in Harold's own shape.
#[derive(Serialize, Deserialize)]
struct StoredEnvelope {
    id: Uuid,
    r#type: String,
    payload: serde_json::Value,
    created_at_ms: i64,
    trace_id: Option<String>,
    span_id: Option<String>,
    actor: String,
}

pub struct EventLog {
    log: LogDb,
}

/// The log's configuration, built here and nowhere else: always the local filesystem at
/// an absolute path, and always Harold's own settings file.
pub(super) fn log_config(root: &Path) -> Result<Config> {
    std::fs::create_dir_all(root)?;
    let root = std::fs::canonicalize(root)?;
    let log_dir = root.join(LOG_DIRECTORY);
    std::fs::create_dir_all(&log_dir)?;
    let settings = root.join(LOG_SETTINGS_FILE);
    Ok(Config {
        storage: StorageConfig::SlateDb(SlateDbStorageConfig {
            path: LOG_PREFIX.into(),
            object_store: ObjectStoreConfig::Local(LocalObjectStoreConfig {
                path: utf8(&log_dir)?,
            }),
            settings_path: Some(utf8(&settings)?),
            block_cache: None,
            meta_cache: None,
        }),
        read_visibility: ReadVisibility::Remote,
        ..Config::default()
    })
}

fn utf8(path: &Path) -> Result<String> {
    path.to_str()
        .map(str::to_string)
        .ok_or_else(|| StoreError::Data(format!("store path is not UTF-8: {}", path.display())))
}

fn log_error(error: impl std::fmt::Display) -> StoreError {
    StoreError::Log(error.to_string())
}

/// A stored millisecond time as a time.
fn time_from_ms(created_at_ms: i64) -> Result<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(created_at_ms) * 1_000_000)
        .map_err(|error| StoreError::Data(format!("event time {created_at_ms}: {error}")))
}

impl EventLog {
    pub(super) async fn open(root: &Path) -> Result<Self> {
        let config = log_config(root)?;
        let StorageConfig::SlateDb(SlateDbStorageConfig {
            settings_path: Some(settings),
            ..
        }) = &config.storage
        else {
            return Err(StoreError::Data(
                "the log's configuration names no settings file".into(),
            ));
        };
        std::fs::write(settings, LOG_SETTINGS)?;
        let log = LogDb::open(config).await.map_err(log_error)?;
        Ok(Self { log })
    }

    /// Appends `batch` as one write and returns once it is flushed. An empty batch writes
    /// nothing.
    pub async fn append(&self, batch: impl IntoIterator<Item = NewEvent>) -> Result<AppendResult> {
        let (trace_id, span_id) = current_trace();
        let created_at_ms = (OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64;
        // In memory, the same millisecond time that is stored, so an appended envelope and
        // the same envelope read back agree.
        let created_at = time_from_ms(created_at_ms)?;
        let mut envelopes = Vec::new();
        let mut records = Vec::new();
        for event in batch {
            let stored = StoredEnvelope {
                id: Uuid::new_v4(),
                r#type: event.r#type,
                payload: event.payload,
                created_at_ms,
                trace_id: trace_id.clone(),
                span_id: span_id.clone(),
                actor: ACTOR.into(),
            };
            records.push(Record {
                key: KEY.into(),
                value: serde_json::to_vec(&stored)?.into(),
            });
            envelopes.push(stored);
        }
        if records.is_empty() {
            return Ok(AppendResult::from_events(Vec::new()));
        }
        let output = self
            .log
            .append_timeout(records, APPEND_QUEUE_WAIT)
            .await
            .map_err(|error: AppendError| log_error(error))?;
        self.log.flush().await.map_err(log_error)?;
        let events = envelopes
            .into_iter()
            .enumerate()
            .map(|(offset, stored)| {
                let version = EventVersion::from_sequence(output.start_sequence + offset as u64)?;
                Ok(envelope(stored, version, created_at))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(AppendResult::from_events(events))
    }

    /// Up to `limit` flushed events after `after`, oldest first.
    pub async fn load_after_version(
        &self,
        after: EventVersion,
        limit: usize,
    ) -> Result<Vec<EventEnvelope>> {
        // Version v is sequence v - 1, so the first sequence after `after` is `after.get()`.
        let mut entries = self
            .log
            .scan(KEY.into(), after.get()..)
            .await
            .map_err(log_error)?;
        let mut events = Vec::new();
        while events.len() < limit {
            let Some(entry) = entries.next().await.map_err(log_error)? else {
                break;
            };
            events.push(decode(entry.sequence, &entry.value)?);
        }
        Ok(events)
    }

    /// The flushed event at `version`, if there is one.
    pub async fn read(&self, version: EventVersion) -> Result<Option<EventEnvelope>> {
        if version.is_start() {
            return Ok(None);
        }
        let sequence = version.sequence();
        let mut entries = self
            .log
            .scan(KEY.into(), sequence..=sequence)
            .await
            .map_err(log_error)?;
        match entries.next().await.map_err(log_error)? {
            Some(entry) => Ok(Some(decode(entry.sequence, &entry.value)?)),
            None => Ok(None),
        }
    }

    /// Appends `value` as a record as it is, for tests of records Harold did not write.
    #[cfg(test)]
    pub(crate) async fn append_raw_for_test(&self, value: Vec<u8>) -> Result<EventVersion> {
        let output = self
            .log
            .append_timeout(
                vec![Record {
                    key: KEY.into(),
                    value: value.into(),
                }],
                APPEND_QUEUE_WAIT,
            )
            .await
            .map_err(log_error)?;
        self.log.flush().await.map_err(log_error)?;
        EventVersion::from_sequence(output.start_sequence)
    }
}

/// The current OpenTelemetry span's trace and span ids, as the `events` crate read them;
/// both empty with telemetry off.
fn current_trace() -> (Option<String>, Option<String>) {
    use opentelemetry::trace::{SpanId, TraceContextExt, TraceId};
    use tracing_opentelemetry::OpenTelemetrySpanExt;

    let context = tracing::Span::current().context();
    let span = context.span();
    let span_context = span.span_context();
    let trace_id =
        (span_context.trace_id() != TraceId::INVALID).then(|| span_context.trace_id().to_string());
    let span_id =
        (span_context.span_id() != SpanId::INVALID).then(|| span_context.span_id().to_string());
    (trace_id, span_id)
}

fn envelope(
    stored: StoredEnvelope,
    version: EventVersion,
    created_at: OffsetDateTime,
) -> EventEnvelope {
    EventEnvelope {
        id: stored.id,
        r#type: stored.r#type,
        payload: stored.payload,
        version,
        created_at,
        trace_id: stored.trace_id,
        span_id: stored.span_id,
        actor: stored.actor,
    }
}

/// A record as an event. A record that is not a Harold envelope, or whose time Harold
/// cannot read, becomes an `UndecodableEvent`, so it cannot block the events after it.
fn decode(sequence: u64, value: &[u8]) -> Result<EventEnvelope> {
    let version = EventVersion::from_sequence(sequence)?;
    let stored = match serde_json::from_slice::<StoredEnvelope>(value) {
        Ok(stored) => stored,
        Err(error) => return Ok(undecodable(sequence, version, error)),
    };
    match time_from_ms(stored.created_at_ms) {
        Ok(created_at) => Ok(envelope(stored, version, created_at)),
        Err(error) => Ok(undecodable(sequence, version, error)),
    }
}

/// The event that stands in for the record at `sequence`, which Harold cannot read. Its
/// id is the same at every read.
fn undecodable(
    sequence: u64,
    version: EventVersion,
    error: impl std::fmt::Display,
) -> EventEnvelope {
    warn!(version = %version, error = %error, "stored record is not a Harold event");
    EventEnvelope {
        id: Uuid::from_u64_pair(0, sequence),
        r#type: UNDECODABLE.into(),
        payload: serde_json::json!({ "error": error.to_string() }),
        version,
        created_at: OffsetDateTime::UNIX_EPOCH,
        trace_id: None,
        span_id: None,
        actor: String::new(),
    }
}
