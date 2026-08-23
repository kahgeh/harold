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

use crate::agent::domain::{
    AgentEvent, AgentMonitorHealthChanged, AgentPaneProjection, AgentScreenObserved, AgentSnapshot,
    EffectiveAgentState, MonitorHealthProjection, ObservedAgentState, ProjectionChange,
    WorkSummaryUpdate,
};
#[cfg(test)]
use crate::agent::reducer::DEFAULT_HOOK_GRACE_MS;
use crate::agent::reducer::reduce_agent_event;
use crate::agent::summary::normalize_work_summary;

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

const AGENT_MONITOR_PROJECTION_SQL: &str =
    include_str!("store/migrations/003_agent_monitor_projection.sql");

const STATE_MIGRATIONS: [(&str, &str); 3] = [
    ("001_last_processed_event", LAST_PROCESSED_EVENT_SQL),
    ("002_delivery_outbox", DELIVERY_OUTBOX_SQL),
    ("003_agent_monitor_projection", AGENT_MONITOR_PROJECTION_SQL),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProjectionBatch {
    pub applied: usize,
    pub through_event_version: EventStreamVersion,
    pub snapshot_changed: bool,
}

pub struct HaroldStore {
    stream: EventStream,
    state: Database,
    hook_grace_ms: u64,
    #[cfg(test)]
    fail_projection_before_checkpoint: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    snapshot_read_gate: std::sync::Mutex<Option<SnapshotReadGate>>,
}

#[cfg(test)]
struct SnapshotReadGate {
    started: tokio::sync::oneshot::Sender<()>,
    resume: tokio::sync::oneshot::Receiver<()>,
}

#[cfg(test)]
pub(crate) struct SnapshotReadGateHandle {
    started: Option<tokio::sync::oneshot::Receiver<()>>,
    resume: Option<tokio::sync::oneshot::Sender<()>>,
}

#[cfg(test)]
impl SnapshotReadGateHandle {
    pub(crate) async fn wait_until_started(&mut self) {
        self.started
            .take()
            .expect("snapshot reader waits once")
            .await
            .expect("snapshot reader started");
    }

    pub(crate) fn resume(&mut self) {
        let _ = self
            .resume
            .take()
            .expect("snapshot reader resume once")
            .send(());
    }
}

fn rotation_policy() -> RotationPolicy {
    RotationPolicy::TimeWindow {
        window: Duration::from_secs(24 * 3600),
        max_bytes: Some(64 * 1024 * 1024),
    }
}

impl HaroldStore {
    #[cfg(test)]
    pub async fn open(path: impl AsRef<Path>) -> events::Result<Self> {
        Self::open_with_hook_grace(path, DEFAULT_HOOK_GRACE_MS).await
    }

    pub(crate) async fn open_with_hook_grace(
        path: impl AsRef<Path>,
        hook_grace_ms: u64,
    ) -> events::Result<Self> {
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

        Ok(Self {
            stream,
            state,
            hook_grace_ms,
            #[cfg(test)]
            fail_projection_before_checkpoint: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            snapshot_read_gate: std::sync::Mutex::new(None),
        })
    }

    #[cfg(test)]
    pub(crate) fn stream(&self) -> &EventStream {
        &self.stream
    }

    pub(crate) async fn last_processed_version(&self) -> events::Result<EventStreamVersion> {
        let conn = self.state.connect()?;
        last_processed_version_from(&conn).await
    }

    pub(crate) async fn project_unhandled_events(
        &self,
        limit: usize,
    ) -> events::Result<ProjectionBatch> {
        let cursor = self.last_processed_version().await?;
        let events = self.stream.load_after_version(cursor, limit).await?;
        let Some(last) = events.last() else {
            return Ok(ProjectionBatch {
                applied: 0,
                through_event_version: cursor,
                snapshot_changed: false,
            });
        };

        let conn = self.state.connect()?;
        conn.execute("BEGIN IMMEDIATE", ()).await?;
        let result = async {
            let mut snapshot_changed = false;
            for event in &events {
                match event.r#type.as_str() {
                    "TurnCompleted" | "InboundMessageReceived" => {
                        stage_delivery_event(&conn, event).await?;
                    }
                    "AgentPaneObserved"
                    | "AgentPaneDeparted"
                    | "AgentLifecycleObserved"
                    | "AgentScreenObserved" => {
                        snapshot_changed |=
                            project_agent_event(&conn, event, self.hook_grace_ms).await?;
                    }
                    "AgentMonitorHealthChanged" => {
                        let health = serde_json::from_value::<AgentMonitorHealthChanged>(
                            event.payload.clone(),
                        )?;
                        upsert_monitor_health(&conn, &health, event.version).await?;
                        snapshot_changed = true;
                    }
                    _ => {
                        // Unknown stream facts stay visible to the existing permanent-delivery
                        // path; advancing without an outbox record would silently lose them.
                        stage_delivery_event(&conn, event).await?;
                    }
                }
            }

            #[cfg(test)]
            if self
                .fail_projection_before_checkpoint
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                return Err(events::EsError::Migration(
                    "test projection failure before checkpoint".into(),
                ));
            }

            let now_ms = now_ms();
            conn.execute(
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
            .await?;
            conn.execute("COMMIT", ()).await?;
            Ok(snapshot_changed)
        }
        .await;
        let snapshot_changed = match result {
            Ok(snapshot_changed) => snapshot_changed,
            Err(error) => {
                let _ = conn.execute("ROLLBACK", ()).await;
                return Err(error);
            }
        };

        Ok(ProjectionBatch {
            applied: events.len(),
            through_event_version: last.version,
            snapshot_changed,
        })
    }

    #[allow(
        dead_code,
        reason = "the Task 9 publisher consumes committed snapshots in the next monitor slice"
    )]
    pub(crate) async fn load_agent_snapshot(&self) -> events::Result<AgentSnapshot> {
        let conn = self.state.connect()?;
        let snapshot = load_agent_snapshot_from_one_query(&conn).await?;
        #[cfg(test)]
        pause_snapshot_read_after_query(&self.snapshot_read_gate).await;
        Ok(snapshot)
    }

    #[cfg(test)]
    pub(crate) fn fail_projection_before_checkpoint_for_test(&self) {
        self.fail_projection_before_checkpoint
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    #[cfg(test)]
    pub(crate) fn pause_snapshot_read_after_query_for_test(&self) -> SnapshotReadGateHandle {
        let (started, started_receiver) = tokio::sync::oneshot::channel();
        let (resume_sender, resume) = tokio::sync::oneshot::channel();
        *self
            .snapshot_read_gate
            .lock()
            .expect("snapshot read gate lock") = Some(SnapshotReadGate { started, resume });
        SnapshotReadGateHandle {
            started: Some(started_receiver),
            resume: Some(resume_sender),
        }
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

const AGENT_PANE_COLUMNS: &str = r#"
    pane_id, pane_pid, agent_pid, agent_started_at_ms, provider_id,
    tmux_target, session_name, window_index, pane_index, working_directory,
    provider_display_name, pane_observed_at_ms, hook_state, hook_observed_at_ms,
    screen_state, screen_classifier_id, screen_observed_at_ms, effective_state,
    explicit_work_summary, explicit_work_summary_updated_at_ms,
    screen_work_summary, screen_work_summary_updated_at_ms, work_summary,
    last_transition_at_ms, last_event_version
"#;

async fn last_processed_version_from(
    conn: &turso::Connection,
) -> events::Result<EventStreamVersion> {
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
    let version =
        row.get_value(0)?.as_integer().copied().ok_or_else(|| {
            events::EsError::Cursor("checkpoint version is not an integer".into())
        })?;
    if version == 0 {
        return Ok(EventStreamVersion::start());
    }
    EventStreamVersion::new(version)
}

async fn stage_delivery_event(
    conn: &turso::Connection,
    event: &events::EventEnvelope,
) -> events::Result<()> {
    let payload = serde_json::to_string(&event.payload)?;
    conn.execute(
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
    .await?;
    Ok(())
}

async fn project_agent_event(
    conn: &turso::Connection,
    event: &events::EventEnvelope,
    hook_grace_ms: u64,
) -> events::Result<bool> {
    let agent_event = match event.r#type.as_str() {
        "AgentPaneObserved" => {
            AgentEvent::PaneObserved(serde_json::from_value(event.payload.clone())?)
        }
        "AgentPaneDeparted" => {
            AgentEvent::PaneDeparted(serde_json::from_value(event.payload.clone())?)
        }
        "AgentLifecycleObserved" => {
            AgentEvent::LifecycleObserved(serde_json::from_value(event.payload.clone())?)
        }
        "AgentScreenObserved" => {
            AgentEvent::ScreenObserved(serde_json::from_value(event.payload.clone())?)
        }
        _ => {
            return Err(events::EsError::Migration(format!(
                "unsupported agent projection event type: {}",
                event.r#type
            )));
        }
    };
    let pane_id = agent_event_pane_id(&agent_event);
    let current = load_agent_pane(conn, pane_id).await?;
    match reduce_agent_event(current, &agent_event, event.version, hook_grace_ms) {
        ProjectionChange::Upsert(projection) => {
            upsert_agent_pane(conn, &projection).await?;
            Ok(true)
        }
        ProjectionChange::Remove(incarnation) => {
            let removed = conn
                .execute(
                    "DELETE FROM agent_panes WHERE pane_id = ?1",
                    (incarnation.pane_id.as_str(),),
                )
                .await?;
            Ok(removed > 0)
        }
        ProjectionChange::Ignore => Ok(false),
    }
}

fn agent_event_pane_id(event: &AgentEvent) -> &str {
    match event {
        AgentEvent::PaneObserved(event) => &event.pane.incarnation.pane_id,
        AgentEvent::PaneDeparted(event) => &event.incarnation.pane_id,
        AgentEvent::LifecycleObserved(event) => &event.incarnation.pane_id,
        AgentEvent::ScreenObserved(event) => &event.incarnation.pane_id,
        AgentEvent::MonitorHealthChanged(_) => unreachable!("health events are not pane events"),
    }
}

async fn upsert_monitor_health(
    conn: &turso::Connection,
    health: &AgentMonitorHealthChanged,
    event_version: EventStreamVersion,
) -> events::Result<()> {
    conn.execute(
        r#"
        INSERT INTO agent_monitor_health
            (component, healthy, reason_code, observed_at_ms, last_event_version)
        VALUES (?1, ?2, ?3, ?4, ?5)
        ON CONFLICT(component) DO UPDATE SET
            healthy = excluded.healthy,
            reason_code = excluded.reason_code,
            observed_at_ms = excluded.observed_at_ms,
            last_event_version = excluded.last_event_version
        "#,
        (
            health.component.as_str(),
            i64::from(health.healthy),
            health.reason_code.as_str(),
            health.observed_at_ms,
            event_version.get(),
        ),
    )
    .await?;
    Ok(())
}

async fn upsert_agent_pane(
    conn: &turso::Connection,
    projection: &AgentPaneProjection,
) -> events::Result<()> {
    let pane = &projection.pane;
    conn.execute(
        r#"
        INSERT INTO agent_panes (
            pane_id, pane_pid, agent_pid, agent_started_at_ms, provider_id,
            tmux_target, session_name, window_index, pane_index, working_directory,
            provider_display_name, pane_observed_at_ms, hook_state, hook_observed_at_ms,
            screen_state, screen_classifier_id, screen_observed_at_ms, effective_state,
            explicit_work_summary, explicit_work_summary_updated_at_ms,
            screen_work_summary, screen_work_summary_updated_at_ms, work_summary,
            last_transition_at_ms, last_event_version
        ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
            ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25
        ) ON CONFLICT(pane_id) DO UPDATE SET
            pane_pid = excluded.pane_pid,
            agent_pid = excluded.agent_pid,
            agent_started_at_ms = excluded.agent_started_at_ms,
            provider_id = excluded.provider_id,
            tmux_target = excluded.tmux_target,
            session_name = excluded.session_name,
            window_index = excluded.window_index,
            pane_index = excluded.pane_index,
            working_directory = excluded.working_directory,
            provider_display_name = excluded.provider_display_name,
            pane_observed_at_ms = excluded.pane_observed_at_ms,
            hook_state = excluded.hook_state,
            hook_observed_at_ms = excluded.hook_observed_at_ms,
            screen_state = excluded.screen_state,
            screen_classifier_id = excluded.screen_classifier_id,
            screen_observed_at_ms = excluded.screen_observed_at_ms,
            effective_state = excluded.effective_state,
            explicit_work_summary = excluded.explicit_work_summary,
            explicit_work_summary_updated_at_ms = excluded.explicit_work_summary_updated_at_ms,
            screen_work_summary = excluded.screen_work_summary,
            screen_work_summary_updated_at_ms = excluded.screen_work_summary_updated_at_ms,
            work_summary = excluded.work_summary,
            last_transition_at_ms = excluded.last_transition_at_ms,
            last_event_version = excluded.last_event_version
        "#,
        turso::params![
            pane.incarnation.pane_id.as_str(),
            i64::from(pane.incarnation.pane_pid),
            i64::from(pane.incarnation.agent_pid),
            pane.incarnation.agent_started_at_ms,
            pane.incarnation.provider_id.as_str(),
            pane.tmux_target.as_str(),
            pane.session_name.as_str(),
            i64::from(pane.window_index),
            i64::from(pane.pane_index),
            pane.working_directory.as_str(),
            pane.provider_display_name.as_str(),
            pane.observed_at_ms,
            projection.hook_state.map(observed_state_text),
            projection.hook_observed_at_ms,
            projection.screen_state.map(observed_state_text),
            projection.screen_classifier_id.as_deref(),
            projection.screen_observed_at_ms,
            effective_state_text(projection.effective_state),
            projection.explicit_work_summary.as_deref(),
            projection.explicit_work_summary_updated_at_ms,
            projection.screen_work_summary.as_deref(),
            projection.screen_work_summary_updated_at_ms,
            projection.work_summary.as_deref(),
            projection.last_transition_at_ms,
            projection.last_event_version.get(),
        ],
    )
    .await?;
    Ok(())
}

async fn load_agent_snapshot_from_one_query(
    conn: &turso::Connection,
) -> events::Result<AgentSnapshot> {
    let mut rows = conn
        .query(
            r#"
            WITH checkpoint AS (
                SELECT COALESCE((
                    SELECT last_processed_event_version
                    FROM last_processed_event
                    WHERE namespace = ?1 AND partition_key = ?2
                ), 0) AS through_event_version
            ), snapshot_rows AS (
                SELECT
                    'checkpoint' AS row_kind,
                    checkpoint.through_event_version,
                    NULL AS component,
                    NULL AS healthy,
                    NULL AS reason_code,
                    NULL AS health_observed_at_ms,
                    NULL AS health_last_event_version,
                    NULL AS pane_id,
                    NULL AS pane_pid,
                    NULL AS agent_pid,
                    NULL AS agent_started_at_ms,
                    NULL AS provider_id,
                    NULL AS tmux_target,
                    NULL AS session_name,
                    NULL AS window_index,
                    NULL AS pane_index,
                    NULL AS working_directory,
                    NULL AS provider_display_name,
                    NULL AS pane_observed_at_ms,
                    NULL AS hook_state,
                    NULL AS hook_observed_at_ms,
                    NULL AS screen_state,
                    NULL AS screen_classifier_id,
                    NULL AS screen_observed_at_ms,
                    NULL AS effective_state,
                    NULL AS explicit_work_summary,
                    NULL AS explicit_work_summary_updated_at_ms,
                    NULL AS screen_work_summary,
                    NULL AS screen_work_summary_updated_at_ms,
                    NULL AS work_summary,
                    NULL AS last_transition_at_ms,
                    NULL AS pane_last_event_version
                FROM checkpoint

                UNION ALL

                SELECT
                    'health',
                    checkpoint.through_event_version,
                    health.component,
                    health.healthy,
                    health.reason_code,
                    health.observed_at_ms,
                    health.last_event_version,
                    NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
                    NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
                    NULL, NULL, NULL
                FROM checkpoint, agent_monitor_health AS health

                UNION ALL

                SELECT
                    'pane',
                    checkpoint.through_event_version,
                    NULL, NULL, NULL, NULL, NULL,
                    pane.pane_id,
                    pane.pane_pid,
                    pane.agent_pid,
                    pane.agent_started_at_ms,
                    pane.provider_id,
                    pane.tmux_target,
                    pane.session_name,
                    pane.window_index,
                    pane.pane_index,
                    pane.working_directory,
                    pane.provider_display_name,
                    pane.pane_observed_at_ms,
                    pane.hook_state,
                    pane.hook_observed_at_ms,
                    pane.screen_state,
                    pane.screen_classifier_id,
                    pane.screen_observed_at_ms,
                    pane.effective_state,
                    pane.explicit_work_summary,
                    pane.explicit_work_summary_updated_at_ms,
                    pane.screen_work_summary,
                    pane.screen_work_summary_updated_at_ms,
                    pane.work_summary,
                    pane.last_transition_at_ms,
                    pane.last_event_version
                FROM checkpoint, agent_panes AS pane
            )
            SELECT * FROM snapshot_rows
            ORDER BY row_kind, component, pane_id
            "#,
            (NAMESPACE, PARTITION_KEY),
        )
        .await?;

    let mut snapshot = AgentSnapshot {
        through_event_version: EventStreamVersion::start(),
        server_time_ms: now_ms(),
        monitor_health: Vec::new(),
        panes: Vec::new(),
    };
    while let Some(row) = rows.next().await? {
        snapshot.through_event_version = event_stream_version(required_integer(&row, 1)?)?;
        match required_text(&row, 0)?.as_str() {
            "checkpoint" => {}
            "health" => snapshot.monitor_health.push(MonitorHealthProjection {
                component: required_text(&row, 2)?,
                healthy: required_integer(&row, 3)? != 0,
                reason_code: required_text(&row, 4)?,
                observed_at_ms: required_integer(&row, 5)?,
                last_event_version: event_stream_version(required_integer(&row, 6)?)?,
            }),
            "pane" => snapshot.panes.push(agent_pane_from_row_at(&row, 7)?),
            row_kind => {
                return Err(events::EsError::Cursor(format!(
                    "invalid snapshot row kind: {row_kind}"
                )));
            }
        }
    }
    Ok(snapshot)
}

async fn load_agent_pane(
    conn: &turso::Connection,
    pane_id: &str,
) -> events::Result<Option<AgentPaneProjection>> {
    let mut rows = conn
        .query(
            format!("SELECT {AGENT_PANE_COLUMNS} FROM agent_panes WHERE pane_id = ?1"),
            (pane_id,),
        )
        .await?;
    rows.next()
        .await?
        .map(|row| agent_pane_from_row(&row))
        .transpose()
}

fn agent_pane_from_row(row: &turso::Row) -> events::Result<AgentPaneProjection> {
    agent_pane_from_row_at(row, 0)
}

fn agent_pane_from_row_at(row: &turso::Row, offset: usize) -> events::Result<AgentPaneProjection> {
    Ok(AgentPaneProjection {
        pane: crate::agent::domain::AgentPaneObservation {
            incarnation: crate::agent::domain::AgentIncarnation {
                pane_id: required_text(row, offset)?,
                pane_pid: required_u32(row, offset + 1)?,
                agent_pid: required_u32(row, offset + 2)?,
                agent_started_at_ms: required_integer(row, offset + 3)?,
                provider_id: required_text(row, offset + 4)?,
            },
            tmux_target: required_text(row, offset + 5)?,
            session_name: required_text(row, offset + 6)?,
            window_index: required_u32(row, offset + 7)?,
            pane_index: required_u32(row, offset + 8)?,
            working_directory: required_text(row, offset + 9)?,
            provider_display_name: required_text(row, offset + 10)?,
            observed_at_ms: required_integer(row, offset + 11)?,
        },
        hook_state: optional_text(row, offset + 12)?
            .map(parse_observed_state)
            .transpose()?,
        hook_observed_at_ms: optional_integer(row, offset + 13)?,
        screen_state: optional_text(row, offset + 14)?
            .map(parse_observed_state)
            .transpose()?,
        screen_classifier_id: optional_text(row, offset + 15)?,
        screen_observed_at_ms: optional_integer(row, offset + 16)?,
        effective_state: parse_effective_state(&required_text(row, offset + 17)?)?,
        explicit_work_summary: optional_text(row, offset + 18)?,
        explicit_work_summary_updated_at_ms: optional_integer(row, offset + 19)?,
        screen_work_summary: optional_text(row, offset + 20)?,
        screen_work_summary_updated_at_ms: optional_integer(row, offset + 21)?,
        work_summary: optional_text(row, offset + 22)?,
        last_transition_at_ms: required_integer(row, offset + 23)?,
        last_event_version: EventStreamVersion::new(required_integer(row, offset + 24)?)?,
    })
}

fn required_text(row: &turso::Row, index: usize) -> events::Result<String> {
    row.get_value(index)?
        .as_text()
        .map(ToString::to_string)
        .ok_or_else(|| events::EsError::Cursor(format!("projection column {index} is not text")))
}

fn optional_text(row: &turso::Row, index: usize) -> events::Result<Option<String>> {
    let value = row.get_value(index)?;
    if value.is_null() {
        return Ok(None);
    }
    value
        .as_text()
        .map(|text| Some(text.to_string()))
        .ok_or_else(|| events::EsError::Cursor(format!("projection column {index} is not text")))
}

fn required_integer(row: &turso::Row, index: usize) -> events::Result<i64> {
    row.get_value(index)?.as_integer().copied().ok_or_else(|| {
        events::EsError::Cursor(format!("projection column {index} is not an integer"))
    })
}

fn optional_integer(row: &turso::Row, index: usize) -> events::Result<Option<i64>> {
    let value = row.get_value(index)?;
    if value.is_null() {
        return Ok(None);
    }
    value.as_integer().copied().map(Some).ok_or_else(|| {
        events::EsError::Cursor(format!("projection column {index} is not an integer"))
    })
}

fn required_u32(row: &turso::Row, index: usize) -> events::Result<u32> {
    u32::try_from(required_integer(row, index)?)
        .map_err(|_| events::EsError::Cursor(format!("projection column {index} is not a u32")))
}

fn event_stream_version(value: i64) -> events::Result<EventStreamVersion> {
    if value == 0 {
        Ok(EventStreamVersion::start())
    } else {
        EventStreamVersion::new(value)
    }
}

#[cfg(test)]
async fn pause_snapshot_read_after_query(gate: &std::sync::Mutex<Option<SnapshotReadGate>>) {
    let snapshot_gate = gate.lock().expect("snapshot read gate lock").take();
    if let Some(SnapshotReadGate { started, resume }) = snapshot_gate {
        let _ = started.send(());
        let _ = resume.await;
    }
}

fn observed_state_text(state: ObservedAgentState) -> &'static str {
    match state {
        ObservedAgentState::Busy => "busy",
        ObservedAgentState::Idle => "idle",
    }
}

fn parse_observed_state(value: String) -> events::Result<ObservedAgentState> {
    match value.as_str() {
        "busy" => Ok(ObservedAgentState::Busy),
        "idle" => Ok(ObservedAgentState::Idle),
        _ => Err(events::EsError::Cursor(format!(
            "invalid observed agent state: {value}"
        ))),
    }
}

fn effective_state_text(state: EffectiveAgentState) -> &'static str {
    match state {
        EffectiveAgentState::Busy => "busy",
        EffectiveAgentState::Idle => "idle",
        EffectiveAgentState::Unknown => "unknown",
    }
}

