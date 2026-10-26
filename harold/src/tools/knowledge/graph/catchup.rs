//! Catch-up and replay: apply every logged record whose sequence is greater than the graph
//! position, the schema log first, then the data log, passing each voided record. A log that
//! started again below the position is read from its start.

use std::collections::HashSet;

use super::backend::{BackendError, GraphBackend, Position, Which};
use super::log::{LogError, RecordLog};
use super::records::{DataRecord, Op, SchemaRecord};

/// What a catch-up applied.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Applied {
    pub schema_steps: usize,
    pub records: usize,
    pub voided: usize,
    /// The logs read from their start, since they started again below the graph position.
    pub restarted: Vec<Which>,
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

/// A log started again when it holds a position but its last sequence is below it, or it is empty.
fn started_again(position: Option<u64>, end: Option<u64>) -> bool {
    position.is_some_and(|position| end.is_none_or(|end| end < position))
}

/// The sequence to read after: `None` (read from the start) for a log that started again,
/// else the position.
pub(crate) fn start_after(position: Option<u64>, end: Option<u64>) -> Option<u64> {
    if started_again(position, end) {
        None
    } else {
        position
    }
}

/// Reads the records to apply from one log, and whether the log is read from its start
/// because it started again or because the caller asked for it.
async fn read_to_apply(
    log: &dyn RecordLog,
    position: Option<u64>,
    forced: bool,
) -> Result<(bool, Vec<(u64, Vec<u8>)>), LogError> {
    let Some(position) = position.filter(|_| !forced) else {
        return Ok((forced, log.read_after(None).await?));
    };
    // A log that did not start again holds a record at or above the position, so the read from
    // the position on is not empty. An empty read means the log started again.
    let mut records = log.read_after(position.checked_sub(1)).await?;
    let end = records.last().map(|(sequence, _)| *sequence);
    let Some(after) = start_after(Some(position), end) else {
        return Ok((true, log.read_after(None).await?));
    };
    records.retain(|(sequence, _)| *sequence > after);
    Ok((false, records))
}

#[cfg(test)]
/// Applies the schema log, then the data log, after the graph position. A log that started
/// again, or one on `from_start`, is read from its start.
pub(crate) async fn catch_up(
    backend: &dyn GraphBackend,
    schema_log: &dyn RecordLog,
    data_log: &dyn RecordLog,
    from_start: &[Which],
) -> Result<Applied, CatchUpError> {
    let mut applied = Applied::default();
    catch_up_into(backend, schema_log, data_log, from_start, &mut applied).await?;
    Ok(applied)
}

/// The catch-up, counting into `applied` as it goes, so a caller still sees the logs found
/// started again and the records applied when the catch-up stops at an error.
pub(crate) async fn catch_up_into(
    backend: &dyn GraphBackend,
    schema_log: &dyn RecordLog,
    data_log: &dyn RecordLog,
    from_start: &[Which],
    applied: &mut Applied,
) -> Result<(), CatchUpError> {
    let position = backend.position().await.map_err(position_error)?;
    let (schema_restarted, schema_records) = read_to_apply(
        schema_log,
        position.schema,
        from_start.contains(&Which::Schema),
    )
    .await?;
    let (data_restarted, records) =
        read_to_apply(data_log, position.data, from_start.contains(&Which::Data)).await?;
    if schema_restarted {
        applied.restarted.push(Which::Schema);
    }
    if data_restarted {
        applied.restarted.push(Which::Data);
    }
    for (sequence, value) in schema_records {
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
    Ok(())
}

/// How many records wait: those with a sequence greater than the position, in both logs, and
/// all records of a log that started again.
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
    let schema = read_to_apply(schema_log, position.schema, false).await?.1;
    let data = read_to_apply(data_log, position.data, false).await?.1;
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
