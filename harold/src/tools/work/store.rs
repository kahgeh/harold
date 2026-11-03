//! One work file: a Turso database of work records, their decisions and links.

use std::path::Path;
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use turso::Connection;

use super::schema::{Decision, Mark, Priority, Record, Status, valid_name};

const MIGRATIONS: [(&str, &str); 1] = [("001_work", include_str!("migrations/001_work.sql"))];

/// The most open work records in `focus`.
const FOCUS_MAX: i64 = 2;

#[derive(Debug, thiserror::Error)]
pub(super) enum StoreError {
    #[error("work database: {0}")]
    Turso(#[from] turso::Error),
    #[error("work data: {0}")]
    Data(String),
}

type Result<T> = std::result::Result<T, StoreError>;

/// Why a write was refused. Nothing is stored then.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Refusal {
    FocusFull,
    NameInUse,
    NotFound,
    ReadOnly,
    BadName,
}

impl Refusal {
    pub(super) fn reason(self) -> &'static str {
        match self {
            Refusal::FocusFull => "focus_full",
            Refusal::NameInUse => "name_in_use",
            Refusal::NotFound => "not_found",
            Refusal::ReadOnly => "read_only",
            Refusal::BadName => "bad_name",
        }
    }
}

/// What a `put` or a `set` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PutOutcome {
    Created,
    Replaced,
    /// The status changed. `since_ms` is when the old status began.
    StatusChanged {
        from: Status,
        since_ms: i64,
    },
}

/// The fields that `set` changes. A `None` field stays as it is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Patch {
    pub(super) title: Option<String>,
    pub(super) priority: Option<Priority>,
    pub(super) status: Option<Status>,
    pub(super) waits_for: Option<String>,
    pub(super) next_step: Option<String>,
}

/// The open work records in a file, with their decisions and links.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Counts {
    pub(super) open_records: u32,
    pub(super) decisions: u32,
    pub(super) links: u32,
}

pub(super) struct WorkFile {
    conn: Mutex<Connection>,
    /// The file name, kept as `created_in` of the records made here.
    file_name: String,
    /// Set for an old file. Every write is refused in code, because Turso has
    /// no read-only open.
    read_only: bool,
    // The database handle stays alive as long as the connection is used.
    _database: turso::Database,
}

impl WorkFile {
    /// Opens the file, and makes and migrates it when it is new.
    pub(super) async fn open(path: &Path) -> Result<Self> {
        if let Some(folder) = path.parent() {
            tokio::fs::create_dir_all(folder)
                .await
                .map_err(|e| StoreError::Data(format!("create {}: {e}", folder.display())))?;
        }
        Self::connect(path, false).await
    }

    /// Opens an existing file for reads. Every write is refused with
    /// `Refusal::ReadOnly`. A missing file is an error, and nothing is made.
    pub(super) async fn open_read_only(path: &Path) -> Result<Self> {
        if !path.is_file() {
            return Err(StoreError::Data(format!("no work file {}", path.display())));
        }
        Self::connect(path, true).await
    }

