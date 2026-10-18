//! The backlog tool's own database, apart from Harold's core store.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::Path;
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use turso::Connection;

use super::schema::{Item, MAX_ITEMS, Note, NoteKind, Place, QueueItem, QueueListing, QueueState};

const DATABASE: &str = "backlog.db";

const MIGRATIONS: [(&str, &str); 1] = [("001_backlog", include_str!("migrations/001_backlog.sql"))];

#[derive(Debug, thiserror::Error)]
pub(super) enum StoreError {
    #[error("backlog database: {0}")]
    Turso(#[from] turso::Error),
    #[error("backlog data: {0}")]
    Data(String),
}

impl From<serde_json::Error> for StoreError {
    fn from(error: serde_json::Error) -> Self {
        Self::Data(error.to_string())
    }
}

type Result<T> = std::result::Result<T, StoreError>;

/// Why a write was refused; nothing is stored then.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Refusal {
    Full,
    NotFound,
    AppendOnly,
    NotEmpty,
    NotParked,
    AlreadyDone,
}

impl Refusal {
    pub(super) fn reason(self) -> &'static str {
        match self {
            Refusal::Full => "backlog_full",
            Refusal::NotFound => "not_found",
            Refusal::AppendOnly => "append_only",
            Refusal::NotEmpty => "not_empty",
            Refusal::NotParked => "not_parked",
            Refusal::AlreadyDone => "already_done",
        }
    }
}

pub(super) trait BacklogStore: Send + Sync + 'static {
    /// Stores the item and returns its rank. An existing id is replaced and
    /// keeps its rank; a new one goes last, or is `Full` when there are already
    /// `MAX_ITEMS`.
    fn put_item(
        &self,
        product: &str,
        item: Item,
        now_ms: i64,
    ) -> impl Future<Output = Result<std::result::Result<u32, Refusal>>> + Send;
    /// Ranks the listed ids first, in order, and the rest after them in the
    /// order they had. Returns the number of items; an unknown id is `NotFound`.
    fn rank_items(
        &self,
        product: &str,
        ids: &[String],
        now_ms: i64,
    ) -> impl Future<Output = Result<std::result::Result<u32, Refusal>>> + Send;
    /// Removes the item and closes up the ranks after it. A reason is kept as
    /// a `history` note, `removed <id>: <reason>`.
    fn remove_item(
        &self,
        product: &str,
        id: &str,
        reason: Option<&str>,
        now_ms: i64,
    ) -> impl Future<Output = Result<std::result::Result<(), Refusal>>> + Send;
    /// The product's items, by rank.
    fn list_items(&self, product: &str) -> impl Future<Output = Result<Vec<Item>>> + Send;
    /// The number of items of each product that has any.
    fn item_counts(&self) -> impl Future<Output = Result<BTreeMap<String, u32>>> + Send;
    /// Adds a note and returns its id.
    fn add_note(
        &self,
        product: &str,
        kind: NoteKind,
        title: &str,
        body: &str,
        now_ms: i64,
    ) -> impl Future<Output = Result<i64>> + Send;
    /// Replaces a note's title and body. A `history` note is `AppendOnly`.
    fn update_note(
        &self,
        product: &str,
        id: i64,
        title: &str,
        body: &str,
        now_ms: i64,
    ) -> impl Future<Output = Result<std::result::Result<(), Refusal>>> + Send;
    /// Removes a note. A `history` note is `AppendOnly`.
    fn remove_note(
        &self,
        product: &str,
        id: i64,
    ) -> impl Future<Output = Result<std::result::Result<(), Refusal>>> + Send;
    /// The product's notes, of one kind or all, by id; `last` keeps the newest n.
    fn list_notes(
        &self,
        product: &str,
        kind: Option<NoteKind>,
        last: Option<u32>,
    ) -> impl Future<Output = Result<Vec<Note>>> + Send;
    /// Whether the product has no item and no note.
    fn product_is_empty(&self, product: &str) -> impl Future<Output = Result<bool>> + Send;
    /// Adds a queue item with the next `N<k>` id. `Place::Now` parks the old
    /// Now at position 1; `Place::At(n)` is `Err("at")` unless n is in
    /// 1..=parked + 1.
    fn add_queue_item(
        &self,
        text: &str,
        place: Place,
        now_ms: i64,
    ) -> impl Future<Output = Result<std::result::Result<QueueItem, String>>> + Send;
    /// Makes a parked item Now and parks the old Now at position 1. Returns the
    /// Now id and the parked count; a done item is `AlreadyDone`.
    fn set_now(
        &self,
        id: &str,
        now_ms: i64,
    ) -> impl Future<Output = Result<std::result::Result<(String, u32), Refusal>>> + Send;
    /// Puts the listed parked ids first, in order, and the rest of the parked
    /// after them in the order they had. Returns the parked count; an id that is
    /// there but not parked is `NotParked`.
    fn order_queue(
        &self,
        ids: &[String],
        now_ms: i64,
    ) -> impl Future<Output = Result<std::result::Result<u32, Refusal>>> + Send;
    /// Replaces a queue item's text.
    fn update_queue_item(
        &self,
        id: &str,
        text: &str,
        now_ms: i64,
    ) -> impl Future<Output = Result<std::result::Result<(), Refusal>>> + Send;
    /// Marks a queue item done and closes up the parked positions. An item
    /// already done is `AlreadyDone`.
    fn done_queue_item(
        &self,
        id: &str,
        now_ms: i64,
    ) -> impl Future<Output = Result<std::result::Result<(), Refusal>>> + Send;
    /// Replaces the queue's note.
    fn set_queue_note(&self, text: &str) -> impl Future<Output = Result<()>> + Send;
    /// The queue, with the done items whose `done_at_ms >= done_since_ms`.
    fn list_queue(&self, done_since_ms: i64) -> impl Future<Output = Result<QueueListing>> + Send;
    /// Whether the queue has no item and no note.
    fn queue_is_empty(&self) -> impl Future<Output = Result<bool>> + Send;
}

