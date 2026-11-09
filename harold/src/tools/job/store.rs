//! The job tool's own database, `jobs.db`, apart from Harold's core store.

use std::path::Path;
use std::time::Duration;

use tokio::sync::Mutex;
use turso::{Connection, Value};

const DATABASE: &str = "jobs.db";

/// How long an ended job stays in the list and in the snapshot: 10 minutes.
pub(super) const SHOW_AFTER_END_MS: i64 = 10 * 60_000;
/// How long a finished job stays in the file: 7 days.
const KEEP_MS: i64 = 7 * 24 * 3_600_000;

#[derive(Debug, thiserror::Error)]
pub(super) enum StoreError {
    #[error("job database: {0}")]
    Turso(#[from] turso::Error),
    #[error("job data: {0}")]
    Data(String),
}

type Result<T> = std::result::Result<T, StoreError>;

/// A job to store: `add` makes it running with no end line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct NewJob {
    pub name: String,
    pub what: String,
    pub pane_id: String,
    pub pid: u32,
    /// The start time of the process in seconds, read when the job is accepted.
    pub process_start_s: i64,
    pub started_at_ms: i64,
    pub due_at_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum JobState {
    Running,
    Ended,
    Removed,
}

impl JobState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Ended => "ended",
            Self::Removed => "removed",
        }
    }

    fn parse(text: &str) -> Result<Self> {
        match text {
            "running" => Ok(Self::Running),
            "ended" => Ok(Self::Ended),
            "removed" => Ok(Self::Removed),
            other => Err(StoreError::Data(format!("unknown state {other}"))),
        }
    }
}

/// A stored job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct JobRow {
    pub id: i64,
    pub name: String,
    pub what: String,
    pub pane_id: String,
    pub pid: u32,
    pub process_start_s: i64,
    pub started_at_ms: i64,
    pub due_at_ms: i64,
    /// 0 while the job runs.
    pub ended_at_ms: i64,
    pub state: JobState,
    /// `None` until the end line is done. Then `typed`, `typed_at_turn_end`, `no_pane`
    /// or `failed`. A removed job has `None` and gets no line.
    pub end_line: Option<String>,
}

const COLUMNS: &str = "id, name, what, pane_id, pid, process_start_s, started_at_ms, \
    due_at_ms, ended_at_ms, state, end_line";

pub(super) struct JobStore {
    conn: Mutex<Connection>,
    // The database handle stays alive as long as the connection is used.
    _database: turso::Database,
}