    async fn connect(path: &Path, read_only: bool) -> Result<Self> {
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| StoreError::Data("work file name is not UTF-8".into()))?
            .to_string();
        let path = path
            .to_str()
            .ok_or_else(|| StoreError::Data("work file path is not UTF-8".into()))?;
        let database = turso::Builder::new_local(path).build().await?;
        let conn = database.connect()?;
        configure(&conn, read_only).await?;
        if !read_only {
            begin(&conn).await?;
            finish(&conn, apply_migrations(&conn).await).await?;
        }
        Ok(Self {
            conn: Mutex::new(conn),
            file_name,
            read_only,
            _database: database,
        })
    }

    /// Makes a record, or replaces the fields of an open one. A replacement keeps
    /// the decisions, the links, `created_in` and `created_at`.
    pub(super) async fn put(
        &self,
        record: Record,
        now_ms: i64,
    ) -> Result<std::result::Result<PutOutcome, Refusal>> {
        if !valid_name(&record.name) {
            return Ok(Err(Refusal::BadName));
        }
        let guard = match self.begin_write().await? {
            Ok(guard) => guard,
            Err(refusal) => return Ok(Err(refusal)),
        };
        let conn: &Connection = &guard;
        finish_refusable(conn, put_in(conn, &self.file_name, &record, now_ms).await).await
    }

    /// Changes some fields of a record.
    pub(super) async fn set(
        &self,
        name: &str,
        patch: Patch,
        now_ms: i64,
    ) -> Result<std::result::Result<PutOutcome, Refusal>> {
        let guard = match self.begin_write().await? {
            Ok(guard) => guard,
            Err(refusal) => return Ok(Err(refusal)),
        };
        let conn: &Connection = &guard;
        finish_refusable(
            conn,
            async move {
                let Some(mut record) = load(conn, name).await? else {
                    return Ok(Err(Refusal::NotFound));
                };
                if let Some(title) = patch.title {
                    record.title = title;
                }
                if let Some(priority) = patch.priority {
                    record.priority = priority;
                }
                if let Some(status) = patch.status {
                    record.status = status;
                }
                if let Some(waits_for) = patch.waits_for {
                    record.waits_for = waits_for;
                }
                if let Some(next_step) = patch.next_step {
                    record.next_step = next_step;
                }
                replace_in(conn, &record, now_ms).await
            }
            .await,
        )
        .await
    }

    /// Adds a decision and returns its number, counted from 1.
    pub(super) async fn decide(
        &self,
        name: &str,
        decision: Decision,
        now_ms: i64,
    ) -> Result<std::result::Result<u32, Refusal>> {
        let guard = match self.begin_write().await? {
            Ok(guard) => guard,
            Err(refusal) => return Ok(Err(refusal)),
        };
        let conn: &Connection = &guard;
        finish_refusable(
            conn,
            async move {
                if !exists(conn, name).await? {
                    return Ok(Err(Refusal::NotFound));
                }
                let n = next_decision_number(conn, name).await?;
                insert_decision(conn, name, n, &decision).await?;
                touch(conn, name, now_ms).await?;
                Ok(Ok(count_of(n)?))
            }
            .await,
        )
        .await
    }

    /// Changes the mark of decision `n`.
    pub(super) async fn mark(
        &self,
        name: &str,
        n: u32,
        mark: Mark,
        now_ms: i64,
    ) -> Result<std::result::Result<(), Refusal>> {
        let guard = match self.begin_write().await? {
            Ok(guard) => guard,
            Err(refusal) => return Ok(Err(refusal)),
        };
        let conn: &Connection = &guard;
        finish_refusable(
            conn,
            async move {
                let changed = conn
                    .execute(
                        "UPDATE decisions SET mark = ?1 WHERE record = ?2 AND n = ?3",
                        (mark.as_str(), name, i64::from(n)),
                    )
                    .await?;
                if changed == 0 {
                    return Ok(Err(Refusal::NotFound));
                }
                touch(conn, name, now_ms).await?;
                Ok(Ok(()))
            }
            .await,
        )
        .await
    }

    /// Adds a link. A link that is there already stays as it is.
    pub(super) async fn link(
        &self,
        name: &str,
        link: &str,
        now_ms: i64,
    ) -> Result<std::result::Result<(), Refusal>> {
        let guard = match self.begin_write().await? {
            Ok(guard) => guard,
            Err(refusal) => return Ok(Err(refusal)),
        };
        let conn: &Connection = &guard;
        finish_refusable(
            conn,
            async move {
                if !exists(conn, name).await? {
                    return Ok(Err(Refusal::NotFound));
                }
                conn.execute(
                    "INSERT OR IGNORE INTO links (record, link) VALUES (?1, ?2)",
                    (name, link),
                )
                .await?;
                touch(conn, name, now_ms).await?;
                Ok(Ok(()))
            }
            .await,
        )
        .await
    }

    /// Removes a link. A link that is not there is `NotFound`.
    pub(super) async fn unlink(
        &self,
        name: &str,
        link: &str,
        now_ms: i64,
    ) -> Result<std::result::Result<(), Refusal>> {
        let guard = match self.begin_write().await? {
            Ok(guard) => guard,
            Err(refusal) => return Ok(Err(refusal)),
        };
        let conn: &Connection = &guard;
        finish_refusable(
            conn,
            async move {
                let removed = conn
                    .execute(
                        "DELETE FROM links WHERE record = ?1 AND link = ?2",
                        (name, link),
                    )
                    .await?;
                if removed == 0 {
                    return Ok(Err(Refusal::NotFound));
                }
                touch(conn, name, now_ms).await?;
                Ok(Ok(()))
            }
            .await,
        )
        .await
    }

    /// Stores a record as it is, with its times, decisions and links. A rotation
    /// uses it. A name that the file holds already is `NameInUse`.
    pub(super) async fn insert_copy(
        &self,
        record: &Record,
    ) -> Result<std::result::Result<(), Refusal>> {
        let guard = match self.begin_write().await? {
            Ok(guard) => guard,
            Err(refusal) => return Ok(Err(refusal)),
        };
        let conn: &Connection = &guard;
        finish_refusable(
            conn,
            async move {
                if exists(conn, &record.name).await? {
                    return Ok(Err(Refusal::NameInUse));
                }
                insert_record(conn, record).await?;
                insert_parts(conn, record).await?;
                Ok(Ok(()))
            }
            .await,
        )
        .await
    }

    pub(super) async fn get(&self, name: &str) -> Result<Option<Record>> {
        let conn = self.conn.lock().await;
        roll_back_abandoned(&conn).await?;
        load(&conn, name).await
    }

    /// The open records: focus, then next, then later. `all` adds the closed
    /// ones after them.
    pub(super) async fn list(&self, all: bool) -> Result<Vec<Record>> {
        let conn = self.conn.lock().await;
        roll_back_abandoned(&conn).await?;
        let filter = if all {
            ""
        } else {
            "WHERE status != 'checked by the user'"
        };
        let sql = format!(
            "SELECT {COLUMNS} FROM records {filter} ORDER BY \
             (status = 'checked by the user'), \
             CASE priority WHEN 'focus' THEN 0 WHEN 'next' THEN 1 ELSE 2 END, \
             created_at_ms, name"
        );
        let mut records = Vec::new();
        let mut rows = conn.query(&sql, ()).await?;
        while let Some(row) = rows.next().await? {
            records.push(record_of(&row)?);
        }
        drop(rows);
        for record in &mut records {
            fill_parts(&conn, record).await?;
        }
        Ok(records)
    }

    pub(super) async fn open_records(&self) -> Result<Vec<Record>> {
        self.list(false).await
    }

    pub(super) async fn counts(&self) -> Result<Counts> {
        let conn = self.conn.lock().await;
        roll_back_abandoned(&conn).await?;
        let open = "SELECT name FROM records WHERE status != 'checked by the user'";
        Ok(Counts {
            open_records: count(&conn, &format!("SELECT COUNT(*) FROM ({open})")).await?,
            decisions: count(
                &conn,
                &format!("SELECT COUNT(*) FROM decisions WHERE record IN ({open})"),
            )
            .await?,
            links: count(
                &conn,
                &format!("SELECT COUNT(*) FROM links WHERE record IN ({open})"),
            )
            .await?,
        })
    }

    /// Locks the connection and starts a transaction. A read-only file refuses
    /// before it starts. The caller ends the transaction with
    /// `finish_refusable`, which rolls back a refused write.
    async fn begin_write(
        &self,
    ) -> Result<std::result::Result<tokio::sync::MutexGuard<'_, Connection>, Refusal>> {
        if self.read_only {
            return Ok(Err(Refusal::ReadOnly));
        }
        let conn = self.conn.lock().await;
        begin(&conn).await?;
        Ok(Ok(conn))
    }
}

