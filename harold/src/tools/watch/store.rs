//! The watch tool's own database, apart from Harold's core store.

use std::path::Path;
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use turso::{Connection, Value};

const DATABASE: &str = "watch.db";

const MIGRATIONS: [(&str, &str); 1] = [("001_watches", include_str!("migrations/001_watches.sql"))];

#[derive(Debug, thiserror::Error)]
pub(super) enum StoreError {
    #[error("watch database: {0}")]
    Turso(#[from] turso::Error),
    #[error("watch data: {0}")]
    Data(String),
}

type Result<T> = std::result::Result<T, StoreError>;

/// A watch to store: `put_watch` makes it running with no runs.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct NewWatch {
    pub name: String,
    pub folder: String,
    pub skill: String,
    pub report_to: String,
    pub every_minutes: u32,
    pub every_run: bool,
    pub run_budget_usd_micros: Option<i64>,
    pub started_at_ms: i64,
    pub ends_at_ms: i64,
    pub next_run_at_ms: i64,
}

/// A stored watch.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct WatchRow {
    pub name: String,
    pub folder: String,
    pub skill: String,
    pub report_to: String,
    pub every_minutes: u32,
    pub every_run: bool,
    pub run_budget_usd_micros: Option<i64>,
    pub running: bool,
    pub started_at_ms: i64,
    pub ends_at_ms: i64,
    pub next_run_at_ms: i64,
    pub runs: u32,
    pub reports: u32,
    pub failed_in_a_row: u32,
    pub cost_usd_micros: i64,
    pub last_outcome: Option<String>,
    pub stop_reason: Option<String>,
    pub ended_at_ms: Option<i64>,
}

/// What a finished run does to the count of runs failed in a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Streak {
    /// One more failure.
    Failed,
    /// A run that worked: back to 0.
    Worked,
    /// A skipped run says nothing about the watch.
    Unchanged,
}

/// One finished run to record.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct RunRecord {
    pub started_at_ms: i64,
    /// `quiet`, `report` or a failure reason.
    pub outcome: String,
    /// The run's reported cost, else its run budget, else `None`.
    pub cost_usd_micros: Option<i64>,
    pub streak: Streak,
    /// When the watch's next run is due.
    pub next_run_at_ms: i64,
}

const COLUMNS: &str = "name, folder, skill, report_to, every_minutes, every_run, \
    run_budget_usd_micros, state, started_at_ms, ends_at_ms, next_run_at_ms, runs, reports, \
    failed_in_a_row, cost_usd_micros, last_outcome, stop_reason, ended_at_ms";

pub(super) struct WatchStore {
    conn: Mutex<Connection>,
    // The database handle stays alive as long as the connection is used.
    _database: turso::Database,
}

impl WatchStore {
    pub(super) async fn open(folder: &Path) -> Result<Self> {
        tokio::fs::create_dir_all(folder)
            .await
            .map_err(|e| StoreError::Data(format!("create {}: {e}", folder.display())))?;
        let path = folder.join(DATABASE);
        let path = path
            .to_str()
            .ok_or_else(|| StoreError::Data("watch database path is not UTF-8".into()))?;
        let database = turso::Builder::new_local(path).build().await?;
        let conn = database.connect()?;
        configure(&conn).await?;
        begin(&conn).await?;
        finish(&conn, apply_migrations(&conn).await).await?;
        Ok(Self {
            conn: Mutex::new(conn),
            _database: database,
        })
    }

    /// Stores the watch as running with no runs, replacing any watch of the same name.
    pub(super) async fn put_watch(&self, watch: &NewWatch) -> Result<()> {
        let conn = self.conn.lock().await;
        begin(&conn).await?;
        let work = async {
            conn.execute(
                "INSERT OR REPLACE INTO watches (name, folder, skill, report_to, every_minutes, \
                 every_run, run_budget_usd_micros, state, started_at_ms, ends_at_ms, \
                 next_run_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'running', ?8, ?9, ?10)",
                turso::params_from_iter(vec![
                    Value::from(watch.name.as_str()),
                    Value::from(watch.folder.as_str()),
                    Value::from(watch.skill.as_str()),
                    Value::from(watch.report_to.as_str()),
                    Value::from(i64::from(watch.every_minutes)),
                    Value::from(i64::from(watch.every_run)),
                    optional(watch.run_budget_usd_micros),
                    Value::from(watch.started_at_ms),
                    Value::from(watch.ends_at_ms),
                    Value::from(watch.next_run_at_ms),
                ]),
            )
            .await?;
            Ok(())
        }
        .await;
        finish(&conn, work).await
    }