pub(super) struct TursoBacklogStore {
    conn: Mutex<Connection>,
    // The database handle stays alive as long as the connection is used.
    _database: turso::Database,
}

impl TursoBacklogStore {
    pub(super) async fn open(folder: &Path) -> Result<Self> {
        tokio::fs::create_dir_all(folder)
            .await
            .map_err(|e| StoreError::Data(format!("create {}: {e}", folder.display())))?;
        let path = folder.join(DATABASE);
        let path = path
            .to_str()
            .ok_or_else(|| StoreError::Data("backlog database path is not UTF-8".into()))?;
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

/// Like `finish`, but a refusal is rolled back too, so a refused write leaves
/// nothing behind.
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
        return Err(StoreError::Data("incompatible backlog schema".into()));
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
                    (name, checksum, super::now_ms()),
                )
                .await?;
            }
        }
    }
    Ok(())
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

const ITEM_COLUMNS: &str =
    "id, rank, title, value, size, risk, reason, paths, after, evidence_json, updated_at_ms";

fn item_of(row: &turso::Row) -> Result<Item> {
    Ok(Item {
        id: text(row, 0)?,
        rank: count_of(integer(row, 1)?)?,
        title: text(row, 2)?,
        value: text(row, 3)?,
        size: text(row, 4)?,
        risk: text(row, 5)?,
        reason: text(row, 6)?,
        paths: text(row, 7)?,
        after: text(row, 8)?,
        evidence: serde_json::from_str(&text(row, 9)?)?,
        updated_at_ms: integer(row, 10)?,
    })
}

const NOTE_COLUMNS: &str = "id, kind, title, body, created_at_ms, updated_at_ms";