impl JobStore {
    pub(super) async fn open(folder: &Path) -> Result<Self> {
        tokio::fs::create_dir_all(folder)
            .await
            .map_err(|e| StoreError::Data(format!("create {}: {e}", folder.display())))?;
        let path = folder.join(DATABASE);
        let path = path
            .to_str()
            .ok_or_else(|| StoreError::Data("job database path is not UTF-8".into()))?;
        let database = turso::Builder::new_local(path).build().await?;
        let conn = database.connect()?;
        conn.busy_timeout(Duration::from_millis(5_000))?;
        let mut rows = conn.query("PRAGMA journal_mode = WAL", ()).await?;
        while rows.next().await?.is_some() {}
        conn.execute("PRAGMA synchronous = NORMAL", ()).await?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS jobs (\
                id INTEGER PRIMARY KEY AUTOINCREMENT, \
                name TEXT NOT NULL, \
                what TEXT NOT NULL, \
                pane_id TEXT NOT NULL, \
                pid INTEGER NOT NULL, \
                process_start_s INTEGER NOT NULL, \
                started_at_ms INTEGER NOT NULL, \
                due_at_ms INTEGER NOT NULL, \
                ended_at_ms INTEGER NOT NULL DEFAULT 0, \
                state TEXT NOT NULL CHECK (state IN ('running','ended','removed')), \
                end_line TEXT)",
            (),
        )
        .await?;
        Ok(Self {
            conn: Mutex::new(conn),
            _database: database,
        })
    }

    /// Stores the job as running. Returns `false`, and stores nothing, when a job of the
    /// same name runs.
    pub(super) async fn add(&self, job: &NewJob) -> Result<bool> {
        let conn = self.conn.lock().await;
        begin(&conn).await?;
        let work = async {
            let mut rows = conn
                .query(
                    "SELECT 1 FROM jobs WHERE name = ?1 AND state = 'running'",
                    [job.name.as_str()],
                )
                .await?;
            if rows.next().await?.is_some() {
                return Ok(false);
            }
            conn.execute(
                "INSERT INTO jobs (name, what, pane_id, pid, process_start_s, started_at_ms, \
                 due_at_ms, ended_at_ms, state, end_line) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, 'running', NULL)",
                turso::params_from_iter(vec![
                    Value::from(job.name.as_str()),
                    Value::from(job.what.as_str()),
                    Value::from(job.pane_id.as_str()),
                    Value::from(i64::from(job.pid)),
                    Value::from(job.process_start_s),
                    Value::from(job.started_at_ms),
                    Value::from(job.due_at_ms),
                ]),
            )
            .await?;
            Ok(true)
        }
        .await;
        finish(&conn, work).await
    }

    /// Every running job, oldest first.
    pub(super) async fn running(&self) -> Result<Vec<JobRow>> {
        self.fetch("state = 'running'", ()).await
    }

    /// The jobs that ended and have no end line yet, oldest first.
    pub(super) async fn waiting_for_line(&self) -> Result<Vec<JobRow>> {
        self.fetch("state = 'ended' AND end_line IS NULL", ()).await
    }

    /// The running jobs, and the jobs that ended in the last 10 minutes before `now_ms`.
    pub(super) async fn shown(&self, now_ms: i64) -> Result<Vec<JobRow>> {
        self.fetch(
            "state = 'running' OR (state = 'ended' AND ended_at_ms > ?1)",
            [now_ms - SHOW_AFTER_END_MS],
        )
        .await
    }

    /// Marks a running job as ended at `now_ms`. Returns `false` when it did not run.
    pub(super) async fn mark_ended(&self, id: i64, now_ms: i64) -> Result<bool> {
        let conn = self.conn.lock().await;
        begin(&conn).await?;
        let work = async {
            let changed = conn
                .execute(
                    "UPDATE jobs SET state = 'ended', ended_at_ms = ?2 \
                     WHERE id = ?1 AND state = 'running'",
                    turso::params_from_iter(vec![Value::from(id), Value::from(now_ms)]),
                )
                .await?;
            Ok(changed > 0)
        }
        .await;
        finish(&conn, work).await
    }

    /// Records how the end line went for an ended job.
    pub(super) async fn set_end_line(&self, id: i64, outcome: &str) -> Result<()> {
        let conn = self.conn.lock().await;
        begin(&conn).await?;
        let work = async {
            conn.execute(
                "UPDATE jobs SET end_line = ?2 WHERE id = ?1 AND state = 'ended'",
                turso::params_from_iter(vec![Value::from(id), Value::from(outcome)]),
            )
            .await?;
            Ok(())
        }
        .await;
        finish(&conn, work).await
    }

    /// Takes away a running job. It is not an end: no line follows. Returns the job, or
    /// `None` when no job of that name runs.
    pub(super) async fn remove(&self, name: &str, now_ms: i64) -> Result<Option<JobRow>> {
        let conn = self.conn.lock().await;
        begin(&conn).await?;
        let work = async {
            let mut rows = conn
                .query(
                    &format!("SELECT {COLUMNS} FROM jobs WHERE name = ?1 AND state = 'running'"),
                    [name],
                )
                .await?;
            let Some(row) = rows.next().await? else {
                return Ok(None);
            };
            let mut job = to_job(&row)?;
            drop(rows);
            conn.execute(
                "UPDATE jobs SET state = 'removed', ended_at_ms = ?2 WHERE id = ?1",
                turso::params_from_iter(vec![Value::from(job.id), Value::from(now_ms)]),
            )
            .await?;
            job.state = JobState::Removed;
            job.ended_at_ms = now_ms;
            Ok(Some(job))
        }
        .await;
        finish(&conn, work).await
    }

    /// Deletes the jobs that finished more than 7 days before `now_ms`.
    pub(super) async fn prune(&self, now_ms: i64) -> Result<()> {
        let conn = self.conn.lock().await;
        begin(&conn).await?;
        let work = async {
            conn.execute(
                "DELETE FROM jobs WHERE state <> 'running' AND ended_at_ms > 0 \
                 AND ended_at_ms < ?1 AND (end_line IS NOT NULL OR state = 'removed')",
                [now_ms - KEEP_MS],
            )
            .await?;
            Ok(())
        }
        .await;
        finish(&conn, work).await
    }

    async fn fetch(&self, condition: &str, params: impl turso::IntoParams) -> Result<Vec<JobRow>> {
        let conn = self.conn.lock().await;
        roll_back_abandoned(&conn).await?;
        let mut rows = conn
            .query(
                &format!("SELECT {COLUMNS} FROM jobs WHERE {condition} ORDER BY started_at_ms, id"),
                params,
            )
            .await?;
        let mut jobs = Vec::new();
        while let Some(row) = rows.next().await? {
            jobs.push(to_job(&row)?);
        }
        Ok(jobs)
    }
}

async fn begin(conn: &Connection) -> Result<()> {
    roll_back_abandoned(conn).await?;
    conn.execute("BEGIN IMMEDIATE", ()).await?;
    Ok(())
}

/// A caller dropped between `begin` and `finish` leaves the connection inside its
/// transaction, so that one is rolled back first.
async fn roll_back_abandoned(conn: &Connection) -> Result<()> {
    if !conn.is_autocommit()? {
        conn.execute("ROLLBACK", ()).await?;
    }
    Ok(())
}

async fn finish<T>(conn: &Connection, work: Result<T>) -> Result<T> {
    match work {
        Ok(value) => {
            if let Err(error) = conn.execute("COMMIT", ()).await {
                let _ = conn.execute("ROLLBACK", ()).await;
                return Err(error.into());
            }
            Ok(value)
        }
        Err(error) => {
            let _ = conn.execute("ROLLBACK", ()).await;
            Err(error)
        }
    }
}

fn integer(row: &turso::Row, index: usize) -> Result<i64> {
    row.get_value(index)?
        .as_integer()
        .copied()
        .ok_or_else(|| StoreError::Data(format!("column {index} is not an integer")))
}

fn text(row: &turso::Row, index: usize) -> Result<String> {
    row.get_value(index)?
        .as_text()
        .cloned()
        .ok_or_else(|| StoreError::Data(format!("column {index} is not text")))
}

fn to_job(row: &turso::Row) -> Result<JobRow> {
    let end_line = {
        let value = row.get_value(10)?;
        if value.is_null() {
            None
        } else {
            Some(text(row, 10)?)
        }
    };
    Ok(JobRow {
        id: integer(row, 0)?,
        name: text(row, 1)?,
        what: text(row, 2)?,
        pane_id: text(row, 3)?,
        pid: u32::try_from(integer(row, 4)?)
            .map_err(|_| StoreError::Data("pid out of range".into()))?,
        process_start_s: integer(row, 5)?,
        started_at_ms: integer(row, 6)?,
        due_at_ms: integer(row, 7)?,
        ended_at_ms: integer(row, 8)?,
        state: JobState::parse(&text(row, 9)?)?,
        end_line,
    })
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
