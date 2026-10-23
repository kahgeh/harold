//! The knowledge index's database: tursodb opened through buckwal, which keeps it durable in
//! a bucket. Every call blocks, so callers run them on a blocking thread.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use buckwal::{Config, Connection, Database, Value};

use super::embed::ModelInfo;
use super::text::ParsedNote;

/// How many passages a search asks the database for.
pub(crate) const NEAREST: usize = 40;

/// Why the database could not answer: the words of `knowledge index failed`'s `reason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StoreError {
    BucketUnreachable,
    StoreFailed,
}

impl StoreError {
    pub(crate) fn reason(self) -> &'static str {
        match self {
            StoreError::BucketUnreachable => "bucket_unreachable",
            StoreError::StoreFailed => "store_failed",
        }
    }
}

impl From<buckwal::Error> for StoreError {
    fn from(error: buckwal::Error) -> Self {
        match error {
            buckwal::Error::Bucket(_)
            | buckwal::Error::UploadFailed(_)
            | buckwal::Error::Timeout(_) => StoreError::BucketUnreachable,
            _ => StoreError::StoreFailed,
        }
    }
}

pub(crate) type Result<T> = std::result::Result<T, StoreError>;

/// The bucket, as buckwal's `Config` needs it. The secrets were read from their files.
#[derive(Clone)]
pub(crate) struct BucketConfig {
    pub endpoint: String,
    pub bucket: String,
    pub prefix: String,
    pub access_key: String,
    pub secret_key: String,
    pub encryption_key_file: PathBuf,
}

impl std::fmt::Debug for BucketConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BucketConfig")
            .field("endpoint", &self.endpoint)
            .field("bucket", &self.bucket)
            .field("prefix", &self.prefix)
            .finish_non_exhaustive()
    }
}

/// One vault note as stored.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NoteRow {
    pub name: String,
    pub path: String,
    pub title: String,
    pub kind: String,
    pub description: String,
    pub created: String,
    pub links: Vec<String>,
    pub hash: String,
    pub changed_at_ms: i64,
    pub body: String,
}

/// A passage the database found close to a query.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Nearest {
    pub note: String,
    pub text: String,
    /// `1 - cosine distance`: 1 for the same meaning.
    pub similarity: f64,
}

/// What `index_info` holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IndexInfo {
    pub runtime: String,
    pub model: String,
    pub dimensions: usize,
    pub prompts: String,
}

impl From<&ModelInfo> for IndexInfo {
    fn from(info: &ModelInfo) -> Self {
        Self {
            runtime: info.runtime.clone(),
            model: info.model.clone(),
            dimensions: info.dimensions,
            prompts: info.prompts.clone(),
        }
    }
}

pub(crate) struct Store {
    // The database is held so that it closes after the connection.
    _db: Database,
    conn: Mutex<Connection>,
}

const SCHEMA: [&str; 3] = [
    "CREATE TABLE IF NOT EXISTS vault_notes(name TEXT PRIMARY KEY, path TEXT NOT NULL, title TEXT NOT NULL, kind TEXT NOT NULL, description TEXT NOT NULL, created TEXT NOT NULL, links TEXT NOT NULL, hash TEXT NOT NULL, changed_at INTEGER NOT NULL, body TEXT NOT NULL)",
    "CREATE TABLE IF NOT EXISTS passages(note TEXT NOT NULL, seq INTEGER NOT NULL, text TEXT NOT NULL, embedding BLOB NOT NULL, PRIMARY KEY(note, seq))",
    "CREATE TABLE IF NOT EXISTS index_info(runtime TEXT NOT NULL, model TEXT NOT NULL, dimensions INTEGER NOT NULL, prompts TEXT NOT NULL, built_at INTEGER NOT NULL)",
];

fn text(value: &Value) -> String {
    value.to_text().unwrap_or_default().to_string()
}

fn number(value: &Value) -> i64 {
    value.to_float_or_zero() as i64
}

fn vector_text(vector: &[f32]) -> String {
    let mut out = String::with_capacity(vector.len() * 10 + 2);
    out.push('[');
    for (at, x) in vector.iter().enumerate() {
        if at > 0 {
            out.push(',');
        }
        out.push_str(&x.to_string());
    }
    out.push(']');
    out
}

const NOTE_COLUMNS: &str =
    "name, path, title, kind, description, created, links, hash, changed_at, body";

