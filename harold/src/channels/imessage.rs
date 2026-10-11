use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::{mpsc, watch};
use tracing::{Instrument, info, info_span, warn};

use super::{AwayNotification, split_body, summarise_for_notification};
use crate::inbound::AgentAddress;
use crate::inbound::ownership::{self, notification_text};
use crate::outbound::is_messaging_paused;
use crate::settings::get_settings;
use crate::store::{HaroldStore, InboundMessage, TurnCompleted, append_inbound_message};
use crate::util::sanitise_for_applescript;

// ===========================================================================
// Sending
// ===========================================================================

/// Marks text as written by Harold. The listener skips marked text when it reads
/// chat.db, so Harold never routes its own messages back in as replies.
const HAROLD_MARKER: char = '🤖';

fn is_marked_as_harold(text: &str) -> bool {
    text.starts_with(HAROLD_MARKER)
}

/// AppleScript that sends `text` to `recipient`. Every message goes out marked here,
/// in the one place that builds a send: an unmarked "Delivered" confirmation is read
/// back as a reply, routed, and confirmed again, without end.
fn send_script(text: &str, recipient: &str) -> String {
    let safe_text = sanitise_for_applescript(&format!("{HAROLD_MARKER} {text}"));
    let safe_recipient = sanitise_for_applescript(recipient);
    let escaped = safe_text.replace('\\', "\\\\").replace('"', "\\\"");
    let escaped_recipient = safe_recipient.replace('\\', "\\\\").replace('"', "\\\"");
    format!("tell application \"Messages\" to send \"{escaped}\" to buddy \"{escaped_recipient}\"")
}

fn send_imessage_to(text: &str, recipient: &str) -> Result<(), String> {
    let status = Command::new("osascript")
        .args(["-e", &send_script(text, recipient)])
        .status()
        .map_err(|error| format!("failed to start osascript: {error}"))?;
    if !status.success() {
        return Err(format!("osascript exited unsuccessfully: {status}"));
    }
    Ok(())
}

/// Send an iMessage notification to `recipient`.
fn send_raw_imessage(text: &str, recipient: &str) -> Result<(), String> {
    info!(msg = %text, "sending iMessage notification");
    send_imessage_to(text, recipient)
}

/// Send a confirmation or error reply to the configured recipient.
pub(crate) fn send_imessage(msg: &str) -> Result<(), String> {
    info!(msg, "sending iMessage");
    let cfg = get_settings();
    let Some(recipient) = cfg.imessage.recipient.as_deref() else {
        return Err("iMessage recipient is not configured".into());
    };
    send_imessage_to(msg, recipient)
}

fn recent_outgoing_texts(handle_id: i64) -> Vec<String> {
    let db_path = get_settings().chat_db.resolved_path();
    let out = Command::new("sqlite3")
        .arg("-json")
        .arg(db_path)
        .arg(format!(
            "SELECT text FROM message WHERE handle_id = {handle_id} AND is_from_me = 1 \
             ORDER BY ROWID DESC LIMIT 2;"
        ))
        .output();
    let Ok(out) = out else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    serde_json::from_slice::<Vec<serde_json::Value>>(&out.stdout)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|row| row.get("text")?.as_str().map(ToString::to_string))
        .collect()
}

#[derive(Debug, PartialEq, Eq)]
enum NotificationPlan<'a> {
    SendAll,
    SendQuestionOnly(&'a str),
    Skip,
}

fn normalize_notification_text(text: &str) -> &str {
    text.trim().trim_start_matches('🤖').trim()
}

fn notification_plan<'a>(
    recent_outgoing: &[&str],
    message: &str,
    question: Option<&'a str>,
) -> NotificationPlan<'a> {
    let Some(last) = recent_outgoing.first().copied() else {
        return NotificationPlan::SendAll;
    };
    let last = normalize_notification_text(last);

    if last == message.trim() {
        return question.map_or(NotificationPlan::Skip, NotificationPlan::SendQuestionOnly);
    }
    if question.is_some_and(|question| last == question.trim())
        && recent_outgoing
            .get(1)
            .is_some_and(|previous| normalize_notification_text(previous) == message.trim())
    {
        return NotificationPlan::Skip;
    }
    NotificationPlan::SendAll
}

