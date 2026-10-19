//! The question queue's own database, apart from Harold's core store.

use std::future::Future;
use std::path::Path;
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use turso::Connection;

use super::schema::{Answer, Question};

const DATABASE: &str = "questions.db";
const DISMISSAL_VISIBLE_MS: i64 = 3_600_000;

const MIGRATIONS: [(&str, &str); 2] = [
    (
        "001_questions",
        include_str!("migrations/001_questions.sql"),
    ),
    (
        "002_answer_send_now",
        include_str!("migrations/002_answer_send_now.sql"),
    ),
];

#[derive(Debug, thiserror::Error)]
pub(super) enum StoreError {
    #[error("question database: {0}")]
    Turso(#[from] turso::Error),
    #[error("question data: {0}")]
    Data(String),
}

impl From<serde_json::Error> for StoreError {
    fn from(error: serde_json::Error) -> Self {
        Self::Data(error.to_string())
    }
}

type Result<T> = std::result::Result<T, StoreError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Refusal {
    VersionChanged,
    NotWaiting,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Listed {
    pub question: Question,
    pub version: u32,
    pub deferred: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Listing {
    pub questions: Vec<Listed>,
    /// Id and reason of questions dismissed within the last hour.
    pub dismissed: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StartedBatch {
    pub id: i64,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Ready {
    /// Id, version, question and answer of each unsent answer, by position.
    pub answers: Vec<(String, u32, Question, Answer)>,
    /// Ids of the deferred questions.
    pub later: Vec<String>,
}

/// What the unsent answers allow now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Readiness {
    /// No answer is unsent.
    Nothing,
    /// Unsent answers are still settling; `wait_ms` is in `1..=settle_ms`.
    Settling {
        wait_ms: i64,
    },
    Ready(Ready),
}

pub(super) trait QuestionStore: Send + Sync + 'static {
    /// Stores the question: version 1, or the next version of an existing id.
    fn put(&self, question: Question, now_ms: i64) -> impl Future<Output = Result<u32>> + Send;
    fn dismiss(
        &self,
        id: &str,
        reason: &str,
        now_ms: i64,
    ) -> impl Future<Output = Result<std::result::Result<u32, Refusal>>> + Send;
    fn reorder(&self, ids: &[String], now_ms: i64) -> impl Future<Output = Result<()>> + Send;
    fn list(&self, now_ms: i64) -> impl Future<Output = Result<Listing>> + Send;
    /// Records the answer the closure resolves from the stored question. The
    /// inner `Err(field)` is the closure's refusal; nothing is stored then.
    /// `send_now` is the mark that asks for the line without the settle wait.
    fn answer(
        &self,
        id: &str,
        version: u32,
        send_now: bool,
        answer: impl FnOnce(&Question) -> std::result::Result<Answer, String> + Send,
        now_ms: i64,
    ) -> impl Future<Output = Result<std::result::Result<std::result::Result<(), String>, Refusal>>> + Send;
    fn defer(
        &self,
        id: &str,
        now_ms: i64,
    ) -> impl Future<Output = Result<std::result::Result<u32, Refusal>>> + Send;
    /// The unsent answers are ready when one is marked send-now, or when the
    /// newest has settled for `settle_ms`. Waiting questions do not hold them.
    fn ready(&self, now_ms: i64, settle_ms: i64) -> impl Future<Output = Result<Readiness>> + Send;
    /// Records a batch of the answers `ready` returned, as `(id, version)`.
    /// Stores nothing and returns `None` when, by now, a listed answer is no
    /// longer answered and unsent at that version.
    fn start_batch(
        &self,
        answers: &[(String, u32)],
        text: &str,
        now_ms: i64,
    ) -> impl Future<Output = Result<Option<StartedBatch>>> + Send;
    fn started_batch(&self) -> impl Future<Output = Result<Option<StartedBatch>>> + Send;
    fn batch_typed(&self, batch_id: i64, now_ms: i64) -> impl Future<Output = Result<()>> + Send;
    /// Undoes a started batch that was never typed, in one transaction: deletes
    /// its row and takes its questions out of it, so its answers are unsent
    /// again and the next round builds the batch anew. Returns whether it was
    /// released; a typed or unknown batch is left as it is.
    fn release_batch(&self, batch_id: i64) -> impl Future<Output = Result<bool>> + Send;
}

pub(super) struct TursoQuestionStore {
    conn: Mutex<Connection>,
    // The database handle stays alive as long as the connection is used.
    _database: turso::Database,
}

impl TursoQuestionStore {
    pub(super) async fn open(folder: &Path) -> Result<Self> {
        tokio::fs::create_dir_all(folder)
            .await
            .map_err(|e| StoreError::Data(format!("create {}: {e}", folder.display())))?;
        let path = folder.join(DATABASE);
        let path = path
            .to_str()
            .ok_or_else(|| StoreError::Data("question database path is not UTF-8".into()))?;
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

#[cfg(test)]
impl TursoQuestionStore {
    /// Whether the stored question carries the send-now mark.
    pub(super) async fn marked(&self, id: &str) -> bool {
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query("SELECT send_now FROM questions WHERE id = ?1", [id])
            .await
            .unwrap();
        let row = rows.next().await.unwrap().expect("question row");
        integer(&row, 0).unwrap() == 1
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
    roll_back_abandoned(conn).await?;
    conn.execute("BEGIN IMMEDIATE", ()).await?;
    Ok(())
}

/// Rolls back a transaction a cancelled write left open, so the next call,
/// read or write, starts from committed data.
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
        return Err(StoreError::Data("incompatible question schema".into()));
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

fn version_of(row: &turso::Row, index: usize) -> Result<u32> {
    u32::try_from(integer(row, index)?).map_err(|_| StoreError::Data("version out of range".into()))
}

struct Stored {
    version: u32,
    state: String,
}

async fn stored(conn: &Connection, id: &str) -> Result<Option<Stored>> {
    let mut rows = conn
        .query("SELECT version, state FROM questions WHERE id = ?1", [id])
        .await?;
    let Some(row) = rows.next().await? else {
        return Ok(None);
    };
    Ok(Some(Stored {
        version: version_of(&row, 0)?,
        state: text(&row, 1)?,
    }))
}

fn open_state(state: &str) -> bool {
    state == "waiting" || state == "deferred"
}

async fn next_position(conn: &Connection) -> Result<i64> {
    let mut rows = conn
        .query("SELECT COALESCE(MAX(position), 0) + 1 FROM questions", ())
        .await?;
    match rows.next().await? {
        Some(row) => integer(&row, 0),
        None => Ok(1),
    }
}

async fn put_in(conn: &Connection, question: &Question, now_ms: i64) -> Result<u32> {
    let body = serde_json::to_string(question)?;
    let Some(existing) = stored(conn, &question.id).await? else {
        let position = next_position(conn).await?;
        conn.execute(
            "INSERT INTO questions (id, version, position, state, body, updated_at_ms) \
             VALUES (?1, 1, ?2, 'waiting', ?3, ?4)",
            (question.id.as_str(), position, body, now_ms),
        )
        .await?;
        return Ok(1);
    };
    let version = existing.version + 1;
    if open_state(&existing.state) {
        conn.execute(
            "UPDATE questions SET version = ?2, state = 'waiting', body = ?3, answer = NULL, \
             reason = '', batch_id = NULL, send_now = 0, updated_at_ms = ?4 WHERE id = ?1",
            (question.id.as_str(), i64::from(version), body, now_ms),
        )
        .await?;
    } else {
        let position = next_position(conn).await?;
        conn.execute(
            "UPDATE questions SET version = ?2, position = ?5, state = 'waiting', body = ?3, \
             answer = NULL, reason = '', batch_id = NULL, send_now = 0, updated_at_ms = ?4 WHERE id = ?1",
            (
                question.id.as_str(),
                i64::from(version),
                body,
                now_ms,
                position,
            ),
        )
        .await?;
    }
    Ok(version)
}

async fn dismiss_in(
    conn: &Connection,
    id: &str,
    reason: &str,
    now_ms: i64,
) -> Result<std::result::Result<u32, Refusal>> {
    let Some(existing) = stored(conn, id).await? else {
        return Ok(Err(Refusal::NotWaiting));
    };
    if !open_state(&existing.state) {
        return Ok(Err(Refusal::NotWaiting));
    }
    conn.execute(
        "UPDATE questions SET state = 'dismissed', reason = ?2, updated_at_ms = ?3 WHERE id = ?1",
        (id, reason, now_ms),
    )
    .await?;
    Ok(Ok(existing.version))
}

async fn reorder_in(conn: &Connection, ids: &[String], now_ms: i64) -> Result<()> {
    let mut current: Vec<(String, i64)> = Vec::new();
    let mut rows = conn
        .query(
            "SELECT id, position FROM questions WHERE state IN ('waiting','deferred') \
             ORDER BY position, id",
            (),
        )
        .await?;
    while let Some(row) = rows.next().await? {
        current.push((text(&row, 0)?, integer(&row, 1)?));
    }
    drop(rows);

    // The positions these questions already use are handed out again in the new order.
    let slots: Vec<i64> = current.iter().map(|(_, position)| *position).collect();
    let mut order: Vec<&str> = Vec::with_capacity(current.len());
    for id in ids {
        let known = current.iter().any(|(held, _)| held == id);
        if known && !order.contains(&id.as_str()) {
            order.push(id);
        }
    }
    for (held, _) in &current {
        if !order.contains(&held.as_str()) {
            order.push(held);
        }
    }
    for (id, position) in order.into_iter().zip(slots) {
        conn.execute(
            "UPDATE questions SET position = ?2, updated_at_ms = ?3 WHERE id = ?1",
            (id, position, now_ms),
        )
        .await?;
    }
    Ok(())
}

async fn list_in(conn: &Connection, now_ms: i64) -> Result<Listing> {
    let mut questions = Vec::new();
    let mut rows = conn
        .query(
            "SELECT body, version, state FROM questions WHERE state IN ('waiting','deferred') \
             ORDER BY CASE state WHEN 'waiting' THEN 0 ELSE 1 END, position, id",
            (),
        )
        .await?;
    while let Some(row) = rows.next().await? {
        questions.push(Listed {
            question: serde_json::from_str(&text(&row, 0)?)?,
            version: version_of(&row, 1)?,
            deferred: text(&row, 2)? == "deferred",
        });
    }
    drop(rows);

    let mut dismissed = Vec::new();
    let mut rows = conn
        .query(
            "SELECT id, reason FROM questions WHERE state = 'dismissed' AND updated_at_ms >= ?1 \
             ORDER BY updated_at_ms, id",
            [now_ms - DISMISSAL_VISIBLE_MS],
        )
        .await?;
    while let Some(row) = rows.next().await? {
        dismissed.push((text(&row, 0)?, text(&row, 1)?));
    }
    Ok(Listing {
        questions,
        dismissed,
    })
}

async fn answer_in(
    conn: &Connection,
    id: &str,
    version: u32,
    send_now: bool,
    resolve: impl FnOnce(&Question) -> std::result::Result<Answer, String>,
    now_ms: i64,
) -> Result<std::result::Result<std::result::Result<(), String>, Refusal>> {
    let mut rows = conn
        .query(
            "SELECT version, state, body FROM questions WHERE id = ?1",
            [id],
        )
        .await?;
    let Some(row) = rows.next().await? else {
        return Ok(Err(Refusal::NotWaiting));
    };
    let current = version_of(&row, 0)?;
    let state = text(&row, 1)?;
    let body = text(&row, 2)?;
    drop(rows);
    if !open_state(&state) {
        return Ok(Err(Refusal::NotWaiting));
    }
    if current != version {
        return Ok(Err(Refusal::VersionChanged));
    }
    let question: Question = serde_json::from_str(&body)?;
    let answer = match resolve(&question) {
        Ok(answer) => answer,
        Err(field) => return Ok(Ok(Err(field))),
    };
    conn.execute(
        "UPDATE questions SET state = 'answered', answer = ?2, updated_at_ms = ?3, \
         send_now = ?4 WHERE id = ?1",
        (
            id,
            serde_json::to_string(&answer)?,
            now_ms,
            i64::from(send_now),
        ),
    )
    .await?;
    Ok(Ok(Ok(())))
}

async fn defer_in(
    conn: &Connection,
    id: &str,
    now_ms: i64,
) -> Result<std::result::Result<u32, Refusal>> {
    let Some(existing) = stored(conn, id).await? else {
        return Ok(Err(Refusal::NotWaiting));
    };
    if !open_state(&existing.state) {
        return Ok(Err(Refusal::NotWaiting));
    }
    let position = next_position(conn).await?;
    conn.execute(
        "UPDATE questions SET state = 'deferred', position = ?2, updated_at_ms = ?3 WHERE id = ?1",
        (id, position, now_ms),
    )
    .await?;
    // Nothing waiting and no answer in the way of a line: the set-aside
    // questions would never come back, so they come back now.
    if ids_in_state(conn, "waiting").await?.is_empty()
        && ids_in_state(conn, "answered").await?.is_empty()
    {
        return_deferred(conn, now_ms).await?;
    }
    Ok(Ok(existing.version))
}

async fn ids_in_state(conn: &Connection, state: &str) -> Result<Vec<String>> {
    let mut ids = Vec::new();
    let mut rows = conn
        .query(
            "SELECT id FROM questions WHERE state = ?1 ORDER BY position, id",
            [state],
        )
        .await?;
    while let Some(row) = rows.next().await? {
        ids.push(text(&row, 0)?);
    }
    Ok(ids)
}

async fn ready_in(conn: &Connection, now_ms: i64, settle_ms: i64) -> Result<Readiness> {
    let mut answers = Vec::new();
    let mut newest = i64::MIN;
    let mut marked = false;
    let mut rows = conn
        .query(
            "SELECT id, version, body, answer, updated_at_ms, send_now FROM questions \
             WHERE state = 'answered' AND batch_id IS NULL ORDER BY position, id",
            (),
        )
        .await?;
    while let Some(row) = rows.next().await? {
        let answer = optional_text(&row, 3)?
            .ok_or_else(|| StoreError::Data("answered question has no answer".into()))?;
        newest = newest.max(integer(&row, 4)?);
        marked |= integer(&row, 5)? == 1;
        answers.push((
            text(&row, 0)?,
            version_of(&row, 1)?,
            serde_json::from_str(&text(&row, 2)?)?,
            serde_json::from_str(&answer)?,
        ));
    }
    drop(rows);
    if answers.is_empty() {
        return Ok(Readiness::Nothing);
    }
    // A stamp in the future (the wall clock stepped back) counts as settled,
    // so it never holds a line back.
    let elapsed = now_ms.saturating_sub(newest);
    if !marked && (0..settle_ms).contains(&elapsed) {
        return Ok(Readiness::Settling {
            wait_ms: settle_ms - elapsed,
        });
    }
    Ok(Readiness::Ready(Ready {
        answers,
        later: ids_in_state(conn, "deferred").await?,
    }))
}

/// Whether the question is still answered, unsent, at this version.
async fn unsent_at(conn: &Connection, id: &str, version: u32) -> Result<bool> {
    let mut rows = conn
        .query(
            "SELECT 1 FROM questions WHERE id = ?1 AND version = ?2 \
             AND state = 'answered' AND batch_id IS NULL",
            (id, i64::from(version)),
        )
        .await?;
    Ok(rows.next().await?.is_some())
}

async fn start_batch_in(
    conn: &Connection,
    answers: &[(String, u32)],
    text_of_batch: &str,
    now_ms: i64,
) -> Result<Option<StartedBatch>> {
    // `ready` and this call take the lock apart, so a write may land between
    // them: an answer replaced.
    if answers.is_empty() {
        return Ok(None);
    }
    for (id, version) in answers {
        if !unsent_at(conn, id, *version).await? {
            return Ok(None);
        }
    }
    conn.execute(
        "INSERT INTO batches (text, started_at_ms) VALUES (?1, ?2)",
        (text_of_batch, now_ms),
    )
    .await?;
    let mut rows = conn.query("SELECT MAX(id) FROM batches", ()).await?;
    let id = match rows.next().await? {
        Some(row) => integer(&row, 0)?,
        None => return Err(StoreError::Data("batch was not stored".into())),
    };
    drop(rows);
    for (question, _) in answers {
        conn.execute(
            "UPDATE questions SET batch_id = ?2 \
             WHERE id = ?1 AND state = 'answered' AND batch_id IS NULL",
            (question.as_str(), id),
        )
        .await?;
    }
    Ok(Some(StartedBatch {
        id,
        text: text_of_batch.to_string(),
    }))
}

async fn started_batch_in(conn: &Connection) -> Result<Option<StartedBatch>> {
    let mut rows = conn
        .query(
            "SELECT id, text FROM batches WHERE typed_at_ms IS NULL ORDER BY id LIMIT 1",
            (),
        )
        .await?;
    let Some(row) = rows.next().await? else {
        return Ok(None);
    };
    Ok(Some(StartedBatch {
        id: integer(&row, 0)?,
        text: text(&row, 1)?,
    }))
}

async fn batch_typed_in(conn: &Connection, batch_id: i64, now_ms: i64) -> Result<()> {
    conn.execute(
        "UPDATE batches SET typed_at_ms = ?2 WHERE id = ?1",
        (batch_id, now_ms),
    )
    .await?;
    conn.execute(
        "UPDATE questions SET state = 'sent', updated_at_ms = ?2 \
         WHERE batch_id = ?1 AND state = 'answered'",
        (batch_id, now_ms),
    )
    .await?;
    // Deferred questions wait again, after everything else, once none is waiting.
    if ids_in_state(conn, "waiting").await?.is_empty() {
        return_deferred(conn, now_ms).await?;
    }
    Ok(())
}

/// Deferred questions wait again, after everything else.
async fn return_deferred(conn: &Connection, now_ms: i64) -> Result<()> {
    for id in ids_in_state(conn, "deferred").await? {
        let position = next_position(conn).await?;
        conn.execute(
            "UPDATE questions SET state = 'waiting', position = ?2, updated_at_ms = ?3 \
             WHERE id = ?1",
            (id, position, now_ms),
        )
        .await?;
    }
    Ok(())
}

async fn release_batch_in(conn: &Connection, batch_id: i64) -> Result<bool> {
    let mut rows = conn
        .query(
            "SELECT 1 FROM batches WHERE id = ?1 AND typed_at_ms IS NULL",
            (batch_id,),
        )
        .await?;
    let started = rows.next().await?.is_some();
    drop(rows);
    if !started {
        return Ok(false);
    }
    conn.execute("DELETE FROM batches WHERE id = ?1", (batch_id,))
        .await?;
    conn.execute(
        "UPDATE questions SET batch_id = NULL WHERE batch_id = ?1",
        (batch_id,),
    )
    .await?;
    Ok(true)
}

impl QuestionStore for TursoQuestionStore {
    async fn put(&self, question: Question, now_ms: i64) -> Result<u32> {
        let conn = self.conn.lock().await;
        begin(&conn).await?;
        finish(&conn, put_in(&conn, &question, now_ms).await).await
    }

    async fn dismiss(
        &self,
        id: &str,
        reason: &str,
        now_ms: i64,
    ) -> Result<std::result::Result<u32, Refusal>> {
        let conn = self.conn.lock().await;
        begin(&conn).await?;
        finish(&conn, dismiss_in(&conn, id, reason, now_ms).await).await
    }

    async fn reorder(&self, ids: &[String], now_ms: i64) -> Result<()> {
        let conn = self.conn.lock().await;
        begin(&conn).await?;
        finish(&conn, reorder_in(&conn, ids, now_ms).await).await
    }

    async fn list(&self, now_ms: i64) -> Result<Listing> {
        let conn = self.conn.lock().await;
        roll_back_abandoned(&conn).await?;
        list_in(&conn, now_ms).await
    }

    async fn answer(
        &self,
        id: &str,
        version: u32,
        send_now: bool,
        answer: impl FnOnce(&Question) -> std::result::Result<Answer, String> + Send,
        now_ms: i64,
    ) -> Result<std::result::Result<std::result::Result<(), String>, Refusal>> {
        let conn = self.conn.lock().await;
        begin(&conn).await?;
        finish(
            &conn,
            answer_in(&conn, id, version, send_now, answer, now_ms).await,
        )
        .await
    }

    async fn defer(&self, id: &str, now_ms: i64) -> Result<std::result::Result<u32, Refusal>> {
        let conn = self.conn.lock().await;
        begin(&conn).await?;
        finish(&conn, defer_in(&conn, id, now_ms).await).await
    }

    async fn ready(&self, now_ms: i64, settle_ms: i64) -> Result<Readiness> {
        let conn = self.conn.lock().await;
        roll_back_abandoned(&conn).await?;
        ready_in(&conn, now_ms, settle_ms).await
    }

    async fn start_batch(
        &self,
        answers: &[(String, u32)],
        text: &str,
        now_ms: i64,
    ) -> Result<Option<StartedBatch>> {
        let conn = self.conn.lock().await;
        begin(&conn).await?;
        finish(&conn, start_batch_in(&conn, answers, text, now_ms).await).await
    }

    async fn started_batch(&self) -> Result<Option<StartedBatch>> {
        let conn = self.conn.lock().await;
        roll_back_abandoned(&conn).await?;
        started_batch_in(&conn).await
    }

    async fn batch_typed(&self, batch_id: i64, now_ms: i64) -> Result<()> {
        let conn = self.conn.lock().await;
        begin(&conn).await?;
        finish(&conn, batch_typed_in(&conn, batch_id, now_ms).await).await
    }

    async fn release_batch(&self, batch_id: i64) -> Result<bool> {
        let conn = self.conn.lock().await;
        begin(&conn).await?;
        finish(&conn, release_batch_in(&conn, batch_id).await).await
    }
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