fn note_of(row: &turso::Row) -> Result<Note> {
    let kind = text(row, 1)?;
    Ok(Note {
        id: integer(row, 0)?,
        kind: NoteKind::parse(&kind)
            .ok_or_else(|| StoreError::Data(format!("unknown note kind {kind}")))?,
        title: text(row, 2)?,
        body: text(row, 3)?,
        created_at_ms: integer(row, 4)?,
        updated_at_ms: integer(row, 5)?,
    })
}

async fn rank_of(conn: &Connection, product: &str, id: &str) -> Result<Option<i64>> {
    let mut rows = conn
        .query(
            "SELECT rank FROM items WHERE product = ?1 AND id = ?2",
            (product, id),
        )
        .await?;
    match rows.next().await? {
        Some(row) => Ok(Some(integer(&row, 0)?)),
        None => Ok(None),
    }
}

/// The product's item ids, by rank.
async fn ranked_ids(conn: &Connection, product: &str) -> Result<Vec<String>> {
    let mut ids = Vec::new();
    let mut rows = conn
        .query(
            "SELECT id FROM items WHERE product = ?1 ORDER BY rank, id",
            [product],
        )
        .await?;
    while let Some(row) = rows.next().await? {
        ids.push(text(&row, 0)?);
    }
    Ok(ids)
}

async fn put_item_in(
    conn: &Connection,
    product: &str,
    item: &Item,
    now_ms: i64,
) -> Result<std::result::Result<u32, Refusal>> {
    let evidence = serde_json::to_string(&item.evidence)?;
    let rank = match rank_of(conn, product, &item.id).await? {
        Some(rank) => rank,
        None => {
            let held = ranked_ids(conn, product).await?.len();
            if held >= MAX_ITEMS {
                return Ok(Err(Refusal::Full));
            }
            held as i64 + 1
        }
    };
    conn.execute(
        "INSERT OR REPLACE INTO items \
         (product, id, rank, title, value, size, risk, reason, paths, after, evidence_json, \
          updated_at_ms) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        (
            product,
            item.id.as_str(),
            rank,
            item.title.as_str(),
            item.value.as_str(),
            item.size.as_str(),
            item.risk.as_str(),
            item.reason.as_str(),
            item.paths.as_str(),
            item.after.as_str(),
            evidence,
            now_ms,
        ),
    )
    .await?;
    Ok(Ok(count_of(rank)?))
}

async fn rank_items_in(
    conn: &Connection,
    product: &str,
    ids: &[String],
    now_ms: i64,
) -> Result<std::result::Result<u32, Refusal>> {
    let current = ranked_ids(conn, product).await?;
    if ids.iter().any(|id| !current.contains(id)) {
        return Ok(Err(Refusal::NotFound));
    }
    let mut order: Vec<&str> = Vec::with_capacity(current.len());
    for id in ids.iter().chain(&current) {
        if !order.contains(&id.as_str()) {
            order.push(id);
        }
    }
    for (rank, id) in order.iter().enumerate() {
        conn.execute(
            "UPDATE items SET rank = ?3, updated_at_ms = ?4 WHERE product = ?1 AND id = ?2",
            (product, *id, rank as i64 + 1, now_ms),
        )
        .await?;
    }
    Ok(Ok(count_of(order.len() as i64)?))
}

async fn insert_note(
    conn: &Connection,
    product: &str,
    kind: NoteKind,
    title: &str,
    body: &str,
    now_ms: i64,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO notes (product, kind, title, body, created_at_ms, updated_at_ms) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
        (product, kind.as_str(), title, body, now_ms),
    )
    .await?;
    let mut rows = conn.query("SELECT MAX(id) FROM notes", ()).await?;
    match rows.next().await? {
        Some(row) => integer(&row, 0),
        None => Err(StoreError::Data("note was not stored".into())),
    }
}