// ---------------------------------------------------------------------------
// Away notification via iMessage — returns the source agent address
// ---------------------------------------------------------------------------

/// The notification text for `turn`, naming the Mac that sends it.
fn notification_message(machine: &str, turn: &TurnCompleted, main_body: &str) -> String {
    notification_text(machine, &turn.pane_label, main_body, &turn.main_context)
}

pub(crate) fn notify_away(
    turn: &TurnCompleted,
    _trace_id: &str,
) -> Result<AwayNotification, String> {
    let cfg = get_settings();
    let Some(recipient) = cfg.imessage.recipient.as_deref() else {
        warn!("iMessage recipient not configured");
        return Err("iMessage recipient is not configured".into());
    };
    let body = summarise_for_notification(&turn.assistant_message, &turn.last_user_prompt);

    let (main_body, question) = split_body(&body);
    let message = notification_message(&cfg.machine_name, turn, main_body);

    let recent_outgoing = cfg
        .imessage
        .handle_ids
        .first()
        .map_or_else(Vec::new, |&id| recent_outgoing_texts(id));
    let recent_outgoing = recent_outgoing
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    match notification_plan(&recent_outgoing, &message, question) {
        NotificationPlan::SendAll => {
            send_raw_imessage(&message, recipient)?;
            info!("iMessage notification sent");
            if let Some(question) = question {
                send_raw_imessage(question, recipient)?;
                info!("iMessage question sent");
            }
        }
        NotificationPlan::SendQuestionOnly(question) => {
            send_raw_imessage(question, recipient)?;
            info!("iMessage question retry sent");
        }
        NotificationPlan::Skip => {
            info!("iMessage skipped (duplicate)");
            return Ok(AwayNotification::Skipped);
        }
    }

    Ok(AwayNotification::Sent(AgentAddress::TmuxPane {
        pane_id: turn.pane_id.clone(),
        label: turn.pane_label.clone(),
    }))
}

// ===========================================================================
// Listening
// ===========================================================================

const START_ROW_RETRY: Duration = Duration::from_secs(5);

/// How many of the last marked rows the listener reads when it starts.
const STARTUP_MARKED_ROWS: usize = 50;

/// The last row read by each of the listener's three queries.
#[derive(Debug)]
struct Cursors {
    inbound: AtomicI64,
    self_sent: AtomicI64,
    /// Rows a Harold sent, in both directions. They name the Mac that notified last.
    marked: AtomicI64,
}

impl Cursors {
    fn starting_at(rowid: i64) -> Self {
        Self {
            inbound: AtomicI64::new(rowid),
            self_sent: AtomicI64::new(rowid),
            marked: AtomicI64::new(rowid),
        }
    }
}

static CURSORS: OnceLock<Cursors> = OnceLock::new();

fn cursors() -> &'static Cursors {
    CURSORS.get().expect("listener not initialised")
}

/// Which of the two queries a row came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Inbound,
    SelfSent,
}

struct FetchedRow {
    rowid: i64,
    text: String,
    direction: Direction,
}

/// How long a recorded row is remembered, to catch its copy from the other direction.
const TWIN_WINDOW: Duration = Duration::from_secs(10);

/// Rows recorded within the last `TWIN_WINDOW` whose twin has not been seen. A message the
/// Mac sends to the user's own number is stored as two rows with the same text, one per
/// direction, so without this a reply typed in Messages on the Mac is routed twice.
struct RecentRows(Vec<(Direction, String, Instant)>);

impl RecentRows {
    const fn new() -> Self {
        Self(Vec::new())
    }

    /// Whether this row is the copy of one recorded from the other direction within the
    /// window. The oldest such row is forgotten, so each recorded row absorbs one twin and
    /// the same text sent again is kept, as is the same text twice from one direction.
    fn take_twin(&mut self, direction: Direction, text: &str, now: Instant) -> bool {
        self.forget_old(now);
        let twin = self.0.iter().position(|(recorded, recorded_text, _)| {
            *recorded != direction && recorded_text == text
        });
        let Some(twin) = twin else {
            return false;
        };
        self.0.remove(twin);
        true
    }

