//! The slave's report backlog: numbered reports kept until the master acknowledges them.
//!
//! The backlog is a small Turso file, `report-backlog.db`. It holds encoded `Turn` and `Lock`
//! messages. It never logs a payload.

use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::store::{Result, StoreError};

/// The most reports the backlog keeps.
pub(crate) const MAX_REPORTS: u64 = 1000;
/// The longest time the backlog keeps a report.
pub(crate) const MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// The kind of message a report holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReportKind {
    Turn,
    Lock,
}

impl ReportKind {
    fn label(self) -> &'static str {
        match self {
            ReportKind::Turn => "turn",
            ReportKind::Lock => "lock",
        }
    }
}

/// How many reports `trim` dropped, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Dropped {
    pub(crate) too_old: u64,
    pub(crate) too_many: u64,
}

#[derive(Debug, Clone, Copy)]
struct Counters {
    last: u64,
    len: u64,
    /// The first `Welcome` of this process has been handled. A row appended before it is
    /// provisional.
    welcomed: bool,
}

pub(crate) struct ReportBacklog {
    _db: turso::Database,
    conn: tokio::sync::Mutex<turso::Connection>,
    counters: Mutex<Counters>,
    /// A test sets it to make the next `welcome_first` fail once.
    #[cfg(test)]
    fail_welcome_first: std::sync::atomic::AtomicBool,
}

