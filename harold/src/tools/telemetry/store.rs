//! The telemetry tool's own database, apart from Harold's core store.

use std::future::Future;
use std::path::Path;
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use turso::Connection;

use super::name::{Attr, Kind, MAX_EVENTS, MAX_NAMES};
use super::schema::{Declaration, LastValues};

const DATABASE: &str = "telemetry.db";

const MIGRATIONS: [(&str, &str); 1] = [("001_names", include_str!("migrations/001_names.sql"))];

#[derive(Debug, thiserror::Error)]
pub(crate) enum StoreError {
    #[error("telemetry database: {0}")]
    Turso(#[from] turso::Error),
    #[error("telemetry data: {0}")]
    Data(String),
}

impl From<serde_json::Error> for StoreError {
    fn from(error: serde_json::Error) -> Self {
        Self::Data(error.to_string())
    }
}

type Result<T> = std::result::Result<T, StoreError>;

/// What a declaration did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Declared {
    /// The name is new and is stored now.
    Added,
    /// The same declaration is stored already; nothing changed.
    Same,
    /// The name is declared with another kind or other attribute lists.
    Differs,
    /// The prefix has `MAX_NAMES` names already.
    TooManyNames,
    /// The prefix has `MAX_EVENTS` events already.
    TooManyEvents,
}

pub(crate) trait TelemetryStore: Send + Sync + 'static {
    /// Stores the declaration, replacing one with the same name. The gauge's
    /// last value starts empty. The caller checks the caps and the rules.
    fn put(&self, declaration: &Declaration) -> impl Future<Output = Result<()>> + Send;
    /// Declares the name in one transaction: it compares with a stored
    /// declaration, checks the prefix's caps, and stores a new one. The caller
    /// validates the declaration first.
    fn declare(&self, declaration: &Declaration) -> impl Future<Output = Result<Declared>> + Send;
    fn get(&self, name: &str) -> impl Future<Output = Result<Option<Declaration>>> + Send;
    /// The declared names by name, of one prefix or all.
    fn list(&self, prefix: Option<&str>) -> impl Future<Output = Result<Vec<Declaration>>> + Send;
    /// Removes the name; `false` when it was not there.
    fn remove(&self, name: &str) -> impl Future<Output = Result<bool>> + Send;
    /// The number of names and, of those, of events for the prefix.
    fn count_for_prefix(&self, prefix: &str) -> impl Future<Output = Result<(u32, u32)>> + Send;
    /// Keeps a gauge's last value for one series (see `series_key`); `false`
    /// when the name is not there.
    fn set_last_value(
        &self,
        name: &str,
        series: &str,
        value: f64,
    ) -> impl Future<Output = Result<bool>> + Send;
}

pub(crate) struct TursoTelemetryStore {
    conn: Mutex<Connection>,
    // The database handle stays alive as long as the connection is used.
    _database: turso::Database,
}

impl TursoTelemetryStore {
    pub(crate) async fn open(folder: &Path) -> Result<Self> {
        tokio::fs::create_dir_all(folder)
            .await
            .map_err(|e| StoreError::Data(format!("create {}: {e}", folder.display())))?;
        let path = folder.join(DATABASE);
        let path = path
            .to_str()
            .ok_or_else(|| StoreError::Data("telemetry database path is not UTF-8".into()))?;
        let database = turso::Builder::new_local(path).build().await?;
        let conn = database.connect()?;
        configure(&conn).await?;
        initialize_schema(&conn).await?;
        Ok(Self {
            conn: Mutex::new(conn),
            _database: database,
        })
    }
}

async fn configure(conn: &Connection) -> Result<()> {
    conn.busy_timeout(Duration::from_millis(5_000))?;
    let mut rows = conn.query("PRAGMA journal_mode = WAL", ()).await?;
    while rows.next().await?.is_some() {}
    conn.execute("PRAGMA synchronous = NORMAL", ()).await?;
    Ok(())
}

async fn initialize_schema(conn: &Connection) -> Result<()> {
    begin(conn).await?;
    finish(conn, apply_migrations(conn).await).await
}

