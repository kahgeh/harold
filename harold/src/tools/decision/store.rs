//! The decision tool's own database, `decisions.db`, in the tool's data folder. It never
//! rotates.

use std::path::Path;
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use turso::{Connection, Value};

const DATABASE: &str = "decisions.db";

/// The migrations, in order. A migration that was applied never changes.
const MIGRATIONS: [(&str, &str); 1] = [(
    "001_decisions",
    include_str!("migrations/001_decisions.sql"),
)];

#[derive(Debug, thiserror::Error)]
pub(super) enum StoreError {
    #[error("decision database: {0}")]
    Turso(#[from] turso::Error),
    #[error("decision data: {0}")]
    Data(String),
}

type Result<T> = std::result::Result<T, StoreError>;

/// Why a change to one record is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Refusal {
    /// No record has this id.
    Unknown,
    /// The record is evaluated, and an evaluated record does not change.
    AlreadyEvaluated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Status {
    Open,
    Due,
    Evaluated,
}

impl Status {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Due => "due",
            Self::Evaluated => "evaluated",
        }
    }

    fn parse(text: &str) -> Result<Self> {
        match text {
            "open" => Ok(Self::Open),
            "due" => Ok(Self::Due),
            "evaluated" => Ok(Self::Evaluated),
            other => Err(StoreError::Data(format!("unknown status {other}"))),
        }
    }
}

/// A record to add. The caller has checked every field.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) struct NewDecision {
    pub organisation: String,
    pub role: String,
    pub kind: String,
    pub details_owner: String,
    pub accountable: String,
    pub title: String,
    pub link: String,
    pub check_on: String,
    pub details: String,
    /// 0 when the question time is not known.
    pub asked_at_ms: i64,
}

/// A stored decision record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DecisionRow {
    pub id: String,
    pub decided_on: String,
    pub organisation: String,
    pub role: String,
    pub kind: String,
    pub details_owner: String,
    pub accountable: String,
    pub title: String,
    pub link: String,
    pub check_on: String,
    pub status: Status,
    pub process_grade: String,
    pub outcome_grade: String,
    pub details: String,
    pub asked_at_ms: i64,
    pub added_at_ms: i64,
    pub evaluated_at_ms: i64,
}

/// What `list` keeps. An empty `statuses` keeps every status.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) struct Filter {
    pub statuses: Vec<String>,
    pub role: Option<String>,
    pub kind: Option<String>,
    pub details_owner: Option<String>,
    pub accountable: Option<String>,
}

const COLUMNS: &str = "id, decided_on, organisation, role, kind, details_owner, accountable, \
    title, link, check_on, status, process_grade, outcome_grade, details, asked_at_ms, \
    added_at_ms, evaluated_at_ms";

pub(super) struct DecisionStore {
    conn: Mutex<Connection>,
    // The database handle stays alive as long as the connection is used.
    _database: turso::Database,
}

impl DecisionStore {
    /// Opens `decisions.db` in `folder`, and makes and migrates it when it is new.
    pub(super) async fn open(folder: &Path) -> Result<Self> {
        tokio::fs::create_dir_all(folder)
            .await
            .map_err(|e| StoreError::Data(format!("create {}: {e}", folder.display())))?;
        let path = folder.join(DATABASE);
        let path = path
            .to_str()
            .ok_or_else(|| StoreError::Data("decision database path is not UTF-8".into()))?;
        let database = turso::Builder::new_local(path).build().await?;
        let conn = database.connect()?;
        conn.busy_timeout(Duration::from_millis(5_000))?;
        let mut rows = conn.query("PRAGMA journal_mode = WAL", ()).await?;
        while rows.next().await?.is_some() {}
        conn.execute("PRAGMA synchronous = NORMAL", ()).await?;
        begin(&conn).await?;
        finish(&conn, apply_migrations(&conn).await).await?;
        Ok(Self {
            conn: Mutex::new(conn),
            _database: database,
        })
    }