async fn remove_item_in(
    conn: &Connection,
    product: &str,
    id: &str,
    reason: Option<&str>,
    now_ms: i64,
) -> Result<std::result::Result<(), Refusal>> {
    let Some(rank) = rank_of(conn, product, id).await? else {
        return Ok(Err(Refusal::NotFound));
    };
    conn.execute(
        "DELETE FROM items WHERE product = ?1 AND id = ?2",
        (product, id),
    )
    .await?;
    conn.execute(
        "UPDATE items SET rank = rank - 1, updated_at_ms = ?3 WHERE product = ?1 AND rank > ?2",
        (product, rank, now_ms),
    )
    .await?;
    if let Some(reason) = reason {
        let title = format!("removed {id}");
        let body = format!("removed {id}: {reason}");
        insert_note(conn, product, NoteKind::History, &title, &body, now_ms).await?;
    }
    Ok(Ok(()))
}

async fn list_items_in(conn: &Connection, product: &str) -> Result<Vec<Item>> {
    let mut items = Vec::new();
    let mut rows = conn
        .query(
            &format!("SELECT {ITEM_COLUMNS} FROM items WHERE product = ?1 ORDER BY rank, id"),
            [product],
        )
        .await?;
    while let Some(row) = rows.next().await? {
        items.push(item_of(&row)?);
    }
    Ok(items)
}

async fn item_counts_in(conn: &Connection) -> Result<BTreeMap<String, u32>> {
    let mut counts = BTreeMap::new();
    let mut rows = conn
        .query("SELECT product, COUNT(*) FROM items GROUP BY product", ())
        .await?;
    while let Some(row) = rows.next().await? {
        counts.insert(text(&row, 0)?, count_of(integer(&row, 1)?)?);
    }
    Ok(counts)
}

/// A note of the product that may be changed: `NotFound` when there is none,
/// `AppendOnly` when it is `history`.
async fn changeable_note(
    conn: &Connection,
    product: &str,
    id: i64,
) -> Result<std::result::Result<(), Refusal>> {
    let mut rows = conn
        .query(
            "SELECT kind FROM notes WHERE product = ?1 AND id = ?2",
            (product, id),
        )
        .await?;
    let Some(row) = rows.next().await? else {
        return Ok(Err(Refusal::NotFound));
    };
    if text(&row, 0)? == NoteKind::History.as_str() {
        return Ok(Err(Refusal::AppendOnly));
    }
    Ok(Ok(()))
}

async fn update_note_in(
    conn: &Connection,
    product: &str,
    id: i64,
    title: &str,
    body: &str,
    now_ms: i64,
) -> Result<std::result::Result<(), Refusal>> {
    if let Err(refusal) = changeable_note(conn, product, id).await? {
        return Ok(Err(refusal));
    }
    conn.execute(
        "UPDATE notes SET title = ?3, body = ?4, updated_at_ms = ?5 \
         WHERE product = ?1 AND id = ?2",
        (product, id, title, body, now_ms),
    )
    .await?;
    Ok(Ok(()))
}

async fn remove_note_in(
    conn: &Connection,
    product: &str,
    id: i64,
) -> Result<std::result::Result<(), Refusal>> {
    if let Err(refusal) = changeable_note(conn, product, id).await? {
        return Ok(Err(refusal));
    }
    conn.execute(
        "DELETE FROM notes WHERE product = ?1 AND id = ?2",
        (product, id),
    )
    .await?;
    Ok(Ok(()))
}

async fn list_notes_in(
    conn: &Connection,
    product: &str,
    kind: Option<NoteKind>,
    last: Option<u32>,
) -> Result<Vec<Note>> {
    let kind = kind.map(NoteKind::as_str).unwrap_or("");
    let limit = last.map_or(i64::MAX, i64::from);
    let mut notes = Vec::new();
    // The newest `limit` notes, read newest first and turned back to id order.
    let mut rows = conn
        .query(
            &format!(
                "SELECT {NOTE_COLUMNS} FROM notes WHERE product = ?1 AND (?2 = '' OR kind = ?2) \
                 ORDER BY id DESC LIMIT ?3"
            ),
            (product, kind, limit),
        )
        .await?;
    while let Some(row) = rows.next().await? {
        notes.push(note_of(&row)?);
    }
    notes.reverse();
    Ok(notes)
}