    pub(super) async fn get(&self, name: &str) -> Result<Option<WatchRow>> {
        let conn = self.conn.lock().await;
        roll_back_abandoned(&conn).await?;
        fetch_one(&conn, name).await
    }

    /// Every running watch, oldest first.
    pub(super) async fn running(&self) -> Result<Vec<WatchRow>> {
        let conn = self.conn.lock().await;
        roll_back_abandoned(&conn).await?;
        fetch_many(&conn, "state = 'running'").await
    }

    /// Every watch that ended at or after `since_ms`.
    pub(super) async fn ended_since(&self, since_ms: i64) -> Result<Vec<WatchRow>> {
        let conn = self.conn.lock().await;
        roll_back_abandoned(&conn).await?;
        fetch_many(
            &conn,
            &format!("state = 'stopped' AND ended_at_ms >= {since_ms}"),
        )
        .await
    }

    /// Records a finished run, and updates the watch if it is still running. A run of a
    /// watch that was stopped or replaced meanwhile is still recorded in `runs`, since
    /// its cost was spent. Returns the updated watch, or `None` when the watch is no
    /// longer running.
    pub(super) async fn record_run(
        &self,
        name: &str,
        started_at_ms: i64,
        record: &RunRecord,
    ) -> Result<Option<WatchRow>> {
        let conn = self.conn.lock().await;
        begin(&conn).await?;
        let work = async {
            let Some(before) = fetch_one(&conn, name)
                .await?
                .filter(|row| row.running && row.started_at_ms == started_at_ms)
            else {
                return Ok(None);
            };
            let run = before.runs + 1;
            conn.execute(
                "INSERT INTO runs (watch, folder, run, started_at_ms, outcome, cost_usd_micros) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                turso::params_from_iter(vec![
                    Value::from(name),
                    Value::from(before.folder.as_str()),
                    Value::from(i64::from(run)),
                    Value::from(record.started_at_ms),
                    Value::from(record.outcome.as_str()),
                    optional(record.cost_usd_micros),
                ]),
            )
            .await?;
            conn.execute(
                "UPDATE watches SET runs = runs + 1, reports = reports + ?2, \
                 failed_in_a_row = CASE ?3 WHEN 1 THEN failed_in_a_row + 1 WHEN 0 THEN 0 \
                 ELSE failed_in_a_row END, \
                 cost_usd_micros = cost_usd_micros + ?4, last_outcome = ?5, next_run_at_ms = ?6 \
                 WHERE name = ?1",
                turso::params_from_iter(vec![
                    Value::from(name),
                    Value::from(i64::from(record.outcome == "report")),
                    Value::from(match record.streak {
                        Streak::Failed => 1_i64,
                        Streak::Worked => 0,
                        Streak::Unchanged => 2,
                    }),
                    Value::from(record.cost_usd_micros.unwrap_or(0)),
                    Value::from(record.outcome.as_str()),
                    Value::from(record.next_run_at_ms),
                ]),
            )
            .await?;
            fetch_one(&conn, name).await
        }
        .await;
        finish(&conn, work).await
    }

    /// Records a run of a watch that is no longer running (stopped or replaced while it
    /// ran): only its cost, in `runs`.
    pub(super) async fn record_orphan_run(
        &self,
        name: &str,
        folder: &str,
        run: u32,
        record: &RunRecord,
    ) -> Result<()> {
        let conn = self.conn.lock().await;
        begin(&conn).await?;
        let work = async {
            conn.execute(
                "INSERT INTO runs (watch, folder, run, started_at_ms, outcome, cost_usd_micros) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                turso::params_from_iter(vec![
                    Value::from(name),
                    Value::from(folder),
                    Value::from(i64::from(run)),
                    Value::from(record.started_at_ms),
                    Value::from(record.outcome.as_str()),
                    optional(record.cost_usd_micros),
                ]),
            )
            .await?;
            Ok(())
        }
        .await;
        finish(&conn, work).await
    }

    /// Ends a running watch with `reason`. Returns the ended watch, or `None` when the
    /// watch is not running.
    pub(super) async fn end(
        &self,
        name: &str,
        started_at_ms: i64,
        reason: &str,
        now_ms: i64,
    ) -> Result<Option<WatchRow>> {
        let conn = self.conn.lock().await;
        begin(&conn).await?;
        let work = async {
            let changed = conn
                .execute(
                    "UPDATE watches SET state = 'stopped', stop_reason = ?2, ended_at_ms = ?3 \
                     WHERE name = ?1 AND state = 'running' AND started_at_ms = ?4",
                    turso::params_from_iter(vec![
                        Value::from(name),
                        Value::from(reason),
                        Value::from(now_ms),
                        Value::from(started_at_ms),
                    ]),
                )
                .await?;
            if changed == 0 {
                return Ok(None);
            }
            fetch_one(&conn, name).await
        }
        .await;
        finish(&conn, work).await
    }

    /// How many of the watch's runs since `started_at_ms` were skipped. The start keeps an
    /// earlier watch of the same name out of the count.
    pub(super) async fn skipped_runs(&self, name: &str, started_at_ms: i64) -> Result<u32> {
        let conn = self.conn.lock().await;
        roll_back_abandoned(&conn).await?;
        let mut rows = conn
            .query(
                "SELECT COUNT(*) FROM runs \
                 WHERE watch = ?1 AND outcome = 'skipped' AND started_at_ms >= ?2",
                turso::params_from_iter(vec![Value::from(name), Value::from(started_at_ms)]),
            )
            .await?;
        let Some(row) = rows.next().await? else {
            return Ok(0);
        };
        Ok(integer(&row, 0)? as u32)
    }

    /// Whether a daily budget stopped any watch at or after `day_start_ms`.
    pub(super) async fn budget_stopped_since(&self, day_start_ms: i64) -> Result<bool> {
        let conn = self.conn.lock().await;
        roll_back_abandoned(&conn).await?;
        let mut rows = conn
            .query(
                "SELECT COUNT(*) FROM watches \
                 WHERE stop_reason = 'daily_budget_spent' AND ended_at_ms >= ?1",
                [day_start_ms],
            )
            .await?;
        let Some(row) = rows.next().await? else {
            return Ok(false);
        };
        Ok(integer(&row, 0)? > 0)
    }

    /// What the runs started in `[from_ms, to_ms)` cost, in millionths of a dollar.
    /// A run whose cost is unknown counts as nothing.
    pub(super) async fn spent_micros(&self, from_ms: i64, to_ms: i64) -> Result<i64> {
        let conn = self.conn.lock().await;
        roll_back_abandoned(&conn).await?;
        let mut rows = conn
            .query(
                "SELECT COALESCE(SUM(cost_usd_micros), 0) FROM runs \
                 WHERE started_at_ms >= ?1 AND started_at_ms < ?2",
                [from_ms, to_ms],
            )
            .await?;
        let Some(row) = rows.next().await? else {
            return Ok(0);
        };
        integer(&row, 0)
    }
}