    fn remember(&mut self, direction: Direction, text: String, now: Instant) {
        self.forget_old(now);
        self.0.push((direction, text, now));
    }

    fn forget_old(&mut self, now: Instant) {
        self.0
            .retain(|(_, _, at)| now.saturating_duration_since(*at) <= TWIN_WINDOW);
    }
}

static RECENT_ROWS: Mutex<RecentRows> = Mutex::new(RecentRows::new());

fn db_path() -> String {
    get_settings().chat_db.resolved_path()
}

/// The newest row in the database at `db`, or why it could not be read. An empty table
/// reads as row 0; a failed read must not, or every stored message would later look new.
fn max_rowid_of(db: &str) -> Result<i64, String> {
    let out = Command::new("sqlite3")
        .arg(db)
        .arg("SELECT COALESCE(MAX(ROWID), 0) FROM message;")
        .output()
        .map_err(|error| format!("sqlite3 did not start: {error}"))?;
    parse_max_rowid(out.status.success(), &out.stdout).ok_or_else(|| {
        first_line(
            &String::from_utf8_lossy(&out.stderr),
            "sqlite3 gave no row number",
        )
    })
}

fn parse_max_rowid(succeeded: bool, stdout: &[u8]) -> Option<i64> {
    if !succeeded {
        return None;
    }
    String::from_utf8_lossy(stdout).trim().parse().ok()
}

/// The first line of `text`, or `when_empty` if it has none.
fn first_line(text: &str, when_empty: &str) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    if line.is_empty() {
        return when_empty.into();
    }
    line.chars().take(200).collect()
}

/// Calls `read` until it succeeds, so the listener never starts from what it did not read.
/// A failure is logged once. Returns `None` if shutdown is signalled first.
async fn wait_for_start_row<T>(
    mut read: impl FnMut() -> Result<T, String>,
    retry_every: Duration,
    shutdown: &mut watch::Receiver<()>,
) -> Option<T> {
    let mut warned = false;
    loop {
        match read() {
            Ok(start) => return Some(start),
            Err(cause) if !warned => {
                warn!(
                    %cause,
                    "cannot read the Messages database; inbound messages wait until it can be read"
                );
                warned = true;
            }
            Err(_) => {}
        }
        tokio::select! {
            biased;

            _ = shutdown.changed() => return None,
            () = tokio::time::sleep(retry_every) => {}
        }
    }
}

/// A row a Harold sent, as the listener reads it to find the latest notifier.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MarkedRow {
    rowid: i64,
    /// The send time Messages stores with the row. A row with none reads as 0.
    date: i64,
    text: String,
}

/// What the listener reads before its first poll: the newest row, which every cursor
/// starts after, and the last marked rows up to it.
fn read_start(db: &ChatDb) -> Result<(i64, Vec<MarkedRow>), String> {
    let initial = max_rowid_of(&db.path)?;
    let marked = db.latest_marked(initial, STARTUP_MARKED_ROWS)?;
    Ok((initial, marked))
}

/// The Messages database and the conversations in it that Harold reads.
#[derive(Debug, Clone)]
struct ChatDb {
    path: String,
    handle_ids: Vec<i64>,
    /// Whether the rows a Harold sent are read. Only when the `machines` setting is set:
    /// a single Mac keeps no latest notifier, so it runs no marked query that could fail
    /// and hold back its replies.
    read_marked: bool,
}

impl ChatDb {
    fn configured() -> Self {
        let cfg = get_settings();
        Self {
            path: cfg.chat_db.resolved_path(),
            handle_ids: cfg.imessage.handle_ids.clone(),
            read_marked: !cfg.machines.is_empty(),
        }
    }

    /// The rows after `last_rowid` in one direction that the user wrote, oldest first.
    /// Rows Harold sent are left out.
    fn replies_after(&self, last_rowid: i64, is_from_me: u8) -> Vec<(i64, String)> {
        let Some(id_list) = self.id_list() else {
            return vec![];
        };
        let mut rows = self.query(&format!(
            "SELECT ROWID, text FROM message \
             WHERE ROWID > {last_rowid} AND handle_id IN ({id_list}) AND is_from_me = {is_from_me} \
               AND text IS NOT NULL AND length(text) > 0 \
             ORDER BY ROWID ASC;"
        ));
        rows.retain(|(_, text)| !is_marked_as_harold(text));
        rows
    }