    /// Stores a new open record with the id `id`, which the caller has checked. Returns
    /// `None`, and stores nothing, when a record has this id. `decided_on` is the UTC date
    /// of `now_ms`.
    pub(super) async fn add(
        &self,
        id: &str,
        new: &NewDecision,
        now_ms: i64,
    ) -> Result<Option<DecisionRow>> {
        let day = utc_day(now_ms)?;
        let conn = self.conn.lock().await;
        begin(&conn).await?;
        let work = async {
            if fetch_one(&conn, id).await?.is_some() {
                return Ok(None);
            }
            conn.execute(
                "INSERT INTO decisions (id, decided_on, organisation, role, kind, details_owner, \
                 accountable, title, link, check_on, status, details, asked_at_ms, added_at_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'open', ?11, ?12, ?13)",
                turso::params_from_iter(vec![
                    Value::from(id),
                    Value::from(day.as_str()),
                    Value::from(new.organisation.as_str()),
                    Value::from(new.role.as_str()),
                    Value::from(new.kind.as_str()),
                    Value::from(new.details_owner.as_str()),
                    Value::from(new.accountable.as_str()),
                    Value::from(new.title.as_str()),
                    Value::from(new.link.as_str()),
                    Value::from(new.check_on.as_str()),
                    Value::from(new.details.as_str()),
                    Value::from(new.asked_at_ms),
                    Value::from(now_ms),
                ]),
            )
            .await?;
            fetch_one(&conn, id)
                .await?
                .ok_or_else(|| StoreError::Data("record missing after insert".into()))
                .map(Some)
        }
        .await;
        finish(&conn, work).await
    }

    /// One record, or `None` when no record has this id.
    pub(super) async fn get(&self, id: &str) -> Result<Option<DecisionRow>> {
        let conn = self.conn.lock().await;
        roll_back_abandoned(&conn).await?;
        fetch_one(&conn, id).await
    }

    /// The records that `filter` keeps, oldest check date first, then oldest id first.
    pub(super) async fn list(&self, filter: &Filter) -> Result<Vec<DecisionRow>> {
        let mut conditions = Vec::new();
        let mut params: Vec<Value> = Vec::new();
        if !filter.statuses.is_empty() {
            let marks: Vec<String> = filter
                .statuses
                .iter()
                .map(|status| {
                    params.push(Value::from(status.as_str()));
                    format!("?{}", params.len())
                })
                .collect();
            conditions.push(format!("status IN ({})", marks.join(", ")));
        }
        for (column, wanted) in [
            ("role", &filter.role),
            ("kind", &filter.kind),
            ("details_owner", &filter.details_owner),
            ("accountable", &filter.accountable),
        ] {
            if let Some(value) = wanted {
                params.push(Value::from(value.as_str()));
                conditions.push(format!("{column} = ?{}", params.len()));
            }
        }
        let wher = if conditions.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conditions.join(" AND "))
        };
        let conn = self.conn.lock().await;
        roll_back_abandoned(&conn).await?;
        let mut rows = conn
            .query(
                &format!("SELECT {COLUMNS} FROM decisions {wher} ORDER BY check_on, id"),
                turso::params_from_iter(params),
            )
            .await?;
        let mut found = Vec::new();
        while let Some(row) = rows.next().await? {
            found.push(to_row(&row)?);
        }
        Ok(found)
    }

    /// Sets the grades and the status `evaluated`.
    pub(super) async fn evaluate(
        &self,
        id: &str,
        process_grade: &str,
        outcome_grade: &str,
        now_ms: i64,
    ) -> Result<std::result::Result<DecisionRow, Refusal>> {
        self.change(id, |_| {
            (
                "status = 'evaluated', process_grade = ?2, outcome_grade = ?3, evaluated_at_ms = ?4",
                vec![
                    Value::from(process_grade),
                    Value::from(outcome_grade),
                    Value::from(now_ms),
                ],
            )
        })
        .await
    }

    /// Moves the check date of an open or due record. A due record goes back to open.
    pub(super) async fn recheck(
        &self,
        id: &str,
        check_on: &str,
    ) -> Result<std::result::Result<DecisionRow, Refusal>> {
        self.change(id, |_| {
            (
                "status = 'open', check_on = ?2",
                vec![Value::from(check_on)],
            )
        })
        .await
    }

    /// Sets or replaces the link. `None` when no record has this id.
    pub(super) async fn set_link(&self, id: &str, link: &str) -> Result<Option<DecisionRow>> {
        let conn = self.conn.lock().await;
        begin(&conn).await?;
        let work = async {
            conn.execute(
                "UPDATE decisions SET link = ?2 WHERE id = ?1",
                turso::params_from_iter(vec![Value::from(id), Value::from(link)]),
            )
            .await?;
            fetch_one(&conn, id).await
        }
        .await;
        finish(&conn, work).await
    }

    /// Sets each open record whose check date is `today` or before to due. Returns the
    /// records that changed, oldest check date first.
    pub(super) async fn mark_due(&self, today: &str) -> Result<Vec<DecisionRow>> {
        let conn = self.conn.lock().await;
        begin(&conn).await?;
        let work = async {
            let mut rows = conn
                .query(
                    &format!(
                        "SELECT {COLUMNS} FROM decisions WHERE status = 'open' AND check_on <= ?1 \
                         ORDER BY check_on, id"
                    ),
                    [today],
                )
                .await?;
            let mut turned = Vec::new();
            while let Some(row) = rows.next().await? {
                let mut record = to_row(&row)?;
                record.status = Status::Due;
                turned.push(record);
            }
            drop(rows);
            conn.execute(
                "UPDATE decisions SET status = 'due' WHERE status = 'open' AND check_on <= ?1",
                [today],
            )
            .await?;
            Ok(turned)
        }
        .await;
        finish(&conn, work).await
    }

    /// The number of open records and the number of due records.
    pub(super) async fn counts(&self) -> Result<(u64, u64)> {
        let conn = self.conn.lock().await;
        roll_back_abandoned(&conn).await?;
        let mut rows = conn
            .query(
                "SELECT status, COUNT(*) FROM decisions WHERE status IN ('open', 'due') \
                 GROUP BY status",
                (),
            )
            .await?;
        let (mut open, mut due) = (0, 0);
        while let Some(row) = rows.next().await? {
            let count = u64::try_from(integer(&row, 1)?).unwrap_or(0);
            match text(&row, 0)?.as_str() {
                "open" => open = count,
                _ => due = count,
            }
        }
        Ok((open, due))
    }

    /// Runs one update of a record that is not evaluated. `set` gives the `SET` clause
    /// (its parameters start at `?2`) and the values.
    async fn change(
        &self,
        id: &str,
        set: impl FnOnce(&DecisionRow) -> (&'static str, Vec<Value>),
    ) -> Result<std::result::Result<DecisionRow, Refusal>> {
        let conn = self.conn.lock().await;
        begin(&conn).await?;
        let work = async {
            let Some(row) = fetch_one(&conn, id).await? else {
                return Ok(Err(Refusal::Unknown));
            };
            if row.status == Status::Evaluated {
                return Ok(Err(Refusal::AlreadyEvaluated));
            }
            let (clause, mut values) = set(&row);
            let mut params = vec![Value::from(id)];
            params.append(&mut values);
            conn.execute(
                &format!("UPDATE decisions SET {clause} WHERE id = ?1"),
                turso::params_from_iter(params),
            )
            .await?;
            let changed = fetch_one(&conn, id)
                .await?
                .ok_or_else(|| StoreError::Data("record missing after update".into()))?;
            Ok(Ok(changed))
        }
        .await;
        finish(&conn, work).await
    }
}