fn parse_effective_state(value: &str) -> events::Result<EffectiveAgentState> {
    match value {
        "busy" => Ok(EffectiveAgentState::Busy),
        "idle" => Ok(EffectiveAgentState::Idle),
        "unknown" => Ok(EffectiveAgentState::Unknown),
        _ => Err(events::EsError::Cursor(format!(
            "invalid effective agent state: {value}"
        ))),
    }
}

fn now_ms() -> i64 {
    (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
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
    let hook_grace_ms = crate::settings::get_settings().agent_monitor.hook_grace_ms;
    Ok(Arc::new(
        HaroldStore::open_with_hook_grace(path, hook_grace_ms).await?,
    ))
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

#[allow(
    dead_code,
    reason = "the serialized monitor runtime is the sole appender in the next monitor slice"
)]
pub(crate) async fn append_agent_events(
    store: &HaroldStore,
    events: Vec<AgentEvent>,
) -> events::Result<events::AppendResult> {
    let events = events
        .into_iter()
        .filter_map(normalize_agent_event)
        .map(agent_new_event)
        .collect::<events::Result<Vec<_>>>()?;
    store.stream.append(ExpectedVersion::Any, events).await
}

#[allow(
    dead_code,
    reason = "called by the runtime-facing agent append boundary above"
)]
fn normalize_agent_event(event: AgentEvent) -> Option<AgentEvent> {
    match event {
        AgentEvent::LifecycleObserved(mut lifecycle) => {
            if let WorkSummaryUpdate::Set(summary) = lifecycle.work_summary {
                lifecycle.work_summary = normalize_work_summary(&summary)
                    .map_or(WorkSummaryUpdate::Clear, WorkSummaryUpdate::Set);
            }
            Some(AgentEvent::LifecycleObserved(lifecycle))
        }
        AgentEvent::ScreenObserved(screen) => normalize_screen_event(screen),
        event => Some(event),
    }
}