fn at_ms(time: SystemTime) -> i64 {
    // A clock before the epoch counts as time zero.
    time.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn count(value: &turso::Value) -> u64 {
    value.as_integer().copied().unwrap_or(0).max(0) as u64
}

async fn one_number(conn: &turso::Connection, sql: &str) -> Result<u64> {
    let mut rows = conn.query(sql, ()).await?;
    let Some(row) = rows.next().await? else {
        return Ok(0);
    };
    Ok(count(&row.get_value(0)?))
}

impl ReportBacklog {
    pub(crate) async fn open(path: &Path) -> Result<Self> {
        let db = turso::Builder::new_local(&path.to_string_lossy())
            .build()
            .await?;
        let conn = db.connect()?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS reports (
                number INTEGER PRIMARY KEY,
                kind TEXT NOT NULL,
                payload BLOB NOT NULL,
                at_ms INTEGER NOT NULL,
                provisional INTEGER NOT NULL DEFAULT 0
            )",
            (),
        )
        .await?;
        // A backlog file from before the column existed gets it. Its rows are not provisional.
        if conn
            .query("SELECT provisional FROM reports LIMIT 1", ())
            .await
            .is_err()
        {
            conn.execute(
                "ALTER TABLE reports ADD COLUMN provisional INTEGER NOT NULL DEFAULT 0",
                (),
            )
            .await?;
        }
        conn.execute(
            "CREATE TABLE IF NOT EXISTS backlog_meta (key TEXT PRIMARY KEY, value INTEGER NOT NULL)",
            (),
        )
        .await?;
        let kept = one_number(
            &conn,
            "SELECT value FROM backlog_meta WHERE key = 'last_number'",
        )
        .await?;
        let newest = one_number(&conn, "SELECT COALESCE(MAX(number), 0) FROM reports").await?;
        let len = one_number(&conn, "SELECT COUNT(*) FROM reports").await?;
        Ok(Self {
            _db: db,
            conn: tokio::sync::Mutex::new(conn),
            counters: Mutex::new(Counters {
                last: kept.max(newest),
                len,
                welcomed: false,
            }),
            #[cfg(test)]
            fail_welcome_first: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Keep a report and return its number.
    ///
    /// Do not cancel this future: a cancel can leave a transaction open on the connection.
    pub(crate) async fn append(
        &self,
        kind: ReportKind,
        payload: Vec<u8>,
        now: SystemTime,
    ) -> Result<u64> {
        let conn = self.conn.lock().await;
        // Take the number and the provisional mark under the counter lock, so `rebase` cannot
        // lower the number. `welcome_first` holds the connection lock too, so an append is
        // wholly before it (provisional, and numbered by it) or wholly after it (numbered
        // above the new last number).
        let (number, provisional) = {
            let mut counters = self.counters.lock().unwrap();
            counters.last += 1;
            (counters.last, !counters.welcomed)
        };
        conn.execute("BEGIN IMMEDIATE", ()).await?;
        let written = async {
            conn.execute(
                "INSERT INTO reports (number, kind, payload, at_ms, provisional)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                (
                    number as i64,
                    kind.label(),
                    turso::Value::Blob(payload),
                    at_ms(now),
                    i64::from(provisional),
                ),
            )
            .await?;
            conn.execute(
                "INSERT INTO backlog_meta (key, value) VALUES ('last_number', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = MAX(value, excluded.value)",
                (number as i64,),
            )
            .await?;
            conn.execute("COMMIT", ()).await?;
            Ok::<(), StoreError>(())
        }
        .await;
        if let Err(error) = written {
            let _ = conn.execute("ROLLBACK", ()).await;
            return Err(error);
        }
        let mut counters = self.counters.lock().unwrap();
        counters.len += 1;
        Ok(number)
    }

    /// Every kept report above `n`, oldest first.
    pub(crate) async fn after(&self, n: u64) -> Result<Vec<(u64, Vec<u8>)>> {
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query(
                "SELECT number, payload FROM reports WHERE number > ?1 ORDER BY number",
                (n as i64,),
            )
            .await?;
        let mut reports = Vec::new();
        while let Some(row) = rows.next().await? {
            let number = count(&row.get_value(0)?);
            let payload = row
                .get_value(1)?
                .as_blob()
                .cloned()
                .ok_or_else(|| StoreError::Data("report payload is not a blob".into()))?;
            reports.push((number, payload));
        }
        Ok(reports)
    }

    /// Delete every report up to and including `n`.
    pub(crate) async fn ack(&self, n: u64) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute("DELETE FROM reports WHERE number <= ?1", (n as i64,))
            .await?;
        self.refresh_len(&conn).await
    }

    /// Handles the first `Welcome` of this process, in one transaction.
    ///
    /// It deletes the old rows the master has (not provisional, number up to `welcome`). It
    /// then gives the provisional rows, in order, the numbers above `welcome` and above the
    /// highest old row left, and clears their mark. The last number is raised to at least
    /// `welcome`. A failure changes nothing, so the next `Welcome` can try again.
    pub(crate) async fn welcome_first(&self, welcome: u64) -> Result<()> {
        #[cfg(test)]
        if self
            .fail_welcome_first
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(StoreError::Data("welcome_first failed (test)".into()));
        }
        let conn = self.conn.lock().await;
        conn.execute("BEGIN IMMEDIATE", ()).await?;
        let moved = async {
            conn.execute(
                "DELETE FROM reports WHERE provisional = 0 AND number <= ?1",
                (welcome as i64,),
            )
            .await?;
            let highest_old = one_number(
                &conn,
                "SELECT COALESCE(MAX(number), 0) FROM reports WHERE provisional = 0",
            )
            .await?;
            let mut rows = conn
                .query(
                    "SELECT kind, payload, at_ms FROM reports WHERE provisional = 1
                     ORDER BY number",
                    (),
                )
                .await?;
            let mut moving = Vec::new();
            while let Some(row) = rows.next().await? {
                let kind = row
                    .get_value(0)?
                    .as_text()
                    .cloned()
                    .ok_or_else(|| StoreError::Data("report kind is not text".into()))?;
                let payload = row
                    .get_value(1)?
                    .as_blob()
                    .cloned()
                    .ok_or_else(|| StoreError::Data("report payload is not a blob".into()))?;
                let at = row.get_value(2)?.as_integer().copied().unwrap_or(0);
                moving.push((kind, payload, at));
            }
            drop(rows);
            conn.execute("DELETE FROM reports WHERE provisional = 1", ())
                .await?;
            let mut number = welcome.max(highest_old);
            for (kind, payload, at) in moving {
                number += 1;
                conn.execute(
                    "INSERT INTO reports (number, kind, payload, at_ms, provisional)
                     VALUES (?1, ?2, ?3, ?4, 0)",
                    (number as i64, kind, turso::Value::Blob(payload), at),
                )
                .await?;
            }
            conn.execute(
                "INSERT INTO backlog_meta (key, value) VALUES ('last_number', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = MAX(value, excluded.value)",
                (number as i64,),
            )
            .await?;
            conn.execute("COMMIT", ()).await?;
            Ok::<u64, StoreError>(number)
        }
        .await;
        match moved {
            Ok(last) => {
                {
                    let mut counters = self.counters.lock().unwrap();
                    counters.last = counters.last.max(last).max(welcome);
                    counters.welcomed = true;
                }
                self.refresh_len(&conn).await
            }
            Err(error) => {
                let _ = conn.execute("ROLLBACK", ()).await;
                Err(error)
            }
        }
    }

    /// Makes `welcome_first` fail once. For tests.
    #[cfg(test)]
    pub(crate) fn fail_welcome_first_once(&self) {
        self.fail_welcome_first
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// The newest number given out. It never goes down.
    pub(crate) fn last_number(&self) -> u64 {
        self.counters.lock().unwrap().last
    }

    /// Make the next number higher than `n`. A lower `n` changes nothing.
    pub(crate) fn rebase(&self, n: u64) {
        let mut counters = self.counters.lock().unwrap();
        counters.last = counters.last.max(n);
    }

    /// Drop reports older than 24 hours, then the oldest reports above 1,000.
    pub(crate) async fn trim(&self, now: SystemTime) -> Result<Dropped> {
        let conn = self.conn.lock().await;
        let cutoff = at_ms(now) - MAX_AGE.as_millis() as i64;
        let before = one_number(&conn, "SELECT COUNT(*) FROM reports").await?;
        conn.execute("DELETE FROM reports WHERE at_ms < ?1", (cutoff,))
            .await?;
        let after_age = one_number(&conn, "SELECT COUNT(*) FROM reports").await?;
        let too_many = after_age.saturating_sub(MAX_REPORTS);
        if too_many > 0 {
            conn.execute(
                "DELETE FROM reports WHERE number IN
                 (SELECT number FROM reports ORDER BY number LIMIT ?1)",
                (too_many as i64,),
            )
            .await?;
        }
        self.refresh_len(&conn).await?;
        Ok(Dropped {
            too_old: before - after_age,
            too_many,
        })
    }

    /// How many reports the backlog keeps.
    pub(crate) fn len(&self) -> u64 {
        self.counters.lock().unwrap().len
    }

    async fn refresh_len(&self, conn: &turso::Connection) -> Result<()> {
        let len = one_number(conn, "SELECT COUNT(*) FROM reports").await?;
        self.counters.lock().unwrap().len = len;
        Ok(())
    }
}

#[cfg(test)]
#[path = "backlog_tests.rs"]
mod backlog_tests;
