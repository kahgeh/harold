//! The question queue's own database, apart from Harold's core store.

use std::future::Future;
use std::path::Path;
use std::time::Duration;

use harold_api::tools::questions::v1 as pb;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use tracing::info;
use turso::Connection;

use super::NAME;
use super::schema::{Answer, Kind, Question, check_picks};

const DATABASE: &str = "questions.db";
const DISMISSAL_VISIBLE_MS: i64 = 3_600_000;

const MIGRATIONS: [(&str, &str); 5] = [
    (
        "001_questions",
        include_str!("migrations/001_questions.sql"),
    ),
    (
        "002_answer_send_now",
        include_str!("migrations/002_answer_send_now.sql"),
    ),
    (
        "003_step_ticks",
        include_str!("migrations/003_step_ticks.sql"),
    ),
    (
        "004_saved_picks",
        include_str!("migrations/004_saved_picks.sql"),
    ),
    (
        "005_answer_trace",
        include_str!("migrations/005_answer_trace.sql"),
    ),
];

#[derive(Debug, thiserror::Error)]
pub(super) enum StoreError {
    #[error("question database: {0}")]
    Turso(#[from] turso::Error),
    #[error("question data: {0}")]
    Data(String),
}

impl StoreError {
    /// Which side failed, for the log: the database or the stored data.
    pub(super) fn kind(&self) -> &'static str {
        match self {
            Self::Turso(_) => "database",
            Self::Data(_) => "data",
        }
    }
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
    /// The id holds instructions, which the action does not apply to.
    NotAQuestion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TickRefusal {
    VersionChanged,
    NotWaiting,
    InvalidStep,
}

/// One saved pick: `decision` counts from 0; `text` is set only for a free option.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct SavedPick {
    pub decision: u32,
    pub key: String,
    pub text: String,
}