async fn product_is_empty_in(conn: &Connection, product: &str) -> Result<bool> {
    let mut rows = conn
        .query(
            "SELECT 1 FROM items WHERE product = ?1 \
             UNION ALL SELECT 1 FROM notes WHERE product = ?1 LIMIT 1",
            [product],
        )
        .await?;
    Ok(rows.next().await?.is_none())
}

const QUEUE_COLUMNS: &str = "id, state, position, text, COALESCE(done_at_ms, 0), updated_at_ms";

fn queue_item_of(row: &turso::Row) -> Result<QueueItem> {
    let state = text(row, 1)?;
    Ok(QueueItem {
        id: text(row, 0)?,
        state: QueueState::parse(&state)
            .ok_or_else(|| StoreError::Data(format!("unknown queue state {state}")))?,
        position: count_of(integer(row, 2)?)?,
        text: text(row, 3)?,
        done_at_ms: integer(row, 4)?,
        updated_at_ms: integer(row, 5)?,
    })
}

/// The k of a queue id `N<k>`.
fn queue_number(id: &str) -> Result<u64> {
    id.strip_prefix('N')
        .and_then(|k| k.parse().ok())
        .ok_or_else(|| StoreError::Data(format!("queue id {id} is not N<k>")))
}

async fn queue_items_in(conn: &Connection) -> Result<Vec<QueueItem>> {
    let mut items = Vec::new();
    let mut rows = conn
        .query(&format!("SELECT {QUEUE_COLUMNS} FROM queue_items"), ())
        .await?;
    while let Some(row) = rows.next().await? {
        items.push(queue_item_of(&row)?);
    }
    Ok(items)
}

async fn queue_item_in(conn: &Connection, id: &str) -> Result<Option<QueueItem>> {
    let mut rows = conn
        .query(
            &format!("SELECT {QUEUE_COLUMNS} FROM queue_items WHERE id = ?1"),
            [id],
        )
        .await?;
    match rows.next().await? {
        Some(row) => Ok(Some(queue_item_of(&row)?)),
        None => Ok(None),
    }
}

/// The parked ids, by position.
async fn parked_ids(conn: &Connection) -> Result<Vec<String>> {
    let mut ids = Vec::new();
    let mut rows = conn
        .query(
            "SELECT id FROM queue_items WHERE state = 'parked' ORDER BY position",
            (),
        )
        .await?;
    while let Some(row) = rows.next().await? {
        ids.push(text(&row, 0)?);
    }
    Ok(ids)
}

async fn now_id(conn: &Connection) -> Result<Option<String>> {
    let mut rows = conn
        .query("SELECT id FROM queue_items WHERE state = 'now'", ())
        .await?;
    match rows.next().await? {
        Some(row) => Ok(Some(text(&row, 0)?)),
        None => Ok(None),
    }
}

/// Moves the parked items at `from` and after it by `by` positions.
async fn shift_parked(conn: &Connection, from: i64, by: i64, now_ms: i64) -> Result<()> {
    conn.execute(
        "UPDATE queue_items SET position = position + ?2, updated_at_ms = ?3 \
         WHERE state = 'parked' AND position >= ?1",
        (from, by, now_ms),
    )
    .await?;
    Ok(())
}

/// Parks the Now item, if there is one, at position 1. Runs before an item is
/// made Now, so the `one_now` index never sees two.
async fn park_the_now(conn: &Connection, now_ms: i64) -> Result<()> {
    let Some(id) = now_id(conn).await? else {
        return Ok(());
    };
    shift_parked(conn, 1, 1, now_ms).await?;
    conn.execute(
        "UPDATE queue_items SET state = 'parked', position = 1, updated_at_ms = ?2 \
         WHERE id = ?1",
        (id, now_ms),
    )
    .await?;
    Ok(())
}

