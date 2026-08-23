use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use events::application_schema::LAST_PROCESSED_EVENT_SQL;
use events::{
    ActorType, EventNamespaces, EventStream, EventStreamVersion, ExpectedVersion, NewEvent,
    RotationPolicy, WorkflowRef,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use turso::Database;

const NAMESPACE: &str = "harold";
const PARTITION_KEY: &str = "main";
const STATE_DATABASE: &str = "harold-state.db";
const DELIVERY_OUTBOX_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS delivery_outbox (
    event_id TEXT PRIMARY KEY,
    event_version INTEGER NOT NULL UNIQUE,
    event_type TEXT NOT NULL,
    payload TEXT NOT NULL,
    trace_id TEXT NOT NULL,
    attempt_count INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    delivered_at_ms INTEGER
);
CREATE INDEX IF NOT EXISTS idx_delivery_outbox_pending
    ON delivery_outbox(delivered_at_ms, event_version);
"#;

const STATE_MIGRATIONS: [(&str, &str); 2] = [
    ("001_last_processed_event", LAST_PROCESSED_EVENT_SQL),
    ("002_delivery_outbox", DELIVERY_OUTBOX_SQL),
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnCompleted {
    pub pane_id: String,
    pub pane_label: String,
    pub last_user_prompt: String,
    pub assistant_message: String,
    pub main_context: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboundMessage {
    pub text: String,
}

#[derive(Debug)]
pub struct PendingDelivery {
    pub event_id: String,
    pub event_version: EventStreamVersion,
    pub event_type: String,
    pub payload: serde_json::Value,
    pub trace_id: String,
}

pub struct HaroldStore {
    stream: EventStream,
    state: Database,
}

fn rotation_policy() -> RotationPolicy {
    RotationPolicy::TimeWindow {
        window: Duration::from_secs(24 * 3600),
        max_bytes: Some(64 * 1024 * 1024),
    }
}

impl HaroldStore {
    pub async fn open(path: impl AsRef<Path>) -> events::Result<Self> {
        let root = path.as_ref();
        tokio::fs::create_dir_all(root).await?;

        let namespaces = EventNamespaces::open(root, rotation_policy()).await?;
        let namespace = namespaces.ensure_namespace(NAMESPACE).await?;
        let partition = namespace.ensure_partition(PARTITION_KEY).await?;
        let stream = partition.open().await?;

        let state_path = root.join(STATE_DATABASE);
        let state_path = state_path.to_str().ok_or_else(|| {
            events::EsError::InvalidPath("Harold state path contains invalid UTF-8".into())
        })?;
        let state = turso::Builder::new_local(state_path).build().await?;
        let conn = state.connect()?;
        configure_state_database(&conn).await?;
        run_state_migrations(&conn).await?;

        Ok(Self { stream, state })
    }

    #[cfg(test)]
    pub(crate) fn stream(&self) -> &EventStream {
        &self.stream
    }

    async fn last_processed_version(&self) -> events::Result<EventStreamVersion> {
        let conn = self.state.connect()?;
        let mut rows = conn
            .query(
                r#"
                SELECT last_processed_event_version
                FROM last_processed_event
                WHERE namespace = ?1 AND partition_key = ?2
                "#,
                (NAMESPACE, PARTITION_KEY),
            )
            .await?;
        let Some(row) = rows.next().await? else {
            return Ok(EventStreamVersion::start());
        };
        let version = row.get_value(0)?.as_integer().copied().ok_or_else(|| {
            events::EsError::Cursor("checkpoint version is not an integer".into())
        })?;
        if version == 0 {
            return Ok(EventStreamVersion::start());
        }
        EventStreamVersion::new(version)
    }

    pub async fn stage_unhandled_events(&self, limit: usize) -> events::Result<usize> {
        let cursor = self.last_processed_version().await?;
        let events = self.stream.load_after_version(cursor, limit).await?;
        let Some(last) = events.last() else {
            return Ok(0);
        };

        let conn = self.state.connect()?;
        conn.execute("BEGIN IMMEDIATE", ()).await?;
        for event in &events {
            let payload = serde_json::to_string(&event.payload)?;
            let result = conn
                .execute(
                    r#"
                    INSERT INTO delivery_outbox
                        (event_id, event_version, event_type, payload, trace_id)
                    VALUES (?1, ?2, ?3, ?4, ?5)
                    ON CONFLICT(event_id) DO NOTHING
                    "#,
                    (
                        event.id.to_string(),
                        event.version.get(),
                        event.r#type.as_str(),
                        payload,
                        event.id.to_string(),
                    ),
                )
                .await;
            if let Err(error) = result {
                let _ = conn.execute("ROLLBACK", ()).await;
                return Err(error.into());
            }
        }

        let now_ms = (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64;
        let checkpoint = conn
            .execute(
                r#"
                INSERT INTO last_processed_event
                    (namespace, partition_key, last_processed_event_version, updated_at_ms)
                VALUES (?1, ?2, ?3, ?4)
                ON CONFLICT(namespace, partition_key) DO UPDATE SET
                    last_processed_event_version = excluded.last_processed_event_version,
                    updated_at_ms = excluded.updated_at_ms
                "#,
                (NAMESPACE, PARTITION_KEY, last.version.get(), now_ms),
            )
            .await;
        if let Err(error) = checkpoint {
            let _ = conn.execute("ROLLBACK", ()).await;
            return Err(error.into());
        }
        if let Err(error) = conn.execute("COMMIT", ()).await {
            let _ = conn.execute("ROLLBACK", ()).await;
            return Err(error.into());
        }

        Ok(events.len())
    }

    pub async fn next_pending_delivery(&self) -> events::Result<Option<PendingDelivery>> {
        let conn = self.state.connect()?;
        let mut rows = conn
            .query(
                r#"
                SELECT event_id, event_version, event_type, payload, trace_id
                FROM delivery_outbox
                WHERE delivered_at_ms IS NULL
                ORDER BY event_version
                LIMIT 1
                "#,
                (),
            )
            .await?;
        let Some(row) = rows.next().await? else {
            return Ok(None);
        };

        let text = |index| -> events::Result<String> {
            row.get_value(index)?
                .as_text()
                .map(ToString::to_string)
                .ok_or_else(|| {
                    events::EsError::Cursor(format!("outbox column {index} is not text"))
                })
        };
        let version =
            row.get_value(1)?.as_integer().copied().ok_or_else(|| {
                events::EsError::Cursor("outbox version is not an integer".into())
            })?;
        let payload = serde_json::from_str(&text(3)?)?;
        Ok(Some(PendingDelivery {
            event_id: text(0)?,
            event_version: EventStreamVersion::new(version)?,
            event_type: text(2)?,
            payload,
            trace_id: text(4)?,
        }))
    }

    pub async fn mark_delivered(&self, event_id: &str) -> events::Result<()> {
        let conn = self.state.connect()?;
        let now_ms = (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64;
        conn.execute(
            r#"
            UPDATE delivery_outbox
            SET delivered_at_ms = ?1, last_error = NULL
            WHERE event_id = ?2
            "#,
            (now_ms, event_id),
        )
        .await?;
        Ok(())
    }

    pub async fn record_delivery_failure(&self, event_id: &str, error: &str) -> events::Result<()> {
        let conn = self.state.connect()?;
        conn.execute(
            r#"
            UPDATE delivery_outbox
            SET attempt_count = attempt_count + 1, last_error = ?1
            WHERE event_id = ?2
            "#,
            (error, event_id),
        )
        .await?;
        Ok(())
    }

    pub async fn mark_undeliverable(&self, event_id: &str, error: &str) -> events::Result<()> {
        let conn = self.state.connect()?;
        let now_ms = (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64;
        conn.execute(
            r#"
            UPDATE delivery_outbox
            SET attempt_count = attempt_count + 1,
                last_error = ?1,
                delivered_at_ms = ?2
            WHERE event_id = ?3
            "#,
            (error, now_ms, event_id),
        )
        .await?;
        Ok(())
    }
}

async fn configure_state_database(conn: &turso::Connection) -> events::Result<()> {
    conn.busy_timeout(Duration::from_millis(5_000))?;
    let mut rows = conn.query("PRAGMA journal_mode = WAL", ()).await?;
    while rows.next().await?.is_some() {}
    conn.execute("PRAGMA synchronous = NORMAL", ()).await?;
    Ok(())
}

async fn run_state_migrations(conn: &turso::Connection) -> events::Result<()> {
    conn.execute(
        r#"
        CREATE TABLE IF NOT EXISTS _migrations (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL UNIQUE,
            checksum TEXT NOT NULL,
            applied_at_ms INTEGER NOT NULL
        )
        "#,
        (),
    )
    .await?;

    for (name, sql) in STATE_MIGRATIONS {
        let checksum = hex::encode(Sha256::digest(sql.as_bytes()));
        let mut rows = conn
            .query("SELECT checksum FROM _migrations WHERE name = ?1", (name,))
            .await?;
        if let Some(row) = rows.next().await? {
            let applied = row
                .get_value(0)?
                .as_text()
                .ok_or_else(|| {
                    events::EsError::Migration(format!("migration {name} has a non-text checksum"))
                })?
                .to_string();
            if applied != checksum {
                return Err(events::EsError::Migration(format!(
                    "migration {name} checksum changed"
                )));
            }
            continue;
        }

        conn.execute("BEGIN IMMEDIATE", ()).await?;
        if let Err(error) = conn.execute_batch(sql).await {
            let _ = conn.execute("ROLLBACK", ()).await;
            return Err(error.into());
        }
        let now_ms = (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64;
        if let Err(error) = conn
            .execute(
                "INSERT INTO _migrations (name, checksum, applied_at_ms) VALUES (?1, ?2, ?3)",
                (name, checksum, now_ms),
            )
            .await
        {
            let _ = conn.execute("ROLLBACK", ()).await;
            return Err(error.into());
        }
        if let Err(error) = conn.execute("COMMIT", ()).await {
            let _ = conn.execute("ROLLBACK", ()).await;
            return Err(error.into());
        }
    }
    Ok(())
}

pub async fn open_store(path: impl AsRef<Path>) -> events::Result<Arc<HaroldStore>> {
    Ok(Arc::new(HaroldStore::open(path).await?))
}

pub async fn append_turn_completed(
    store: &HaroldStore,
    event: &TurnCompleted,
) -> events::Result<()> {
    store
        .stream
        .append(
            ExpectedVersion::Any,
            [NewEvent {
                r#type: "TurnCompleted".into(),
                payload: json!(event),
                workflow_kind: None,
                workflow: WorkflowRef::None,
                request_id: None,
                actor_id: "system:harold".into(),
                actor_type: ActorType::System,
            }],
        )
        .await?;
    Ok(())
}

pub async fn append_inbound_message(
    store: &HaroldStore,
    event: &InboundMessage,
) -> events::Result<()> {
    store
        .stream
        .append(
            ExpectedVersion::Any,
            [NewEvent {
                r#type: "InboundMessageReceived".into(),
                payload: json!(event),
                workflow_kind: None,
                workflow: WorkflowRef::None,
                request_id: None,
                actor_id: "system:harold".into(),
                actor_type: ActorType::System,
            }],
        )
        .await?;
    Ok(())
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