/// A stored question as `ShowQuestion` reports it, in any state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Shown {
    pub question: Question,
    pub version: u32,
    pub state: String,
    pub reason: String,
    pub picks: Vec<SavedPick>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Listed {
    pub question: Question,
    pub version: u32,
    pub deferred: bool,
    /// Ticked step numbers, from 1, ascending. Empty for a question.
    pub ticked: Vec<u32>,
    /// The saved picks of a question, by decision.
    pub picks: Vec<SavedPick>,
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
    /// The `traceparent` of each answer in the batch that came with one, in the order of
    /// the answers. Kept in memory only: a batch found after a restart has none.
    pub traces: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Ready {
    /// Id, version, question and answer of each unsent answer, by position.
    pub answers: Vec<(String, u32, Question, Answer)>,
    /// Ids of the deferred questions.
    pub later: Vec<String>,
    /// The `traceparent` of each unsent answer that came with one, in the order of the
    /// answers.
    pub traces: Vec<String>,
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
    fn answer_traced(
        &self,
        id: &str,
        version: u32,
        send_now: bool,
        trace_parent: Option<&str>,
        answer: impl FnOnce(&Question) -> std::result::Result<Answer, String> + Send,
        now_ms: i64,
    ) -> impl Future<Output = Result<std::result::Result<std::result::Result<(), String>, Refusal>>> + Send;
    /// `answer_traced` for an answer that carries no trace, such as one from the dash.
    fn answer(
        &self,
        id: &str,
        version: u32,
        send_now: bool,
        answer: impl FnOnce(&Question) -> std::result::Result<Answer, String> + Send,
        now_ms: i64,
    ) -> impl Future<Output = Result<std::result::Result<std::result::Result<(), String>, Refusal>>> + Send
    {
        self.answer_traced(id, version, send_now, None, answer, now_ms)
    }
    fn defer(
        &self,
        id: &str,
        now_ms: i64,
    ) -> impl Future<Output = Result<std::result::Result<u32, Refusal>>> + Send;
    /// Ticks or unticks one step of waiting or deferred instructions.
    fn tick(
        &self,
        id: &str,
        version: u32,
        step: u32,
        ticked: bool,
        now_ms: i64,
    ) -> impl Future<Output = Result<std::result::Result<(), TickRefusal>>> + Send;
    /// Replaces the saved picks of a waiting or deferred question at `version`.
    /// The inner `Err(field)` is `picks`; nothing is stored then.
    fn save_picks(
        &self,
        id: &str,
        version: u32,
        picks: &[pb::Pick],
    ) -> impl Future<Output = Result<std::result::Result<std::result::Result<(), String>, Refusal>>> + Send;
    /// The stored question in any state, with its saved picks; `None` for an unknown id.
    fn show(&self, id: &str) -> impl Future<Output = Result<Option<Shown>>> + Send;
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

fn parse_ticks(ticks: &str) -> Vec<u32> {
    let mut parsed: Vec<u32> = ticks
        .split(',')
        .filter_map(|part| part.trim().parse().ok())
        .collect();
    parsed.sort_unstable();
    parsed.dedup();
    parsed
}

fn format_ticks(ticks: &[u32]) -> String {
    ticks
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

fn parse_picks(picks: &str) -> Vec<SavedPick> {
    serde_json::from_str(picks).unwrap_or_default()
}

fn format_picks(picks: &[SavedPick]) -> Result<String> {
    if picks.is_empty() {
        return Ok(String::new());
    }
    Ok(serde_json::to_string(picks)?)
}

/// The saved picks of the new version: a decision takes the pick of an old decision with
/// the same ask and options, each old decision used once, in order, renumbered.
fn carried_picks(old: &Question, old_picks: &[SavedPick], new: &Question) -> Vec<SavedPick> {
    let mut available: Vec<&SavedPick> = old_picks.iter().collect();
    let mut carried = Vec::new();
    for (i, decision) in new.decisions.iter().enumerate() {
        let found = available.iter().position(|pick| {
            old.decisions
                .get(pick.decision as usize)
                .is_some_and(|d| d.ask == decision.ask && d.options == decision.options)
        });
        if let Some(at) = found {
            let pick = available.remove(at);
            carried.push(SavedPick {
                decision: i as u32,
                ..pick.clone()
            });
        }
    }
    carried
}

/// The ticks of the new version: a step is ticked when an old ticked step has
/// the same text, each old step used once, in order.
fn carried_ticks(old: &Question, old_ticks: &[u32], new: &Question) -> Vec<u32> {
    let mut available: Vec<(usize, &str)> = old
        .steps
        .iter()
        .enumerate()
        .filter(|(i, _)| old_ticks.contains(&(*i as u32 + 1)))
        .map(|(i, step)| (i, step.text.as_str()))
        .collect();
    let mut carried = Vec::new();
    for (i, step) in new.steps.iter().enumerate() {
        if let Some(at) = available.iter().position(|(_, text)| *text == step.text) {
            available.remove(at);
            carried.push(i as u32 + 1);
        }
    }
    carried
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
    let (ticks, picks) = carried_of(conn, &question.id, question).await?;
    let (ticks, picks) = (format_ticks(&ticks), format_picks(&picks)?);
    if open_state(&existing.state) {
        conn.execute(
            "UPDATE questions SET version = ?2, state = 'waiting', body = ?3, answer = NULL, \
             reason = '', batch_id = NULL, send_now = 0, trace_parent = NULL, updated_at_ms = ?4, ticks = ?5, picks = ?6 WHERE id = ?1",
            (
                question.id.as_str(),
                i64::from(version),
                body,
                now_ms,
                ticks,
                picks,
            ),
        )
        .await?;
    } else {
        let position = next_position(conn).await?;
        conn.execute(
            "UPDATE questions SET version = ?2, position = ?5, state = 'waiting', body = ?3, \
             answer = NULL, reason = '', batch_id = NULL, send_now = 0, trace_parent = NULL, updated_at_ms = ?4, \
             ticks = ?6, picks = ?7 WHERE id = ?1",
            (
                question.id.as_str(),
                i64::from(version),
                body,
                now_ms,
                position,
                ticks,
                picks,
            ),
        )
        .await?;
    }
    Ok(version)
}

/// The ticks and the saved picks the new version of `id` carries from the stored one.
async fn carried_of(
    conn: &Connection,
    id: &str,
    new: &Question,
) -> Result<(Vec<u32>, Vec<SavedPick>)> {
    let mut rows = conn
        .query(
            "SELECT body, ticks, picks FROM questions WHERE id = ?1",
            [id],
        )
        .await?;
    let Some(row) = rows.next().await? else {
        return Ok((Vec::new(), Vec::new()));
    };
    let old: Question = serde_json::from_str(&text(&row, 0)?)?;
    Ok((
        carried_ticks(&old, &parse_ticks(&text(&row, 1)?), new),
        carried_picks(&old, &parse_picks(&text(&row, 2)?), new),
    ))
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
    return_deferred_when_idle(conn, now_ms, "dismiss").await?;
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
            "SELECT body, version, state, ticks, picks FROM questions WHERE state IN ('waiting','deferred') \
             ORDER BY CASE state WHEN 'waiting' THEN 0 ELSE 1 END, position, id",
            (),
        )
        .await?;
    while let Some(row) = rows.next().await? {
        questions.push(Listed {
            question: serde_json::from_str(&text(&row, 0)?)?,
            version: version_of(&row, 1)?,
            deferred: text(&row, 2)? == "deferred",
            ticked: parse_ticks(&text(&row, 3)?),
            picks: parse_picks(&text(&row, 4)?),
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
    trace_parent: Option<&str>,
    resolve: impl FnOnce(&Question) -> std::result::Result<Answer, String>,
    now_ms: i64,
) -> Result<std::result::Result<std::result::Result<(), String>, Refusal>> {
    let mut rows = conn
        .query(
            "SELECT version, state, body, ticks FROM questions WHERE id = ?1",
            [id],
        )
        .await?;
    let Some(row) = rows.next().await? else {
        return Ok(Err(Refusal::NotWaiting));
    };
    let current = version_of(&row, 0)?;
    let state = text(&row, 1)?;
    let body = text(&row, 2)?;
    let ticks = parse_ticks(&text(&row, 3)?);
    drop(rows);
    if !open_state(&state) {
        return Ok(Err(Refusal::NotWaiting));
    }
    if current != version {
        return Ok(Err(Refusal::VersionChanged));
    }
    let question: Question = serde_json::from_str(&body)?;
    let mut answer = match resolve(&question) {
        Ok(answer) => answer,
        Err(field) => return Ok(Ok(Err(field))),
    };
    if answer.kind == Kind::Done {
        answer.ticked = u32::try_from(ticks.len()).unwrap_or(u32::MAX);
    }
    let saved: Vec<SavedPick> = answer
        .picks
        .iter()
        .map(|picked| SavedPick {
            decision: picked.decision,
            key: picked.key.clone(),
            text: if picked.free {
                picked.label.clone()
            } else {
                String::new()
            },
        })
        .collect();
    conn.execute(
        "UPDATE questions SET state = 'answered', answer = ?2, updated_at_ms = ?3, \
         send_now = ?4, picks = ?5, trace_parent = ?6 WHERE id = ?1",
        (
            id,
            serde_json::to_string(&answer)?,
            now_ms,
            i64::from(send_now),
            format_picks(&saved)?,
            trace_parent,
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
    if body_of(conn, id).await?.is_instructions() {
        return Ok(Err(Refusal::NotAQuestion));
    }
    let position = next_position(conn).await?;
    conn.execute(
        "UPDATE questions SET state = 'deferred', position = ?2, updated_at_ms = ?3 WHERE id = ?1",
        (id, position, now_ms),
    )
    .await?;
    return_deferred_when_idle(conn, now_ms, "later").await?;
    Ok(Ok(existing.version))
}

async fn body_of(conn: &Connection, id: &str) -> Result<Question> {
    let mut rows = conn
        .query("SELECT body FROM questions WHERE id = ?1", [id])
        .await?;
    let row = rows
        .next()
        .await?
        .ok_or_else(|| StoreError::Data("question row is missing".into()))?;
    Ok(serde_json::from_str(&text(&row, 0)?)?)
}

/// Ids of the waiting questions; waiting instructions are left out, so they
/// never hold set-aside questions back.
async fn waiting_question_ids(conn: &Connection) -> Result<Vec<String>> {
    let mut ids = Vec::new();
    let mut rows = conn
        .query(
            "SELECT id, body FROM questions WHERE state = 'waiting' ORDER BY position, id",
            (),
        )
        .await?;
    while let Some(row) = rows.next().await? {
        let question: Question = serde_json::from_str(&text(&row, 1)?)?;
        if !question.is_instructions() {
            ids.push(text(&row, 0)?);
        }
    }
    Ok(ids)
}

async fn tick_in(
    conn: &Connection,
    id: &str,
    version: u32,
    step: u32,
    ticked: bool,
) -> Result<std::result::Result<(), TickRefusal>> {
    let mut rows = conn
        .query(
            "SELECT version, state, body, ticks FROM questions WHERE id = ?1",
            [id],
        )
        .await?;
    let Some(row) = rows.next().await? else {
        return Ok(Err(TickRefusal::NotWaiting));
    };
    let current = version_of(&row, 0)?;
    let state = text(&row, 1)?;
    let question: Question = serde_json::from_str(&text(&row, 2)?)?;
    let mut ticks = parse_ticks(&text(&row, 3)?);
    drop(rows);
    if !open_state(&state) {
        return Ok(Err(TickRefusal::NotWaiting));
    }
    if current != version {
        return Ok(Err(TickRefusal::VersionChanged));
    }
    if step == 0 || step as usize > question.steps.len() {
        return Ok(Err(TickRefusal::InvalidStep));
    }
    ticks.retain(|held| *held != step);
    if ticked {
        ticks.push(step);
        ticks.sort_unstable();
    }
    conn.execute(
        "UPDATE questions SET ticks = ?2 WHERE id = ?1",
        (id, format_ticks(&ticks)),
    )
    .await?;
    Ok(Ok(()))
}

async fn save_picks_in(
    conn: &Connection,
    id: &str,
    version: u32,
    picks: &[pb::Pick],
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
    let question: Question = serde_json::from_str(&text(&row, 2)?)?;
    drop(rows);
    if !open_state(&state) {
        return Ok(Err(Refusal::NotWaiting));
    }
    if current != version {
        return Ok(Err(Refusal::VersionChanged));
    }
    if let Err(field) = check_picks(&question, picks) {
        return Ok(Ok(Err(field)));
    }
    // Text belongs to a free option only, as in an answer.
    let saved: Vec<SavedPick> = picks
        .iter()
        .map(|pick| {
            let free = question
                .decisions
                .get(pick.decision as usize)
                .and_then(|d| d.options.iter().find(|o| o.key == pick.key))
                .is_some_and(|o| o.free);
            SavedPick {
                decision: pick.decision,
                key: pick.key.clone(),
                text: if free {
                    pick.text.clone()
                } else {
                    String::new()
                },
            }
        })
        .collect();
    conn.execute(
        "UPDATE questions SET picks = ?2 WHERE id = ?1",
        (id, format_picks(&saved)?),
    )
    .await?;
    Ok(Ok(Ok(())))
}

async fn show_in(conn: &Connection, id: &str) -> Result<Option<Shown>> {
    let mut rows = conn
        .query(
            "SELECT body, version, state, reason, picks FROM questions WHERE id = ?1",
            [id],
        )
        .await?;
    let Some(row) = rows.next().await? else {
        return Ok(None);
    };
    Ok(Some(Shown {
        question: serde_json::from_str(&text(&row, 0)?)?,
        version: version_of(&row, 1)?,
        state: text(&row, 2)?,
        reason: text(&row, 3)?,
        picks: parse_picks(&text(&row, 4)?),
    }))
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
    let mut traces = Vec::new();
    let mut newest = i64::MIN;
    let mut marked = false;
    let mut rows = conn
        .query(
            "SELECT id, version, body, answer, updated_at_ms, send_now, trace_parent FROM questions \
             WHERE state = 'answered' AND batch_id IS NULL ORDER BY position, id",
            (),
        )
        .await?;
    while let Some(row) = rows.next().await? {
        let answer = optional_text(&row, 3)?
            .ok_or_else(|| StoreError::Data("answered question has no answer".into()))?;
        newest = newest.max(integer(&row, 4)?);
        marked |= integer(&row, 5)? == 1;
        traces.extend(optional_text(&row, 6)?);
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
        traces,
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
        traces: Vec::new(),
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
        traces: Vec::new(),
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
    if waiting_question_ids(conn).await?.is_empty() {
        return_deferred(conn, now_ms, "answers_line").await?;
    }
    Ok(())
}

/// Nothing waiting and no answer in the way of a line: the set-aside
/// questions would never come back, so they come back now. A defer and a
/// dismiss both end here.
async fn return_deferred_when_idle(
    conn: &Connection,
    now_ms: i64,
    after: &'static str,
) -> Result<()> {
    if waiting_question_ids(conn).await?.is_empty()
        && ids_in_state(conn, "answered").await?.is_empty()
    {
        return_deferred(conn, now_ms, after).await?;
    }
    Ok(())
}

/// Deferred questions wait again, after everything else. Logs how many came
/// back, when any did.
async fn return_deferred(conn: &Connection, now_ms: i64, after: &'static str) -> Result<()> {
    let ids = ids_in_state(conn, "deferred").await?;
    for id in &ids {
        let position = next_position(conn).await?;
        conn.execute(
            "UPDATE questions SET state = 'waiting', position = ?2, updated_at_ms = ?3 \
             WHERE id = ?1",
            (id.as_str(), position, now_ms),
        )
        .await?;
    }
    if !ids.is_empty() {
        info!(
            tool = NAME,
            count = ids.len(),
            after,
            "set-aside questions returned"
        );
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

    async fn answer_traced(
        &self,
        id: &str,
        version: u32,
        send_now: bool,
        trace_parent: Option<&str>,
        answer: impl FnOnce(&Question) -> std::result::Result<Answer, String> + Send,
        now_ms: i64,
    ) -> Result<std::result::Result<std::result::Result<(), String>, Refusal>> {
        let conn = self.conn.lock().await;
        begin(&conn).await?;
        finish(
            &conn,
            answer_in(&conn, id, version, send_now, trace_parent, answer, now_ms).await,
        )
        .await
    }

    async fn defer(&self, id: &str, now_ms: i64) -> Result<std::result::Result<u32, Refusal>> {
        let conn = self.conn.lock().await;
        begin(&conn).await?;
        finish(&conn, defer_in(&conn, id, now_ms).await).await
    }

    async fn tick(
        &self,
        id: &str,
        version: u32,
        step: u32,
        ticked: bool,
        _now_ms: i64,
    ) -> Result<std::result::Result<(), TickRefusal>> {
        let conn = self.conn.lock().await;
        begin(&conn).await?;
        finish(&conn, tick_in(&conn, id, version, step, ticked).await).await
    }

    async fn save_picks(
        &self,
        id: &str,
        version: u32,
        picks: &[pb::Pick],
    ) -> Result<std::result::Result<std::result::Result<(), String>, Refusal>> {
        let conn = self.conn.lock().await;
        begin(&conn).await?;
        finish(&conn, save_picks_in(&conn, id, version, picks).await).await
    }

    async fn show(&self, id: &str) -> Result<Option<Shown>> {
        let conn = self.conn.lock().await;
        roll_back_abandoned(&conn).await?;
        show_in(&conn, id).await
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