    /// The rows after `last_rowid` that a Harold sent, from this Mac or another, in both
    /// directions, oldest first, or why they could not be read. None while `read_marked`
    /// is off.
    fn marked_after(&self, last_rowid: i64) -> Result<Vec<MarkedRow>, String> {
        let Some(id_list) = self.marked_id_list() else {
            return Ok(vec![]);
        };
        self.query_marked(&format!(
            "SELECT ROWID, COALESCE(date, 0) AS date, text FROM message \
             WHERE ROWID > {last_rowid} AND handle_id IN ({id_list}) \
               AND text LIKE '{HAROLD_MARKER}%' \
             ORDER BY ROWID ASC;"
        ))
    }

    /// The last `limit` rows a Harold sent, up to row `up_to_rowid`, oldest first, or why
    /// they could not be read. None while `read_marked` is off.
    fn latest_marked(&self, up_to_rowid: i64, limit: usize) -> Result<Vec<MarkedRow>, String> {
        let Some(id_list) = self.marked_id_list() else {
            return Ok(vec![]);
        };
        self.query_marked(&format!(
            "SELECT ROWID, date, text FROM (\
               SELECT ROWID, COALESCE(date, 0) AS date, text FROM message \
               WHERE ROWID <= {up_to_rowid} AND handle_id IN ({id_list}) \
                 AND text LIKE '{HAROLD_MARKER}%' \
               ORDER BY ROWID DESC LIMIT {limit}\
             ) ORDER BY ROWID ASC;"
        ))
    }

    /// The handle ids for a marked query, or `None` when no marked row is to be read.
    fn marked_id_list(&self) -> Option<String> {
        self.read_marked.then(|| self.id_list()).flatten()
    }

    /// The handle ids as an SQL list, or `None` when none is configured. Every value put
    /// into a query is an integer from the configuration or from internal state, never
    /// user input.
    fn id_list(&self) -> Option<String> {
        if self.handle_ids.is_empty() {
            return None;
        }
        let ids = self
            .handle_ids
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        Some(ids.join(","))
    }

    /// The (ROWID, trimmed text) of each row `sql` selects. A failed read gives no rows.
    fn query(&self, sql: &str) -> Vec<(i64, String)> {
        let rows = self.select(sql).unwrap_or_default();
        rows.iter()
            .filter_map(|row| Some((row.get("ROWID")?.as_i64()?, trimmed_text(row)?)))
            .collect()
    }

    /// The marked row of each row `sql` selects, or why they could not be read.
    fn query_marked(&self, sql: &str) -> Result<Vec<MarkedRow>, String> {
        let rows = self.select(sql)?;
        Ok(rows
            .iter()
            .filter_map(|row| {
                Some(MarkedRow {
                    rowid: row.get("ROWID")?.as_i64()?,
                    date: row.get("date")?.as_i64()?,
                    text: trimmed_text(row)?,
                })
            })
            .collect())
    }

    /// The rows `sql` selects, or why sqlite3 could not read them. `sqlite3 -json` prints
    /// nothing when no row matches, which is no rows, not a failure.
    fn select(&self, sql: &str) -> Result<Vec<serde_json::Value>, String> {
        let out = Command::new("sqlite3")
            .arg("-json")
            .arg(&self.path)
            .arg(sql)
            .output()
            .map_err(|error| format!("sqlite3 did not start: {error}"))?;
        if !out.status.success() {
            return Err(first_line(
                &String::from_utf8_lossy(&out.stderr),
                &format!("sqlite3 exited unsuccessfully: {}", out.status),
            ));
        }
        if out.stdout.trim_ascii().is_empty() {
            return Ok(vec![]);
        }
        serde_json::from_slice(&out.stdout)
            .map_err(|error| format!("sqlite3 printed JSON that cannot be read: {error}"))
    }
}