async fn fetch_one(conn: &Connection, id: &str) -> Result<Option<DecisionRow>> {
    let mut rows = conn
        .query(
            &format!("SELECT {COLUMNS} FROM decisions WHERE id = ?1"),
            [id],
        )
        .await?;
    match rows.next().await? {
        Some(row) => Ok(Some(to_row(&row)?)),
        None => Ok(None),
    }
}

/// The UTC date of `ms`, as `YYYY-MM-DD`.
fn utc_day(ms: i64) -> Result<String> {
    let moment = time::OffsetDateTime::from_unix_timestamp(ms.div_euclid(1000))
        .map_err(|_| StoreError::Data("time out of range".into()))?;
    Ok(format!(
        "{:04}-{:02}-{:02}",
        moment.year(),
        u8::from(moment.month()),
        moment.day()
    ))
}

async fn apply_migrations(conn: &Connection) -> Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS _migrations (\
            id INTEGER PRIMARY KEY, \
            name TEXT NOT NULL UNIQUE, \
            checksum TEXT NOT NULL, \
            applied_at_ms INTEGER NOT NULL)",
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
        return Err(StoreError::Data("incompatible decision schema".into()));
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
    i64::try_from(time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000)
        .unwrap_or(i64::MAX)
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

fn to_row(row: &turso::Row) -> Result<DecisionRow> {
    Ok(DecisionRow {
        id: text(row, 0)?,
        decided_on: text(row, 1)?,
        organisation: text(row, 2)?,
        role: text(row, 3)?,
        kind: text(row, 4)?,
        details_owner: text(row, 5)?,
        accountable: text(row, 6)?,
        title: text(row, 7)?,
        link: text(row, 8)?,
        check_on: text(row, 9)?,
        status: Status::parse(&text(row, 10)?)?,
        process_grade: text(row, 11)?,
        outcome_grade: text(row, 12)?,
        details: text(row, 13)?,
        asked_at_ms: integer(row, 14)?,
        added_at_ms: integer(row, 15)?,
        evaluated_at_ms: integer(row, 16)?,
    })
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
