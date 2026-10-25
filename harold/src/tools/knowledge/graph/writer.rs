//! The graph writer: one task that checks, logs and applies every change to the knowledge
//! graph, one at a time. Callers send a change and wait for the answer. The task finishes a
//! change even when its caller goes away, so the log and TypeDB never part between a log
//! record and its apply.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use tokio::sync::{mpsc, oneshot};
use tracing::{Instrument, info, warn};

use super::backend::{BackendError, GraphBackend, RelationRow, Snapshot};
use super::catchup::{self, Applied, CatchUpError, Refusal};
use super::log::{RecordLog, end_sequence};
use super::metrics;
use super::records::{By, DataRecord, Malformed, Op, SchemaRecord};
use super::schema;

/// Why a graph call is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GraphError {
    /// `invalid relation`
    InvalidRelation,
    /// `invalid vault_note`
    InvalidVaultNote,
    /// `invalid reason`
    InvalidReason,
    UnknownVaultNote,
    NotRefused,
    Off,
    Behind,
    Rebuilding,
    LogFailed,
    TypeDbUnreachable,
}

impl GraphError {
    /// The reason word a caller sees.
    pub(crate) fn reason(&self) -> &'static str {
        match self {
            Self::InvalidRelation => "invalid relation",
            Self::InvalidVaultNote => "invalid vault_note",
            Self::InvalidReason => "invalid reason",
            Self::UnknownVaultNote => "unknown_vault_note",
            Self::NotRefused => "not_refused",
            Self::Off => "graph_off",
            Self::Behind => "graph_behind",
            Self::Rebuilding => "graph_rebuilding",
            Self::LogFailed => "log_failed",
            Self::TypeDbUnreachable => "typedb_unreachable",
        }
    }
}

impl From<Malformed> for GraphError {
    fn from(malformed: Malformed) -> Self {
        match malformed {
            Malformed::Relation => Self::InvalidRelation,
            Malformed::VaultNote => Self::InvalidVaultNote,
            Malformed::Reason => Self::InvalidReason,
        }
    }
}

/// What a rebuild did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Rebuilt {
    pub applied: Applied,
    pub vault_notes: usize,
    pub links: usize,
    pub typed_relations: usize,
    pub ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RebuildReason {
    Asked,
    Missing,
}

impl RebuildReason {
    fn word(self) -> &'static str {
        match self {
            Self::Asked => "asked",
            Self::Missing => "missing",
        }
    }
}

/// The graph writer's state, as graph calls see it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    Ready,
    Rebuilding,
    /// TypeDB refused a record: the catch-up stopped there.
    Behind(Refusal),
    /// A record is logged and not applied yet, since TypeDB did not answer.
    Unapplied,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Status {
    pub schema_position: Option<u64>,
    pub data_position: Option<u64>,
    pub schema_end: Option<u64>,
    pub data_end: Option<u64>,
    pub waiting: usize,
    pub last_refusal: Option<Refusal>,
}

enum Command {
    Change {
        op: Op,
        by: By,
        reply: oneshot::Sender<Result<Option<u64>, GraphError>>,
    },
    Void {
        sequence: u64,
        reason: String,
        reply: oneshot::Sender<Result<u64, GraphError>>,
    },
    Rebuild {
        reply: oneshot::Sender<Result<Rebuilt, GraphError>>,
    },
}

struct Shared {
    phase: Mutex<Phase>,
    last_refusal: Mutex<Option<Refusal>>,
}