/// The trimmed `text` of `row`, or `None` when it has none.
fn trimmed_text(row: &serde_json::Value) -> Option<String> {
    let text = row.get("text")?.as_str()?.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// Record one fetched row as an inbound event and move `cursor` past it. While messaging
/// is paused the row is discarded: the cursor still moves, so nothing is replayed on resume.
/// A row whose text was just recorded from the other direction is skipped the same way.
async fn record_row(
    store: &HaroldStore,
    row: FetchedRow,
    cursor: &AtomicI64,
    recent: &Mutex<RecentRows>,
    messaging_paused: bool,
    now: Instant,
) {
    let FetchedRow {
        rowid,
        text,
        direction,
    } = row;
    if messaging_paused {
        info!("iMessage discarded (messaging paused)");
        cursor.store(rowid, Ordering::Relaxed);
        return;
    }
    if recent.lock().unwrap().take_twin(direction, &text, now) {
        info!("iMessage skipped (same text already recorded from the other direction)");
        cursor.store(rowid, Ordering::Relaxed);
        return;
    }
    let message = InboundMessage {
        text,
        forwarded: false,
    };
    match append_inbound_message(store, &message).await {
        Ok(()) => {
            cursor.store(rowid, Ordering::Relaxed);
            recent
                .lock()
                .unwrap()
                .remember(direction, message.text, now);
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to append InboundMessageReceived event")
        }
    }
}

/// Feed the rows a Harold sent to `record_marker` with their dates, oldest first, and move
/// `cursor` past them. They are never recorded as replies.
fn read_marked_rows(rows: Vec<MarkedRow>, cursor: &AtomicI64, record_marker: impl Fn(&str, i64)) {
    for row in rows {
        record_marker(&row.text, row.date);
        cursor.store(row.rowid, Ordering::Relaxed);
    }
}

/// The cursors of a listener that starts after row `initial`. `marked`, the last marked
/// rows stored up to that row, are fed to `record_marker` first, so a restart keeps the
/// latest notifier.
fn start_cursors(
    initial: i64,
    marked: Vec<MarkedRow>,
    record_marker: impl Fn(&str, i64),
) -> Cursors {
    for row in marked {
        record_marker(&row.text, row.date);
    }
    Cursors::starting_at(initial)
}

async fn poll(store: &HaroldStore) {
    poll_db(store, ChatDb::configured(), cursors(), is_messaging_paused).await;
}

/// One poll of `db` with the process-wide notifier record and recent rows.
async fn poll_db(store: &HaroldStore, db: ChatDb, cursors: &Cursors, is_paused: impl Fn() -> bool) {
    poll_chat_db(
        store,
        db,
        cursors,
        ownership::record_marker,
        &RECENT_ROWS,
        is_paused,
    )
    .await;
}

/// The listener's cursors, seeding the process-wide notifier record with `marked`.
fn start_listener_cursors(initial: i64, marked: Vec<MarkedRow>) -> Cursors {
    start_cursors(initial, marked, ownership::record_marker)
}

/// One poll of `db`. Marked rows are read before any reply is recorded, and while
/// messaging is paused too, so the latest notifier is current when a reply is routed. If
/// they cannot be read, no reply is recorded and no cursor moves, so the next poll reads
/// the same rows again. `is_paused` is asked after the rows are fetched.
async fn poll_chat_db(
    store: &HaroldStore,
    db: ChatDb,
    cursors: &Cursors,
    record_marker: impl Fn(&str, i64),
    recent: &Mutex<RecentRows>,
    is_paused: impl Fn() -> bool,
) {
    let inbound_rowid = cursors.inbound.load(Ordering::Relaxed);
    let self_rowid = cursors.self_sent.load(Ordering::Relaxed);
    let marked_rowid = cursors.marked.load(Ordering::Relaxed);

    let fetched = tokio::task::spawn_blocking(move || {
        // The replies are fetched first: a notification stored while these two queries
        // run is then among the marked rows of this poll.
        let inbound = db.replies_after(inbound_rowid, 0);
        let self_msgs = db.replies_after(self_rowid, 1);
        let marked = db.marked_after(marked_rowid);
        (inbound, self_msgs, marked)
    })
    .await;
    let (inbound, self_msgs, marked) = match fetched {
        Ok((inbound, self_msgs, Ok(marked))) => (inbound, self_msgs, marked),
        Ok((_, _, Err(cause))) => {
            warn!(%cause, "cannot read Harold's messages; replies wait for the next poll");
            return;
        }
        Err(error) => {
            warn!(%error, "fetch task panicked; replies wait for the next poll");
            return;
        }
    };
    let messaging_paused = is_paused();
    read_marked_rows(marked, &cursors.marked, record_marker);
    let now = Instant::now();

    for (rowid, text) in inbound {
        let trace_id = uuid::Uuid::new_v4().to_string();
        let span = info_span!("listener_inbound", trace_id = %trace_id, rowid = rowid);

        async {
            info!("iMessage received (inbound)");
            let row = FetchedRow {
                rowid,
                text,
                direction: Direction::Inbound,
            };
            record_row(store, row, &cursors.inbound, recent, messaging_paused, now).await;
        }
        .instrument(span)
        .await;
    }

    for (rowid, text) in self_msgs {
        let trace_id = uuid::Uuid::new_v4().to_string();
        let span = info_span!("listener_self", trace_id = %trace_id, rowid = rowid);

        async {
            info!("iMessage received (self)");
            let row = FetchedRow {
                rowid,
                text,
                direction: Direction::SelfSent,
            };
            record_row(
                store,
                row,
                &cursors.self_sent,
                recent,
                messaging_paused,
                now,
            )
            .await;
        }
        .instrument(span)
        .await;
    }
}

fn start_watcher(chat_db_path: &str) -> Option<(RecommendedWatcher, mpsc::UnboundedReceiver<()>)> {
    let parent = Path::new(chat_db_path).parent()?;
    let db_name = Path::new(chat_db_path).file_name()?.to_str()?.to_string();
    let wal_name = format!("{db_name}-wal");

    let (tx, rx) = mpsc::unbounded_channel();

    let watcher = notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
        let event = match res {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e, "fs watcher event error");
                return;
            }
        };
        if !matches!(event.kind, EventKind::Modify(_) | EventKind::Create(_)) {
            return;
        }
        let targets_chat_db = event.paths.iter().any(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n == db_name || n == wal_name)
        });
        if targets_chat_db {
            let _ = tx.send(());
        }
    });

    let mut watcher = match watcher {
        Ok(w) => w,
        Err(e) => {
            warn!(error = %e, "failed to create fs watcher, falling back to poll-only");
            return None;
        }
    };

    if let Err(e) = watcher.watch(parent, RecursiveMode::NonRecursive) {
        warn!(error = %e, "failed to watch chat.db directory, falling back to poll-only");
        return None;
    }

    info!(path = %parent.display(), "fs watcher active on chat.db directory");
    Some((watcher, rx))
}

