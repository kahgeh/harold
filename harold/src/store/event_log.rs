//! Harold's event stream. Callers see only Harold's own types; this file is the one place
//! that knows how the stream is stored.

use std::path::Path;
use std::time::Duration;

use events::{
    ActorType, EventNamespaces, EventStream, ExpectedVersion, RotationPolicy, WorkflowRef,
};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use super::error::{Result, StoreError};

const NAMESPACE: &str = "harold";
const PARTITION_KEY: &str = "main";
const ACTOR: &str = "system:harold";

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
#[allow(
    dead_code,
    reason = "span_id and actor are part of the stored envelope; nothing reads them yet"
)]
pub struct EventEnvelope {
    pub id: Uuid,
    pub r#type: String,
    pub payload: serde_json::Value,
    pub version: EventVersion,
    pub created_at: OffsetDateTime,
    pub trace_id: Option<String>,
    pub span_id: Option<String>,
    pub actor: String,
}

#[derive(Debug, Clone)]
#[allow(
    dead_code,
    reason = "first_version and last_version are part of an append's result; nothing reads them yet"
)]
pub struct AppendResult {
    pub first_version: EventVersion,
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

pub struct EventLog {
    stream: EventStream,
}

fn rotation_policy() -> RotationPolicy {
    RotationPolicy::TimeWindow {
        window: Duration::from_secs(24 * 3600),
        max_bytes: Some(64 * 1024 * 1024),
    }
}

impl EventLog {
    pub(super) async fn open(root: &Path) -> Result<Self> {
        let namespaces = EventNamespaces::open(root, rotation_policy()).await?;
        let namespace = namespaces.ensure_namespace(NAMESPACE).await?;
        let partition = namespace.ensure_partition(PARTITION_KEY).await?;
        Ok(Self {
            stream: partition.open().await?,
        })
    }

    /// Appends `batch` as one write. An empty batch writes nothing.
    pub async fn append(&self, batch: impl IntoIterator<Item = NewEvent>) -> Result<AppendResult> {
        let batch: Vec<events::NewEvent> = batch
            .into_iter()
            .map(|event| events::NewEvent {
                r#type: event.r#type,
                payload: event.payload,
                workflow_kind: None,
                workflow: WorkflowRef::None,
                request_id: None,
                actor_id: ACTOR.into(),
                actor_type: ActorType::System,
            })
            .collect();
        if batch.is_empty() {
            return Ok(AppendResult::from_events(Vec::new()));
        }
        let appended = self.stream.append(ExpectedVersion::Any, batch).await?;
        let events = appended
            .events
            .into_iter()
            .map(envelope_from)
            .collect::<Result<Vec<_>>>()?;
        Ok(AppendResult::from_events(events))
    }

    /// Up to `limit` events after `after`, oldest first.
    pub async fn load_after_version(
        &self,
        after: EventVersion,
        limit: usize,
    ) -> Result<Vec<EventEnvelope>> {
        let after = if after.is_start() {
            events::EventStreamVersion::start()
        } else {
            events::EventStreamVersion::new(after.to_column())?
        };
        self.stream
            .load_after_version(after, limit)
            .await?
            .into_iter()
            .map(envelope_from)
            .collect()
    }

    /// The event at `version`, if there is one.
    pub async fn read(&self, version: EventVersion) -> Result<Option<EventEnvelope>> {
        if version.is_start() {
            return Ok(None);
        }
        let before = EventVersion::from_column(version.to_column() - 1)?;
        Ok(self
            .load_after_version(before, 1)
            .await?
            .into_iter()
            .find(|event| event.version == version))
    }
}

fn envelope_from(event: events::EventEnvelope) -> Result<EventEnvelope> {
    Ok(EventEnvelope {
        id: event.id,
        r#type: event.r#type,
        payload: event.payload,
        version: EventVersion::from_column(event.version.get())?,
        created_at: event.created_at,
        trace_id: event.trace_id,
        span_id: event.span_id,
        actor: event.actor_id,
    })
}