fn optional(value: Option<i64>) -> Value {
    value.map_or(Value::Null, Value::from)
}

async fn fetch_one(conn: &Connection, name: &str) -> Result<Option<WatchRow>> {
    let mut rows = conn
        .query(
            &format!("SELECT {COLUMNS} FROM watches WHERE name = ?1"),
            [name],
        )
        .await?;
    rows.next().await?.map(|row| to_watch(&row)).transpose()
}

async fn fetch_many(conn: &Connection, condition: &str) -> Result<Vec<WatchRow>> {
    let mut rows = conn
        .query(
            &format!(
                "SELECT {COLUMNS} FROM watches WHERE {condition} ORDER BY started_at_ms, name"
            ),
            (),
        )
        .await?;
    let mut watches = Vec::new();
    while let Some(row) = rows.next().await? {
        watches.push(to_watch(&row)?);
    }
    Ok(watches)
}

fn to_watch(row: &turso::Row) -> Result<WatchRow> {
    let count = |index| {
        u32::try_from(integer(row, index)?)
            .map_err(|_| StoreError::Data(format!("column {index} out of range")))
    };
    Ok(WatchRow {
        name: text(row, 0)?,
        folder: text(row, 1)?,
        skill: text(row, 2)?,
        report_to: text(row, 3)?,
        every_minutes: count(4)?,
        every_run: integer(row, 5)? == 1,
        run_budget_usd_micros: optional_integer(row, 6)?,
        running: text(row, 7)? == "running",
        started_at_ms: integer(row, 8)?,
        ends_at_ms: integer(row, 9)?,
        next_run_at_ms: integer(row, 10)?,
        runs: count(11)?,
        reports: count(12)?,
        failed_in_a_row: count(13)?,
        cost_usd_micros: integer(row, 14)?,
        last_outcome: optional_text(row, 15)?,
        stop_reason: optional_text(row, 16)?,
        ended_at_ms: optional_integer(row, 17)?,
    })
}