async fn add_queue_item_in(
    conn: &Connection,
    text: &str,
    place: Place,
    now_ms: i64,
) -> Result<std::result::Result<QueueItem, String>> {
    let parked = parked_ids(conn).await?.len() as u32;
    let (state, position) = match place {
        Place::End => (QueueState::Parked, parked + 1),
        Place::Now => (QueueState::Now, 0),
        Place::At(n) if (1..=parked + 1).contains(&n) => (QueueState::Parked, n),
        Place::At(_) => return Ok(Err("at".into())),
    };
    let mut highest = 0;
    for item in queue_items_in(conn).await? {
        highest = highest.max(queue_number(&item.id)?);
    }
    let id = format!("N{}", highest + 1);
    match state {
        QueueState::Now => park_the_now(conn, now_ms).await?,
        _ => shift_parked(conn, i64::from(position), 1, now_ms).await?,
    }
    conn.execute(
        "INSERT INTO queue_items (id, state, position, text, done_at_ms, updated_at_ms) \
         VALUES (?1, ?2, ?3, ?4, NULL, ?5)",
        (
            id.as_str(),
            state.as_str(),
            i64::from(position),
            text,
            now_ms,
        ),
    )
    .await?;
    Ok(Ok(QueueItem {
        id,
        state,
        position,
        text: text.into(),
        done_at_ms: 0,
        updated_at_ms: now_ms,
    }))
}

async fn set_now_in(
    conn: &Connection,
    id: &str,
    now_ms: i64,
) -> Result<std::result::Result<(String, u32), Refusal>> {
    let Some(item) = queue_item_in(conn, id).await? else {
        return Ok(Err(Refusal::NotFound));
    };
    match item.state {
        QueueState::Done => return Ok(Err(Refusal::AlreadyDone)),
        QueueState::Now => {}
        QueueState::Parked => {
            // The item's own position is overwritten when it is made Now.
            shift_parked(conn, i64::from(item.position) + 1, -1, now_ms).await?;
            park_the_now(conn, now_ms).await?;
            conn.execute(
                "UPDATE queue_items SET state = 'now', position = 0, updated_at_ms = ?2 \
                 WHERE id = ?1",
                (id, now_ms),
            )
            .await?;
        }
    }
    let parked = count_of(parked_ids(conn).await?.len() as i64)?;
    Ok(Ok((id.to_string(), parked)))
}

async fn order_queue_in(
    conn: &Connection,
    ids: &[String],
    now_ms: i64,
) -> Result<std::result::Result<u32, Refusal>> {
    let current = parked_ids(conn).await?;
    if let Some(id) = ids.iter().find(|id| !current.contains(id)) {
        let refusal = match queue_item_in(conn, id).await? {
            Some(_) => Refusal::NotParked,
            None => Refusal::NotFound,
        };
        return Ok(Err(refusal));
    }
    let mut order: Vec<&str> = Vec::with_capacity(current.len());
    for id in ids.iter().chain(&current) {
        if !order.contains(&id.as_str()) {
            order.push(id);
        }
    }
    for (position, id) in order.iter().enumerate() {
        conn.execute(
            "UPDATE queue_items SET position = ?2, updated_at_ms = ?3 WHERE id = ?1",
            (*id, position as i64 + 1, now_ms),
        )
        .await?;
    }
    Ok(Ok(count_of(order.len() as i64)?))
}

async fn update_queue_item_in(
    conn: &Connection,
    id: &str,
    text: &str,
    now_ms: i64,
) -> Result<std::result::Result<(), Refusal>> {
    if queue_item_in(conn, id).await?.is_none() {
        return Ok(Err(Refusal::NotFound));
    }
    conn.execute(
        "UPDATE queue_items SET text = ?2, updated_at_ms = ?3 WHERE id = ?1",
        (id, text, now_ms),
    )
    .await?;
    Ok(Ok(()))
}