#[allow(
    dead_code,
    reason = "called by the runtime-facing agent append boundary above"
)]
fn normalize_screen_event(mut screen: AgentScreenObserved) -> Option<AgentEvent> {
    screen.fallback_summary = screen
        .fallback_summary
        .as_deref()
        .and_then(normalize_work_summary);
    (screen.state.is_some() || screen.fallback_summary.is_some())
        .then_some(AgentEvent::ScreenObserved(screen))
}

#[allow(
    dead_code,
    reason = "called by the runtime-facing agent append boundary above"
)]
fn agent_new_event(event: AgentEvent) -> events::Result<NewEvent> {
    let (event_type, payload) = match event {
        AgentEvent::PaneObserved(event) => ("AgentPaneObserved", serde_json::to_value(event)?),
        AgentEvent::PaneDeparted(event) => ("AgentPaneDeparted", serde_json::to_value(event)?),
        AgentEvent::LifecycleObserved(event) => {
            ("AgentLifecycleObserved", serde_json::to_value(event)?)
        }
        AgentEvent::ScreenObserved(event) => ("AgentScreenObserved", serde_json::to_value(event)?),
        AgentEvent::MonitorHealthChanged(event) => {
            ("AgentMonitorHealthChanged", serde_json::to_value(event)?)
        }
    };
    Ok(NewEvent {
        r#type: event_type.into(),
        payload,
        workflow_kind: None,
        workflow: WorkflowRef::None,
        request_id: None,
        actor_id: "system:harold".into(),
        actor_type: ActorType::System,
    })
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