/// Starts a write. A caller dropped between `begin` and `finish` leaves the
/// connection inside its transaction, so that one is rolled back first.
async fn begin(conn: &Connection) -> Result<()> {
    if !conn.is_autocommit()? {
        conn.execute("ROLLBACK", ()).await?;
    }
    conn.execute("BEGIN IMMEDIATE", ()).await?;
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
        return Err(StoreError::Data("incompatible telemetry schema".into()));
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

/// The last values column: a JSON map, or empty for none.
fn last_values(row: &turso::Row, index: usize) -> Result<LastValues> {
    let value = row.get_value(index)?;
    if value.is_null() {
        return Ok(LastValues::new());
    }
    let json = value
        .as_text()
        .ok_or_else(|| StoreError::Data(format!("column {index} is not text")))?;
    Ok(serde_json::from_str(json)?)
}

const COLUMNS: &str = "name, kind, meaning, attrs_json, created_ms, last_value";

fn declaration_of(row: &turso::Row) -> Result<Declaration> {
    let kind = text(row, 1)?;
    let attrs: Vec<Attr> = serde_json::from_str(&text(row, 3)?)?;
    Ok(Declaration {
        name: text(row, 0)?,
        kind: Kind::parse(&kind).ok_or_else(|| StoreError::Data(format!("unknown kind {kind}")))?,
        meaning: text(row, 2)?,
        attrs,
        created_ms: integer(row, 4)?,
        last_values: last_values(row, 5)?,
    })
}

async fn put_in(conn: &Connection, declaration: &Declaration) -> Result<()> {
    let prefix = declaration.name.split('.').next().unwrap_or_default();
    conn.execute(
        "INSERT OR REPLACE INTO names \
         (name, prefix, kind, meaning, attrs_json, created_ms, last_value) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL)",
        (
            declaration.name.as_str(),
            prefix,
            declaration.kind.as_str(),
            declaration.meaning.as_str(),
            serde_json::to_string(&declaration.attrs)?,
            declaration.created_ms,
        ),
    )
    .await?;
    Ok(())
}

async fn get_in(conn: &Connection, name: &str) -> Result<Option<Declaration>> {
    let mut rows = conn
        .query(
            &format!("SELECT {COLUMNS} FROM names WHERE name = ?1"),
            [name],
        )
        .await?;
    match rows.next().await? {
        Some(row) => Ok(Some(declaration_of(&row)?)),
        None => Ok(None),
    }
}

async fn count_in(conn: &Connection, prefix: &str) -> Result<(u32, u32)> {
    let mut rows = conn
        .query(
            "SELECT COUNT(*), COALESCE(SUM(kind = 'event'), 0) FROM names WHERE prefix = ?1",
            [prefix],
        )
        .await?;
    let Some(row) = rows.next().await? else {
        return Ok((0, 0));
    };
    let count = |value: i64| {
        u32::try_from(value).map_err(|_| StoreError::Data("count out of range".into()))
    };
    Ok((count(integer(&row, 0)?)?, count(integer(&row, 1)?)?))
}

async fn declare_in(conn: &Connection, declaration: &Declaration) -> Result<Declared> {
    if let Some(existing) = get_in(conn, &declaration.name).await? {
        return Ok(match existing.same_shape(declaration) {
            true => Declared::Same,
            false => Declared::Differs,
        });
    }
    let prefix = declaration.name.split('.').next().unwrap_or_default();
    let (names, events) = count_in(conn, prefix).await?;
    if names >= MAX_NAMES {
        return Ok(Declared::TooManyNames);
    }
    if declaration.kind == Kind::Event && events >= MAX_EVENTS {
        return Ok(Declared::TooManyEvents);
    }
    put_in(conn, declaration).await?;
    Ok(Declared::Added)
}

async fn set_last_value_in(
    conn: &Connection,
    name: &str,
    series: &str,
    value: f64,
) -> Result<bool> {
    let Some(declaration) = get_in(conn, name).await? else {
        return Ok(false);
    };
    let mut values = declaration.last_values;
    values.insert(series.to_string(), value);
    conn.execute(
        "UPDATE names SET last_value = ?1 WHERE name = ?2",
        (serde_json::to_string(&values)?, name),
    )
    .await?;
    Ok(true)
}

impl TelemetryStore for TursoTelemetryStore {
    async fn declare(&self, declaration: &Declaration) -> Result<Declared> {
        let conn = self.conn.lock().await;
        begin(&conn).await?;
        finish(&conn, declare_in(&conn, declaration).await).await
    }

    async fn put(&self, declaration: &Declaration) -> Result<()> {
        let conn = self.conn.lock().await;
        begin(&conn).await?;
        finish(&conn, put_in(&conn, declaration).await).await
    }

    async fn get(&self, name: &str) -> Result<Option<Declaration>> {
        let conn = self.conn.lock().await;
        get_in(&conn, name).await
    }

    async fn list(&self, prefix: Option<&str>) -> Result<Vec<Declaration>> {
        let conn = self.conn.lock().await;
        let mut rows = match prefix {
            Some(prefix) => {
                conn.query(
                    &format!("SELECT {COLUMNS} FROM names WHERE prefix = ?1 ORDER BY name"),
                    [prefix],
                )
                .await?
            }
            None => {
                conn.query(&format!("SELECT {COLUMNS} FROM names ORDER BY name"), ())
                    .await?
            }
        };
        let mut found = Vec::new();
        while let Some(row) = rows.next().await? {
            found.push(declaration_of(&row)?);
        }
        Ok(found)
    }

    async fn remove(&self, name: &str) -> Result<bool> {
        let conn = self.conn.lock().await;
        begin(&conn).await?;
        let work = conn
            .execute("DELETE FROM names WHERE name = ?1", [name])
            .await
            .map(|changed| changed > 0)
            .map_err(StoreError::from);
        finish(&conn, work).await
    }

    async fn count_for_prefix(&self, prefix: &str) -> Result<(u32, u32)> {
        let conn = self.conn.lock().await;
        count_in(&conn, prefix).await
    }

    async fn set_last_value(&self, name: &str, series: &str, value: f64) -> Result<bool> {
        let conn = self.conn.lock().await;
        begin(&conn).await?;
        finish(&conn, set_last_value_in(&conn, name, series, value).await).await
    }
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