/// A read-only open skips the pragmas that write to the file.
async fn configure(conn: &Connection, read_only: bool) -> Result<()> {
    conn.busy_timeout(Duration::from_millis(5_000))?;
    if read_only {
        return Ok(());
    }
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

async fn finish_refusable<T, E>(
    conn: &Connection,
    work: Result<std::result::Result<T, E>>,
) -> Result<std::result::Result<T, E>> {
    match work {
        Ok(Err(refusal)) => {
            conn.execute("ROLLBACK", ()).await?;
            Ok(Err(refusal))
        }
        work => finish(conn, work).await,
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
        return Err(StoreError::Data("incompatible work schema".into()));
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
                    (name, checksum, now_ms()),
                )
                .await?;
            }
        }
    }
    Ok(())
}

fn now_ms() -> i64 {
    (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
}

fn text(row: &turso::Row, index: usize) -> Result<String> {
    row.get_value(index)?
        .as_text()
        .cloned()
        .ok_or_else(|| StoreError::Data(format!("column {index} is not text")))
}

fn integer(row: &turso::Row, index: usize) -> Result<i64> {
    row.get_value(index)?
        .as_integer()
        .copied()
        .ok_or_else(|| StoreError::Data(format!("column {index} is not an integer")))
}

fn count_of(value: i64) -> Result<u32> {
    u32::try_from(value).map_err(|_| StoreError::Data("count out of range".into()))
}

async fn count(conn: &Connection, sql: &str) -> Result<u32> {
    let mut rows = conn.query(sql, ()).await?;
    match rows.next().await? {
        Some(row) => count_of(integer(&row, 0)?),
        None => Ok(0),
    }
}

const COLUMNS: &str = "name, title, priority, status, waits_for, next_step, created_in, \
                       created_at_ms, updated_at_ms, status_since_ms";

fn record_of(row: &turso::Row) -> Result<Record> {
    let priority = text(row, 2)?;
    let status = text(row, 3)?;
    Ok(Record {
        name: text(row, 0)?,
        title: text(row, 1)?,
        priority: Priority::parse(&priority)
            .ok_or_else(|| StoreError::Data(format!("unknown priority {priority}")))?,
        status: Status::parse(&status)
            .ok_or_else(|| StoreError::Data(format!("unknown status {status}")))?,
        waits_for: text(row, 4)?,
        next_step: text(row, 5)?,
        created_in: text(row, 6)?,
        created_at_ms: integer(row, 7)?,
        updated_at_ms: integer(row, 8)?,
        status_since_ms: integer(row, 9)?,
        decisions: Vec::new(),
        links: Vec::new(),
    })
}

async fn fill_parts(conn: &Connection, record: &mut Record) -> Result<()> {
    let mut rows = conn
        .query(
            "SELECT at, who, mark, text FROM decisions WHERE record = ?1 ORDER BY n",
            [record.name.as_str()],
        )
        .await?;
    while let Some(row) = rows.next().await? {
        let mark = text(&row, 2)?;
        record.decisions.push(Decision {
            at: text(&row, 0)?,
            who: text(&row, 1)?,
            mark: Mark::parse(&mark)
                .ok_or_else(|| StoreError::Data(format!("unknown mark {mark}")))?,
            text: text(&row, 3)?,
        });
    }
    drop(rows);
    let mut rows = conn
        .query(
            "SELECT link FROM links WHERE record = ?1 ORDER BY rowid",
            [record.name.as_str()],
        )
        .await?;
    while let Some(row) = rows.next().await? {
        record.links.push(text(&row, 0)?);
    }
    Ok(())
}

async fn load(conn: &Connection, name: &str) -> Result<Option<Record>> {
    let mut rows = conn
        .query(
            &format!("SELECT {COLUMNS} FROM records WHERE name = ?1"),
            [name],
        )
        .await?;
    let Some(row) = rows.next().await? else {
        return Ok(None);
    };
    let mut record = record_of(&row)?;
    drop(rows);
    fill_parts(conn, &mut record).await?;
    Ok(Some(record))
}

async fn exists(conn: &Connection, name: &str) -> Result<bool> {
    let mut rows = conn
        .query("SELECT 1 FROM records WHERE name = ?1", [name])
        .await?;
    Ok(rows.next().await?.is_some())
}

async fn touch(conn: &Connection, name: &str, now_ms: i64) -> Result<()> {
    conn.execute(
        "UPDATE records SET updated_at_ms = ?1 WHERE name = ?2",
        (now_ms, name),
    )
    .await?;
    Ok(())
}

async fn next_decision_number(conn: &Connection, name: &str) -> Result<i64> {
    let mut rows = conn
        .query("SELECT MAX(n) FROM decisions WHERE record = ?1", [name])
        .await?;
    match rows.next().await? {
        Some(row) => Ok(row.get_value(0)?.as_integer().copied().unwrap_or(0) + 1),
        None => Ok(1),
    }
}

async fn insert_decision(conn: &Connection, name: &str, n: i64, d: &Decision) -> Result<()> {
    conn.execute(
        "INSERT INTO decisions (record, n, at, who, mark, text) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        (
            name,
            n,
            d.at.as_str(),
            d.who.as_str(),
            d.mark.as_str(),
            d.text.as_str(),
        ),
    )
    .await?;
    Ok(())
}