fn note_row(row: &[Value]) -> NoteRow {
    NoteRow {
        name: text(&row[0]),
        path: text(&row[1]),
        title: text(&row[2]),
        kind: text(&row[3]),
        description: text(&row[4]),
        created: text(&row[5]),
        links: serde_json::from_str(&text(&row[6])).unwrap_or_default(),
        hash: text(&row[7]),
        changed_at_ms: number(&row[8]),
        body: text(&row[9]),
    }
}

impl Store {
    /// Opens the database in `dir` and creates its tables. An empty `dir` whose bucket
    /// prefix holds the database restores it from the bucket.
    pub(crate) fn open(dir: &Path, bucket: &BucketConfig) -> Result<Self> {
        Self::finish(buckwal::open(config_for(dir, bucket))?)
    }

    /// Opens the database over any object store, for tests.
    #[cfg(test)]
    pub(crate) fn open_with_store(
        dir: &Path,
        bucket: &BucketConfig,
        store: std::sync::Arc<dyn buckwal::store::ObjectStore>,
    ) -> Result<Self> {
        Self::finish(buckwal::open_with_store(config_for(dir, bucket), store)?)
    }

    fn finish(db: Database) -> Result<Self> {
        let conn = db.connect()?;
        for statement in SCHEMA {
            conn.execute(statement, &[])?;
        }
        Ok(Self {
            _db: db,
            conn: Mutex::new(conn),
        })
    }
}

fn config_for(dir: &Path, bucket: &BucketConfig) -> Config {
    let mut config = Config::new(
        dir,
        &bucket.endpoint,
        &bucket.bucket,
        &bucket.access_key,
        &bucket.secret_key,
    );
    config.prefix = bucket.prefix.clone();
    config.encryption_key_file = Some(bucket.encryption_key_file.clone());
    config.request_timeout = Duration::from_secs(10);
    config.retry_deadline = Duration::from_secs(20);
    config
}

impl Store {
    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn index_info(&self) -> Result<Option<IndexInfo>> {
        let rows = self.conn().query(
            "SELECT runtime, model, dimensions, prompts FROM index_info LIMIT 1",
            &[],
        )?;
        Ok(rows.first().map(|row| IndexInfo {
            runtime: text(&row[0]),
            model: text(&row[1]),
            dimensions: number(&row[2]) as usize,
            prompts: text(&row[3]),
        }))
    }

    /// Records the model the passages were embedded with.
    pub(crate) fn set_index_info(&self, info: &IndexInfo, built_at_ms: i64) -> Result<()> {
        let conn = self.conn();
        conn.transaction(|tx| {
            tx.execute("DELETE FROM index_info", &[])?;
            tx.execute(
                &format!(
                    "INSERT INTO index_info VALUES (?, ?, {}, ?, {built_at_ms})",
                    info.dimensions
                ),
                &[
                    Value::build_text(info.runtime.clone()),
                    Value::build_text(info.model.clone()),
                    Value::build_text(info.prompts.clone()),
                ],
            )?;
            Ok(())
        })?;
        Ok(())
    }

    /// Removes every vault note, passage and the model record.
    pub(crate) fn clear(&self) -> Result<()> {
        self.conn().transaction(|tx| {
            tx.execute("DELETE FROM passages", &[])?;
            tx.execute("DELETE FROM vault_notes", &[])?;
            tx.execute("DELETE FROM index_info", &[])?;
            Ok(())
        })?;
        Ok(())
    }

    /// Every vault note's content hash.
    pub(crate) fn hashes(&self) -> Result<HashMap<String, String>> {
        let rows = self
            .conn()
            .query("SELECT name, hash FROM vault_notes", &[])?;
        Ok(rows
            .iter()
            .map(|row| (text(&row[0]), text(&row[1])))
            .collect())
    }

