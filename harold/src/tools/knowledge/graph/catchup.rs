//! Catch-up and replay: apply every logged record whose sequence is greater than the graph
//! position, the schema log first, then the data log, passing each voided record.

use std::collections::HashSet;

use super::backend::{BackendError, GraphBackend, Position, Which};
use super::log::{LogError, RecordLog};
use super::records::{DataRecord, Op, SchemaRecord};

/// What a catch-up applied.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Applied {
    pub schema_steps: usize,
    pub records: usize,
    pub voided: usize,
}

/// A record the database still refuses, or cannot be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Refusal {
    pub sequence: u64,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CatchUpError {
    Log(LogError),
    Unreachable,
    Refused(Refusal),
}

impl From<LogError> for CatchUpError {
    fn from(error: LogError) -> Self {
        Self::Log(error)
    }
}

/// A failed read of the position is never a refused record: no record is at fault.
fn position_error(_: BackendError) -> CatchUpError {
    CatchUpError::Unreachable
}

fn backend_error(sequence: u64, error: BackendError) -> CatchUpError {
    match error {
        BackendError::Unreachable => CatchUpError::Unreachable,
        BackendError::Refused(reason) => CatchUpError::Refused(Refusal { sequence, reason }),
    }
}

/// Applies the schema log, then the data log, after the graph position.
pub(crate) async fn catch_up(
    backend: &dyn GraphBackend,
    schema_log: &dyn RecordLog,
    data_log: &dyn RecordLog,
) -> Result<Applied, CatchUpError> {
    let position = backend.position().await.map_err(position_error)?;
    let mut applied = Applied::default();
    for (sequence, value) in schema_log.read_after(position.schema).await? {
        let record: SchemaRecord = serde_json::from_slice(&value).map_err(|_| {
            CatchUpError::Refused(Refusal {
                sequence,
                reason: "undecodable_schema".into(),
            })
        })?;
        backend
            .apply_schema(&record, sequence)
            .await
            .map_err(|e| backend_error(sequence, e))?;
        applied.schema_steps += 1;
    }
    let records = data_log.read_after(position.data).await?;
    // A void is logged after the record it names, so it is in this slice whenever that record is.
    let mut voided = HashSet::new();
    let mut decoded = Vec::with_capacity(records.len());
    for (_, value) in &records {
        let record = serde_json::from_slice::<DataRecord>(value).ok();
        if let Some(DataRecord {
            op: Op::VoidRecord {
                sequence: target, ..
            },
            ..
        }) = &record
        {
            voided.insert(*target);
        }
        decoded.push(record);
    }
    for ((sequence, _), record) in records.iter().zip(decoded) {
        if voided.contains(sequence) {
            backend
                .advance(Which::Data, *sequence)
                .await
                .map_err(|e| backend_error(*sequence, e))?;
            applied.voided += 1;
            continue;
        }
        let Some(record) = record else {
            return Err(CatchUpError::Refused(Refusal {
                sequence: *sequence,
                reason: "undecodable".into(),
            }));
        };
        backend
            .apply(&record.op, *sequence)
            .await
            .map_err(|e| backend_error(*sequence, e))?;
        if !matches!(record.op, Op::VoidRecord { .. }) {
            applied.records += 1;
        }
    }
    Ok(applied)
}

/// How many records wait: those with a sequence greater than the position, in both logs.
pub(crate) async fn waiting(
    backend: &dyn GraphBackend,
    schema_log: &dyn RecordLog,
    data_log: &dyn RecordLog,
) -> Result<usize, CatchUpError> {
    let position = backend.position().await.map_err(position_error)?;
    Ok(waiting_from(position, schema_log, data_log).await?.count)
}

/// The records that wait after a position read earlier.
pub(crate) struct Waiting {
    pub count: usize,
    /// The sequence of the first record that waits: a schema record when one waits, else a data record.
    pub first: Option<u64>,
}

pub(crate) async fn waiting_from(
    position: Position,
    schema_log: &dyn RecordLog,
    data_log: &dyn RecordLog,
) -> Result<Waiting, LogError> {
    let schema = schema_log.read_after(position.schema).await?;
    let data = data_log.read_after(position.data).await?;
    Ok(Waiting {
        count: schema.len() + data.len(),
        first: schema
            .first()
            .or(data.first())
            .map(|(sequence, _)| *sequence),
    })
}

#[cfg(test)]
#[path = "catchup_tests.rs"]
mod tests;