async fn insert_record(conn: &Connection, r: &Record) -> Result<()> {
    conn.execute(
        &format!(
            "INSERT INTO records ({COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)"
        ),
        (
            r.name.as_str(),
            r.title.as_str(),
            r.priority.as_str(),
            r.status.as_str(),
            r.waits_for.as_str(),
            r.next_step.as_str(),
            r.created_in.as_str(),
            r.created_at_ms,
            r.updated_at_ms,
            r.status_since_ms,
        ),
    )
    .await?;
    Ok(())
}

async fn insert_parts(conn: &Connection, record: &Record) -> Result<()> {
    for (i, decision) in record.decisions.iter().enumerate() {
        insert_decision(conn, &record.name, i as i64 + 1, decision).await?;
    }
    for link in &record.links {
        conn.execute(
            "INSERT OR IGNORE INTO links (record, link) VALUES (?1, ?2)",
            (record.name.as_str(), link.as_str()),
        )
        .await?;
    }
    Ok(())
}

/// Whether the record, as it would be stored, may take a place in `focus`: at
/// most `FOCUS_MAX` open records are there, not counting this one.
async fn focus_has_room(conn: &Connection, record: &Record) -> Result<bool> {
    if record.priority != Priority::Focus || !record.status.is_open() {
        return Ok(true);
    }
    let mut rows = conn
        .query(
            "SELECT COUNT(*) FROM records WHERE priority = 'focus' \
             AND status != 'checked by the user' AND name != ?1",
            [record.name.as_str()],
        )
        .await?;
    let held = match rows.next().await? {
        Some(row) => integer(&row, 0)?,
        None => 0,
    };
    Ok(held < FOCUS_MAX)
}