async fn done_queue_item_in(
    conn: &Connection,
    id: &str,
    now_ms: i64,
) -> Result<std::result::Result<(), Refusal>> {
    let Some(item) = queue_item_in(conn, id).await? else {
        return Ok(Err(Refusal::NotFound));
    };
    if item.state == QueueState::Done {
        return Ok(Err(Refusal::AlreadyDone));
    }
    conn.execute(
        "UPDATE queue_items SET state = 'done', position = 0, done_at_ms = ?2, \
         updated_at_ms = ?2 WHERE id = ?1",
        (id, now_ms),
    )
    .await?;
    if item.state == QueueState::Parked {
        shift_parked(conn, i64::from(item.position) + 1, -1, now_ms).await?;
    }
    Ok(Ok(()))
}

async fn set_queue_note_in(conn: &Connection, text: &str) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO queue_note (id, text) VALUES (1, ?1)",
        [text],
    )
    .await?;
    Ok(())
}

async fn queue_note_in(conn: &Connection) -> Result<String> {
    let mut rows = conn
        .query("SELECT text FROM queue_note WHERE id = 1", ())
        .await?;
    match rows.next().await? {
        Some(row) => text(&row, 0),
        None => Ok(String::new()),
    }
}

async fn list_queue_in(conn: &Connection, done_since_ms: i64) -> Result<QueueListing> {
    let mut listing = QueueListing {
        now: None,
        parked: Vec::new(),
        done: Vec::new(),
        note: queue_note_in(conn).await?,
    };
    for item in queue_items_in(conn).await? {
        match item.state {
            QueueState::Now => listing.now = Some(item),
            QueueState::Parked => listing.parked.push(item),
            QueueState::Done if item.done_at_ms >= done_since_ms => listing.done.push(item),
            QueueState::Done => {}
        }
    }
    listing.parked.sort_by_key(|item| item.position);
    // Ids compare by their number, so N10 follows N9.
    listing
        .done
        .sort_by_key(|item| (item.done_at_ms, queue_number(&item.id).unwrap_or(u64::MAX)));
    Ok(listing)
}

async fn queue_is_empty_in(conn: &Connection) -> Result<bool> {
    let mut rows = conn
        .query(
            "SELECT 1 FROM queue_items \
             UNION ALL SELECT 1 FROM queue_note WHERE text <> '' LIMIT 1",
            (),
        )
        .await?;
    Ok(rows.next().await?.is_none())
}

impl BacklogStore for TursoBacklogStore {
    async fn put_item(
        &self,
        product: &str,
        item: Item,
        now_ms: i64,
    ) -> Result<std::result::Result<u32, Refusal>> {
        let conn = self.conn.lock().await;
        begin(&conn).await?;
        finish_refusable(&conn, put_item_in(&conn, product, &item, now_ms).await).await
    }

    async fn rank_items(
        &self,
        product: &str,
        ids: &[String],
        now_ms: i64,
    ) -> Result<std::result::Result<u32, Refusal>> {
        let conn = self.conn.lock().await;
        begin(&conn).await?;
        finish_refusable(&conn, rank_items_in(&conn, product, ids, now_ms).await).await
    }

    async fn remove_item(
        &self,
        product: &str,
        id: &str,
        reason: Option<&str>,
        now_ms: i64,
    ) -> Result<std::result::Result<(), Refusal>> {
        let conn = self.conn.lock().await;
        begin(&conn).await?;
        finish_refusable(
            &conn,
            remove_item_in(&conn, product, id, reason, now_ms).await,
        )
        .await
    }

    async fn list_items(&self, product: &str) -> Result<Vec<Item>> {
        let conn = self.conn.lock().await;
        roll_back_abandoned(&conn).await?;
        list_items_in(&conn, product).await
    }

    async fn item_counts(&self) -> Result<BTreeMap<String, u32>> {
        let conn = self.conn.lock().await;
        roll_back_abandoned(&conn).await?;
        item_counts_in(&conn).await
    }