/// A handle to the graph writer. Cloning it shares the one writer.
#[derive(Clone)]
pub(crate) struct GraphWriter {
    commands: mpsc::Sender<Command>,
    shared: Arc<Shared>,
    backend: Arc<dyn GraphBackend>,
    schema_log: Arc<dyn RecordLog>,
    data_log: Arc<dyn RecordLog>,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl GraphWriter {
    /// Appends the schema steps the log lacks, makes the database if it is missing (a
    /// rebuild with reason `missing`), else catches it up, and starts the writer task.
    /// A record TypeDB refuses leaves the writer behind: calls answer `graph_behind`.
    pub(crate) async fn start(
        backend: Arc<dyn GraphBackend>,
        schema_log: Arc<dyn RecordLog>,
        data_log: Arc<dyn RecordLog>,
    ) -> Result<Self, GraphError> {
        let (commands, inbox) = mpsc::channel(64);
        let writer = Self {
            commands,
            shared: Arc::new(Shared {
                phase: Mutex::new(Phase::Ready),
                last_refusal: Mutex::new(None),
            }),
            backend,
            schema_log,
            data_log,
        };
        let mut task = Task {
            writer: writer.clone(),
        };
        task.open().await?;
        tokio::spawn(task.run(inbox));
        Ok(writer)
    }

    fn phase_error(&self) -> Option<GraphError> {
        match &*lock(&self.shared.phase) {
            Phase::Ready | Phase::Unapplied => None,
            Phase::Rebuilding => Some(GraphError::Rebuilding),
            Phase::Behind(_) => Some(GraphError::Behind),
        }
    }

    async fn ask<T>(
        &self,
        command: impl FnOnce(oneshot::Sender<Result<T, GraphError>>) -> Command,
    ) -> Result<T, GraphError> {
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(command(reply))
            .await
            .map_err(|_| GraphError::Off)?;
        answer.await.map_err(|_| GraphError::Off)?
    }

    /// Checks, logs and applies one change. Returns the data log sequence, or `None` when
    /// the graph already held the change, so nothing was logged.
    pub(crate) async fn submit(&self, op: Op, by: By) -> Result<Option<u64>, GraphError> {
        if let Some(error) = self.phase_error() {
            return Err(error);
        }
        self.ask(|reply| Command::Change { op, by, reply }).await
    }

    /// Appends a voided record for the record the catch-up stopped at, then goes on.
    pub(crate) async fn void(&self, sequence: u64, reason: String) -> Result<u64, GraphError> {
        if *lock(&self.shared.phase) == Phase::Rebuilding {
            return Err(GraphError::Rebuilding);
        }
        self.ask(|reply| Command::Void {
            sequence,
            reason,
            reply,
        })
        .await
    }

    pub(crate) async fn rebuild(&self) -> Result<Rebuilt, GraphError> {
        if *lock(&self.shared.phase) == Phase::Rebuilding {
            return Err(GraphError::Rebuilding);
        }
        self.ask(|reply| Command::Rebuild { reply }).await
    }

    pub(crate) async fn relations_of(&self, name: &str) -> Result<Vec<RelationRow>, GraphError> {
        if let Some(error) = self.phase_error() {
            return Err(error);
        }
        let state = self.backend.note_state(name).await.map_err(unreachable)?;
        if state.is_none() {
            return Err(GraphError::UnknownVaultNote);
        }
        self.backend.relations_of(name).await.map_err(unreachable)
    }

    pub(crate) async fn snapshot(&self) -> Result<Snapshot, GraphError> {
        if let Some(error) = self.phase_error() {
            return Err(error);
        }
        self.backend.snapshot().await.map_err(unreachable)
    }

    pub(crate) async fn status(&self) -> Result<Status, GraphError> {
        let position = self.backend.position().await.map_err(unreachable)?;
        let waiting = catchup::waiting(&*self.backend, &*self.schema_log, &*self.data_log)
            .await
            .map_err(|_| GraphError::LogFailed)?;
        Ok(Status {
            schema_position: position.schema,
            data_position: position.data,
            schema_end: end_sequence(&*self.schema_log)
                .await
                .map_err(|_| GraphError::LogFailed)?,
            data_end: end_sequence(&*self.data_log)
                .await
                .map_err(|_| GraphError::LogFailed)?,
            waiting,
            last_refusal: lock(&self.shared.last_refusal).clone(),
        })
    }
}

fn unreachable(_: BackendError) -> GraphError {
    GraphError::TypeDbUnreachable
}

struct Task {
    writer: GraphWriter,
}

impl Task {
    fn set_phase(&self, phase: Phase) {
        *lock(&self.writer.shared.phase) = phase;
    }

    async fn open(&mut self) -> Result<(), GraphError> {
        self.log_pending_steps().await?;
        let exists = self
            .writer
            .backend
            .database_exists()
            .await
            .map_err(unreachable)?;
        if !exists {
            return match self.rebuild(RebuildReason::Missing).await {
                Ok(_) | Err(GraphError::Behind) => Ok(()),
                Err(error) => Err(error),
            };
        }
        let caught_up = match self.catch_up().await {
            Ok(_) | Err(GraphError::Behind) => Ok(()),
            Err(error) => Err(error),
        };
        self.publish_metrics(true).await;
        caught_up
    }

    /// The records waiting, or 0 when the logs cannot be read.
    async fn waiting_now(&self) -> usize {
        catchup::waiting(
            &*self.writer.backend,
            &*self.writer.schema_log,
            &*self.writer.data_log,
        )
        .await
        .unwrap_or(0)
    }

    /// Publishes the two gauges' numbers. The typed relations are counted when asked, since
    /// it reads the whole graph; the records waiting are read from the logs and the position.
    async fn publish_metrics(&self, count_typed: bool) {
        let backend = &self.writer.backend;
        let Ok(waiting) =
            catchup::waiting(&**backend, &*self.writer.schema_log, &*self.writer.data_log).await
        else {
            return;
        };
        let typed = if count_typed {
            backend.snapshot().await.ok().map(|s| s.relations.len())
        } else {
            None
        };
        metrics::publish(waiting, typed);
    }