async fn put_in(
    conn: &Connection,
    file_name: &str,
    record: &Record,
    now_ms: i64,
) -> Result<std::result::Result<PutOutcome, Refusal>> {
    match load(conn, &record.name).await? {
        None => {
            if !focus_has_room(conn, record).await? {
                return Ok(Err(Refusal::FocusFull));
            }
            let mut new = record.clone();
            new.created_in = file_name.to_string();
            new.created_at_ms = now_ms;
            new.updated_at_ms = now_ms;
            new.status_since_ms = now_ms;
            insert_record(conn, &new).await?;
            insert_parts(conn, &new).await?;
            Ok(Ok(PutOutcome::Created))
        }
        // A closed record keeps its name for life.
        Some(old) if !old.status.is_open() => Ok(Err(Refusal::NameInUse)),
        Some(mut old) => {
            old.title = record.title.clone();
            old.priority = record.priority;
            old.status = record.status;
            old.waits_for = record.waits_for.clone();
            old.next_step = record.next_step.clone();
            replace_in(conn, &old, now_ms).await
        }
    }
}

/// Writes the scalar fields of `record` over the stored ones. The status time
/// moves only when the status differs from the stored one.
async fn replace_in(
    conn: &Connection,
    record: &Record,
    now_ms: i64,
) -> Result<std::result::Result<PutOutcome, Refusal>> {
    let Some(stored) = load(conn, &record.name).await? else {
        return Ok(Err(Refusal::NotFound));
    };
    if !focus_has_room(conn, record).await? {
        return Ok(Err(Refusal::FocusFull));
    }
    let (outcome, since) = if stored.status == record.status {
        (PutOutcome::Replaced, stored.status_since_ms)
    } else {
        (
            PutOutcome::StatusChanged {
                from: stored.status,
                since_ms: stored.status_since_ms,
            },
            now_ms,
        )
    };
    conn.execute(
        "UPDATE records SET title = ?1, priority = ?2, status = ?3, waits_for = ?4, \
         next_step = ?5, updated_at_ms = ?6, status_since_ms = ?7 WHERE name = ?8",
        (
            record.title.as_str(),
            record.priority.as_str(),
            record.status.as_str(),
            record.waits_for.as_str(),
            record.next_step.as_str(),
            now_ms,
            since,
            record.name.as_str(),
        ),
    )
    .await?;
    Ok(Ok(outcome))
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