    async fn add_note(
        &self,
        product: &str,
        kind: NoteKind,
        title: &str,
        body: &str,
        now_ms: i64,
    ) -> Result<i64> {
        let conn = self.conn.lock().await;
        begin(&conn).await?;
        finish(
            &conn,
            insert_note(&conn, product, kind, title, body, now_ms).await,
        )
        .await
    }

    async fn update_note(
        &self,
        product: &str,
        id: i64,
        title: &str,
        body: &str,
        now_ms: i64,
    ) -> Result<std::result::Result<(), Refusal>> {
        let conn = self.conn.lock().await;
        begin(&conn).await?;
        finish_refusable(
            &conn,
            update_note_in(&conn, product, id, title, body, now_ms).await,
        )
        .await
    }

    async fn remove_note(
        &self,
        product: &str,
        id: i64,
    ) -> Result<std::result::Result<(), Refusal>> {
        let conn = self.conn.lock().await;
        begin(&conn).await?;
        finish_refusable(&conn, remove_note_in(&conn, product, id).await).await
    }

    async fn list_notes(
        &self,
        product: &str,
        kind: Option<NoteKind>,
        last: Option<u32>,
    ) -> Result<Vec<Note>> {
        let conn = self.conn.lock().await;
        roll_back_abandoned(&conn).await?;
        list_notes_in(&conn, product, kind, last).await
    }

    async fn product_is_empty(&self, product: &str) -> Result<bool> {
        let conn = self.conn.lock().await;
        roll_back_abandoned(&conn).await?;
        product_is_empty_in(&conn, product).await
    }

    async fn add_queue_item(
        &self,
        text: &str,
        place: Place,
        now_ms: i64,
    ) -> Result<std::result::Result<QueueItem, String>> {
        let conn = self.conn.lock().await;
        begin(&conn).await?;
        finish_refusable(&conn, add_queue_item_in(&conn, text, place, now_ms).await).await
    }

    async fn set_now(
        &self,
        id: &str,
        now_ms: i64,
    ) -> Result<std::result::Result<(String, u32), Refusal>> {
        let conn = self.conn.lock().await;
        begin(&conn).await?;
        finish_refusable(&conn, set_now_in(&conn, id, now_ms).await).await
    }

    async fn order_queue(
        &self,
        ids: &[String],
        now_ms: i64,
    ) -> Result<std::result::Result<u32, Refusal>> {
        let conn = self.conn.lock().await;
        begin(&conn).await?;
        finish_refusable(&conn, order_queue_in(&conn, ids, now_ms).await).await
    }

    async fn update_queue_item(
        &self,
        id: &str,
        text: &str,
        now_ms: i64,
    ) -> Result<std::result::Result<(), Refusal>> {
        let conn = self.conn.lock().await;
        begin(&conn).await?;
        finish_refusable(&conn, update_queue_item_in(&conn, id, text, now_ms).await).await
    }

    async fn done_queue_item(
        &self,
        id: &str,
        now_ms: i64,
    ) -> Result<std::result::Result<(), Refusal>> {
        let conn = self.conn.lock().await;
        begin(&conn).await?;
        finish_refusable(&conn, done_queue_item_in(&conn, id, now_ms).await).await
    }

    async fn set_queue_note(&self, text: &str) -> Result<()> {
        let conn = self.conn.lock().await;
        begin(&conn).await?;
        finish(&conn, set_queue_note_in(&conn, text).await).await
    }

    async fn list_queue(&self, done_since_ms: i64) -> Result<QueueListing> {
        let conn = self.conn.lock().await;
        roll_back_abandoned(&conn).await?;
        list_queue_in(&conn, done_since_ms).await
    }

    async fn queue_is_empty(&self) -> Result<bool> {
        let conn = self.conn.lock().await;
        roll_back_abandoned(&conn).await?;
        queue_is_empty_in(&conn).await
    }
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