    /// Replaces a vault note and its passages in one commit.
    pub(crate) fn replace_note(
        &self,
        note: &ParsedNote,
        path: &str,
        hash: &str,
        changed_at_ms: i64,
        passages: &[(String, Vec<f32>)],
    ) -> Result<()> {
        let links = serde_json::to_string(&note.links).unwrap_or_else(|_| "[]".into());
        self.conn().transaction(|tx| {
            let name = Value::build_text(note.name.clone());
            tx.execute(
                "DELETE FROM passages WHERE note = ?",
                std::slice::from_ref(&name),
            )?;
            tx.execute(
                "DELETE FROM vault_notes WHERE name = ?",
                std::slice::from_ref(&name),
            )?;
            tx.execute(
                &format!(
                    "INSERT INTO vault_notes VALUES (?, ?, ?, ?, ?, ?, ?, ?, {changed_at_ms}, ?)"
                ),
                &[
                    name.clone(),
                    Value::build_text(path.to_string()),
                    Value::build_text(note.title.clone()),
                    Value::build_text(note.kind.clone()),
                    Value::build_text(note.description.clone()),
                    Value::build_text(note.created.clone()),
                    Value::build_text(links.clone()),
                    Value::build_text(hash.to_string()),
                    Value::build_text(note.body.clone()),
                ],
            )?;
            for (seq, (passage, embedding)) in passages.iter().enumerate() {
                tx.execute(
                    &format!("INSERT INTO passages VALUES (?, {seq}, ?, vector32(?))"),
                    &[
                        name.clone(),
                        Value::build_text(passage.clone()),
                        Value::build_text(vector_text(embedding)),
                    ],
                )?;
            }
            Ok(())
        })?;
        Ok(())
    }

    pub(crate) fn delete_note(&self, name: &str) -> Result<()> {
        self.conn().transaction(|tx| {
            let name = Value::build_text(name.to_string());
            tx.execute(
                "DELETE FROM passages WHERE note = ?",
                std::slice::from_ref(&name),
            )?;
            tx.execute("DELETE FROM vault_notes WHERE name = ?", &[name])?;
            Ok(())
        })?;
        Ok(())
    }

    /// The number of vault notes and of passages.
    pub(crate) fn counts(&self) -> Result<(usize, usize)> {
        let conn = self.conn();
        let notes = conn.query("SELECT COUNT(*) FROM vault_notes", &[])?;
        let passages = conn.query("SELECT COUNT(*) FROM passages", &[])?;
        Ok((
            notes.first().map_or(0, |r| number(&r[0]) as usize),
            passages.first().map_or(0, |r| number(&r[0]) as usize),
        ))
    }

    /// The vault notes changed most recently, newest first.
    pub(crate) fn recent(&self, limit: usize) -> Result<Vec<NoteRow>> {
        let rows = self.conn().query(
            &format!(
                "SELECT {NOTE_COLUMNS} FROM vault_notes ORDER BY changed_at DESC, name LIMIT {limit}"
            ),
            &[],
        )?;
        Ok(rows.iter().map(|row| note_row(row)).collect())
    }

    pub(crate) fn note(&self, name: &str) -> Result<Option<NoteRow>> {
        let rows = self.conn().query(
            &format!("SELECT {NOTE_COLUMNS} FROM vault_notes WHERE name = ?"),
            &[Value::build_text(name.to_string())],
        )?;
        Ok(rows.first().map(|row| note_row(row)))
    }

    /// The notes with these names, in no order.
    pub(crate) fn notes(&self, names: &[String]) -> Result<HashMap<String, NoteRow>> {
        let mut found = HashMap::new();
        for name in names {
            if let Some(row) = self.note(name)? {
                found.insert(row.name.clone(), row);
            }
        }
        Ok(found)
    }

    /// The `limit` passages closest to `query`, closest first.
    pub(crate) fn nearest(&self, query: &[f32], limit: usize) -> Result<Vec<Nearest>> {
        let rows = self.conn().query(
            &format!(
                "SELECT note, text, vector_distance_cos(embedding, vector32(?)) AS d FROM passages ORDER BY d LIMIT {limit}"
            ),
            &[Value::build_text(vector_text(query))],
        )?;
        Ok(rows.iter().map(|row| nearest_row(row)).collect())
    }

    /// The passages of other notes closest to any passage of `name`, closest first.
    pub(crate) fn nearest_to_note(&self, name: &str, limit: usize) -> Result<Vec<Nearest>> {
        let name = Value::build_text(name.to_string());
        let rows = self.conn().query(
            &format!(
                "SELECT p2.note, p2.text, vector_distance_cos(p2.embedding, p1.embedding) AS d FROM passages p1, passages p2 WHERE p1.note = ? AND p2.note <> ? ORDER BY d LIMIT {limit}"
            ),
            &[name.clone(), name],
        )?;
        Ok(rows.iter().map(|row| nearest_row(row)).collect())
    }
}

fn nearest_row(row: &[Value]) -> Nearest {
    Nearest {
        note: text(&row[0]),
        text: text(&row[1]),
        similarity: 1.0 - row[2].to_float_or_zero(),
    }
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