async fn configure(conn: &Connection) -> Result<()> {
    conn.busy_timeout(Duration::from_millis(5_000))?;
    let mut rows = conn.query("PRAGMA journal_mode = WAL", ()).await?;
    while rows.next().await?.is_some() {}
    conn.execute("PRAGMA synchronous = NORMAL", ()).await?;
    Ok(())
}

/// Starts a write. A caller dropped between `begin` and `finish` leaves the
/// connection inside its transaction, so that one is rolled back first.
async fn begin(conn: &Connection) -> Result<()> {
    roll_back_abandoned(conn).await?;
    conn.execute("BEGIN IMMEDIATE", ()).await?;
    Ok(())
}

async fn roll_back_abandoned(conn: &Connection) -> Result<()> {
    if !conn.is_autocommit()? {
        conn.execute("ROLLBACK", ()).await?;
    }
    Ok(())
}

/// Commits when the work succeeded, rolls back when it failed.
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

async fn apply_migrations(conn: &Connection) -> Result<()> {
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

    let mut applied = std::collections::HashMap::new();
    let mut rows = conn
        .query("SELECT name, checksum FROM _migrations", ())
        .await?;
    while let Some(row) = rows.next().await? {
        applied.insert(text(&row, 0)?, text(&row, 1)?);
    }
    drop(rows);

    if applied
        .keys()
        .any(|name| !MIGRATIONS.iter().any(|(known, _)| known == name))
    {
        return Err(StoreError::Data("incompatible watch schema".into()));
    }

    for (name, sql) in MIGRATIONS {
        let checksum = hex::encode(Sha256::digest(sql.as_bytes()));
        match applied.get(name) {
            Some(existing) if *existing == checksum => {}
            Some(_) => {
                return Err(StoreError::Data(format!(
                    "migration {name} checksum changed"
                )));
            }
            None => {
                conn.execute_batch(sql).await?;
                conn.execute(
                    "INSERT INTO _migrations (name, checksum, applied_at_ms) VALUES (?1, ?2, ?3)",
                    (name, checksum, wall_clock_ms()),
                )
                .await?;
            }
        }
    }
    Ok(())
}

fn wall_clock_ms() -> i64 {
    (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
}

fn text(row: &turso::Row, index: usize) -> Result<String> {
    row.get_value(index)?
        .as_text()
        .cloned()
        .ok_or_else(|| StoreError::Data(format!("column {index} is not text")))
}

fn optional_text(row: &turso::Row, index: usize) -> Result<Option<String>> {
    let value = row.get_value(index)?;
    if value.is_null() {
        return Ok(None);
    }
    value
        .as_text()
        .cloned()
        .map(Some)
        .ok_or_else(|| StoreError::Data(format!("column {index} is not text")))
}

fn integer(row: &turso::Row, index: usize) -> Result<i64> {
    row.get_value(index)?
        .as_integer()
        .copied()
        .ok_or_else(|| StoreError::Data(format!("column {index} is not an integer")))
}

fn optional_integer(row: &turso::Row, index: usize) -> Result<Option<i64>> {
    let value = row.get_value(index)?;
    if value.is_null() {
        return Ok(None);
    }
    value
        .as_integer()
        .copied()
        .map(Some)
        .ok_or_else(|| StoreError::Data(format!("column {index} is not an integer")))
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