    /// Appends each schema step the schema log lacks, flushed.
    async fn log_pending_steps(&self) -> Result<(), GraphError> {
        let logged: Vec<SchemaRecord> = self
            .writer
            .schema_log
            .read_after(None)
            .await
            .map_err(|_| GraphError::LogFailed)?
            .iter()
            .filter_map(|(_, value)| serde_json::from_slice(value).ok())
            .collect();
        for step in schema::pending(&logged) {
            if !schema::only_adds(step.define) {
                return Err(GraphError::LogFailed);
            }
            let record = SchemaRecord {
                step: step.number,
                at_ms: now_ms(),
                define: step.define.to_string(),
            };
            let value = serde_json::to_vec(&record).map_err(|_| GraphError::LogFailed)?;
            self.writer
                .schema_log
                .append(value)
                .await
                .map_err(|_| GraphError::LogFailed)?;
        }
        Ok(())
    }

    /// Catches TypeDB up with the logs. A refusal makes the writer behind.
    async fn catch_up(&mut self) -> Result<Applied, GraphError> {
        let result = catchup::catch_up(
            &*self.writer.backend,
            &*self.writer.schema_log,
            &*self.writer.data_log,
        )
        .await;
        match result {
            Ok(applied) => {
                self.set_phase(Phase::Ready);
                // A catch-up may clear the records that waited; the gauge must show it.
                self.publish_metrics(false).await;
                Ok(applied)
            }
            Err(CatchUpError::Refused(refusal)) => {
                self.behind(refusal).await;
                Err(GraphError::Behind)
            }
            Err(CatchUpError::Unreachable) => {
                self.set_phase(Phase::Unapplied);
                Err(GraphError::TypeDbUnreachable)
            }
            Err(CatchUpError::Log(_)) => {
                // Not Rebuilding and not Ready: the next change tries the catch-up again.
                self.set_phase(Phase::Unapplied);
                Err(GraphError::LogFailed)
            }
        }
    }

    async fn behind(&self, refusal: Refusal) {
        let waiting = catchup::waiting(
            &*self.writer.backend,
            &*self.writer.schema_log,
            &*self.writer.data_log,
        )
        .await
        .unwrap_or(0);
        warn!(
            sequence = refusal.sequence,
            reason = %refusal.reason,
            waiting,
            "knowledge graph behind"
        );
        metrics::publish(waiting, None);
        *lock(&self.writer.shared.last_refusal) = Some(refusal.clone());
        self.set_phase(Phase::Behind(refusal));
    }

    async fn rebuild(&mut self, reason: RebuildReason) -> Result<Rebuilt, GraphError> {
        let span = tracing::info_span!(
            target: crate::telemetry::TRACE_TARGET,
            "harold.knowledge.graph_rebuild",
            reason = reason.word(),
            schema_steps = tracing::field::Empty,
            records = tracing::field::Empty,
            voided = tracing::field::Empty,
            ms = tracing::field::Empty,
            result = tracing::field::Empty,
        );
        let result = self
            .rebuild_inner(reason, &span)
            .instrument(span.clone())
            .await;
        if result.is_err() {
            span.record("result", "failed");
        }
        result
    }

    async fn rebuild_inner(
        &mut self,
        reason: RebuildReason,
        span: &tracing::Span,
    ) -> Result<Rebuilt, GraphError> {
        let started = Instant::now();
        self.set_phase(Phase::Rebuilding);
        let backend = Arc::clone(&self.writer.backend);
        let made = async {
            backend.drop_database().await?;
            backend.create_database().await
        }
        .await;
        if let Err(error) = made {
            self.set_phase(Phase::Unapplied);
            return Err(unreachable(error));
        }
        let applied = match self.catch_up().await {
            Ok(applied) => applied,
            Err(error) => {
                // A rebuild that stops at a refused record leaves the graph behind, or short.
                return Err(error);
            }
        };
        let snapshot = backend.snapshot().await.map_err(unreachable)?;
        let rebuilt = Rebuilt {
            applied,
            vault_notes: snapshot.notes.len(),
            links: snapshot.links.len(),
            typed_relations: snapshot.relations.len(),
            ms: started.elapsed().as_millis() as u64,
        };
        span.record("schema_steps", rebuilt.applied.schema_steps as u64);
        span.record("records", rebuilt.applied.records as u64);
        span.record("voided", rebuilt.applied.voided as u64);
        span.record("ms", rebuilt.ms);
        span.record("result", "ok");
        info!(
            reason = reason.word(),
            schema_steps = rebuilt.applied.schema_steps,
            records = rebuilt.applied.records,
            voided = rebuilt.applied.voided,
            ms = rebuilt.ms,
            "knowledge graph rebuilt"
        );
        Ok(rebuilt)
    }