pub(crate) async fn listen(store: Arc<HaroldStore>, mut shutdown: watch::Receiver<()>) {
    let db = ChatDb::configured();
    let Some((initial, marked)) = wait_for_start_row(
        || tokio::task::block_in_place(|| read_start(&db)),
        START_ROW_RETRY,
        &mut shutdown,
    )
    .await
    else {
        info!("iMessage listener shutting down before the Messages database was readable");
        return;
    };
    let cursors = start_listener_cursors(initial, marked);
    CURSORS.set(cursors).expect("listen called more than once");
    info!(initial_rowid = initial, "iMessage listener started");

    // Keep _watcher alive (dropping it stops watching). In the fallback path,
    // _keep_tx stays alive so fs_rx.recv() pends forever rather than returning None.
    let (_watcher, mut fs_rx, _keep_tx) = match start_watcher(&db_path()) {
        Some((watcher, rx)) => (Some(watcher), rx, None),
        None => {
            let (tx, rx) = mpsc::unbounded_channel();
            (None, rx, Some(tx))
        }
    };

    loop {
        tokio::select! {
            biased;

            _ = shutdown.changed() => {
                info!("iMessage listener shutting down");
                break;
            }
            _ = fs_rx.recv() => {
                while fs_rx.try_recv().is_ok() {}
                poll(&store).await;
            }
            // The 5s timer restarts after every fs-triggered poll, which is intentional:
            // if fs events are flowing, we don't need the fallback.
            () = tokio::time::sleep(tokio::time::Duration::from_secs(5)) => {
                poll(&store).await;
            }
        }
    }
}

#[cfg(test)]
#[path = "imessage_tests.rs"]
mod tests;