    async fn run(mut self, mut inbox: mpsc::Receiver<Command>) {
        while let Some(command) = inbox.recv().await {
            match command {
                Command::Change { op, by, reply } => {
                    let typed = matches!(op, Op::PutRelation { .. } | Op::RemoveRelation { .. });
                    let result = self.change(op, by).await;
                    let logged = matches!(result, Ok(Some(_)));
                    let _ = reply.send(result);
                    if logged {
                        self.publish_metrics(typed).await;
                    }
                }
                Command::Void {
                    sequence,
                    reason,
                    reply,
                } => {
                    let _ = reply.send(self.void(sequence, reason).await);
                    self.publish_metrics(true).await;
                }
                Command::Rebuild { reply } => {
                    let _ = reply.send(self.rebuild(RebuildReason::Asked).await);
                    self.publish_metrics(true).await;
                }
            }
        }
    }

    async fn change(&mut self, op: Op, by: By) -> Result<Option<u64>, GraphError> {
        if matches!(op, Op::VoidRecord { .. }) {
            return Err(GraphError::InvalidRelation);
        }
        op.well_formed()?;
        // A change queued before a refusal must not pass the refused record.
        if let Some(error) = self.writer.phase_error() {
            return Err(error);
        }
        if *lock(&self.writer.shared.phase) == Phase::Unapplied {
            self.catch_up().await?;
        }
        self.check(&op).await?;
        if !self
            .writer
            .backend
            .would_change(&op)
            .await
            .map_err(unreachable)?
        {
            return Ok(None);
        }
        let sequence = self.log_and_apply(op, by).await?;
        Ok(Some(sequence))
    }

    /// The check against the graph: the vault notes a change names are there.
    async fn check(&self, op: &Op) -> Result<(), GraphError> {
        let backend = &self.writer.backend;
        let state = |name: &str| {
            let backend = Arc::clone(backend);
            let name = name.to_string();
            async move { backend.note_state(&name).await.map_err(unreachable) }
        };
        match op {
            Op::PutLink { from, to } => {
                for name in [from, to] {
                    if state(name).await?.is_none() {
                        return Err(GraphError::UnknownVaultNote);
                    }
                }
            }
            Op::PutRelation { from, to, .. } => {
                for name in [from, to] {
                    if state(name).await?.is_none_or(|state| state.missing) {
                        return Err(GraphError::UnknownVaultNote);
                    }
                }
            }
            // The pass must mark a vault note missing, never remove one a typed relation names.
            Op::RemoveVaultNote { name } => {
                let named = state(name).await?;
                if named.is_some_and(|state| state.in_typed_relation) {
                    return Err(GraphError::InvalidVaultNote);
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Steps 2 and 3: append and flush the record, then apply it with the position.
    async fn log_and_apply(&mut self, op: Op, by: By) -> Result<u64, GraphError> {
        let record = DataRecord {
            op,
            at_ms: now_ms(),
            by,
        };
        let value = serde_json::to_vec(&record).map_err(|_| GraphError::LogFailed)?;
        let sequence = self
            .writer
            .data_log
            .append(value)
            .await
            .map_err(|_| GraphError::LogFailed)?;
        match self.writer.backend.apply(&record.op, sequence).await {
            Ok(()) => Ok(sequence),
            Err(BackendError::Unreachable) => {
                self.set_phase(Phase::Unapplied);
                // The record is logged and waits: the gauge must show it.
                metrics::publish(self.waiting_now().await.max(1), None);
                Err(GraphError::TypeDbUnreachable)
            }
            Err(BackendError::Refused(reason)) => {
                self.behind(Refusal { sequence, reason }).await;
                Err(GraphError::Behind)
            }
        }
    }

    async fn void(&mut self, sequence: u64, reason: String) -> Result<u64, GraphError> {
        let op = Op::VoidRecord {
            sequence,
            reason: reason.clone(),
        };
        op.well_formed()?;
        let refused = match &*lock(&self.writer.shared.phase) {
            Phase::Behind(refusal) => Some(refusal.sequence),
            _ => None,
        };
        if refused != Some(sequence) {
            return Err(GraphError::NotRefused);
        }
        let record = DataRecord {
            op,
            at_ms: now_ms(),
            by: By::Call,
        };
        let value = serde_json::to_vec(&record).map_err(|_| GraphError::LogFailed)?;
        let void_sequence = self
            .writer
            .data_log
            .append(value)
            .await
            .map_err(|_| GraphError::LogFailed)?;
        warn!(sequence, reason = %reason, "knowledge graph record voided");
        self.catch_up().await?;
        Ok(void_sequence)
    }
}

#[cfg(test)]
#[path = "writer_tests.rs"]
mod tests;
