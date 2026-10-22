mod error;
mod event_log;

pub use error::{FailedWrite, Result, StoreError};
#[cfg(test)]
pub(crate) use event_log::WriteFault;
pub use event_log::{AppendResult, EventEnvelope, EventLog, EventVersion, NewEvent};

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::time::Instant;
use tracing::{info, warn};
use turso::Database;

use crate::agent::domain::{
    AgentEvent, AgentIncarnation, AgentMonitorHealthChanged, AgentPaneObservation,
    AgentPaneProjection, AgentScreenObserved, AgentSnapshot, CompletionSummaryUpdate,
    EffectiveAgentState, MonitorHealthProjection, ObservedAgentState, ProjectionChange,
    WorkSummaryUpdate,
};
#[cfg(test)]
use crate::agent::reducer::DEFAULT_HOOK_GRACE_MS;
use crate::agent::reducer::{GraceWindows, reduce_agent_event, reduce_turn_completed};
use crate::agent::summary::{normalize_work_summary, sanitize_bounded_metadata};
use crate::alerts::AlertQueue;

const NAMESPACE: &str = "harold";
const PARTITION_KEY: &str = "main";
const STATE_DATABASE: &str = "harold-state.db";
const LOCK_FILE: &str = "harold.lock";
const MIGRATIONS: [(&str, &str); 3] = [
    (
        "001_initial",
        include_str!("store/migrations/001_initial.sql"),
    ),
    (
        "002_web_app",
        include_str!("store/migrations/002_web_app.sql"),
    ),
    (
        "003_app_photo",
        include_str!("store/migrations/003_app_photo.sql"),
    ),
];

/// The folder of the web app's photos, beside the store's folder.
const APP_PHOTOS_DIRECTORY: &str = "app-photos";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnCompleted {
    pub pane_id: String,
    pub pane_label: String,
    pub last_user_prompt: String,
    pub assistant_message: String,
    pub main_context: String,
    pub agent_incarnation: Option<AgentIncarnation>,
    pub work_summary: CompletionSummaryUpdate,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboundMessage {
    pub text: String,
}

/// A message an agent asked Harold to send to the user's away device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AwayMessageRequested {
    /// The source pane's id, such as `%47`.
    pub pane_id: String,
    /// The pane label when the request was accepted, such as `pods:0.1`.
    pub pane_label: String,
    /// The one-line text, 1 to 1,000 characters.
    pub text: String,
}

/// A line the chief asked Harold to say at the desk or send as a voice note.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpokenLineRequested {
    /// The source pane's id, such as `%47`.
    pub pane_id: String,
    /// The one-line text, 1 to 300 characters.
    pub text: String,
}

/// How one timed prompt ended on one local date.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum TimedPromptOutcome {
    /// The text and Enter were typed.
    Typed,
    /// The window ended and nothing was typed.
    Skipped,
    /// The text was typed and Enter failed.
    Failed,
}

/// What became of one timed prompt on one local date. The projection keeps the date, so
/// a prompt is not typed twice on one date across restarts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TimedPromptHandled {
    /// The entry's `name`.
    pub name: String,
    /// The local date, such as `2026-10-14`.
    pub day: String,
    pub outcome: TimedPromptOutcome,
    /// Why it was skipped; none for the other outcomes.
    pub reason: Option<String>,
    /// The pane typed into; none when skipped.
    pub pane_id: Option<String>,
}

/// The phone's push subscription, which the web app hands Harold when the user turns alerts on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AppPushSubscribed {
    /// The push service's address for this phone; one subscription per endpoint.
    pub endpoint: String,
    /// The phone's public key, as the browser gives it.
    pub p256dh: String,
    /// The phone's auth secret, as the browser gives it.
    pub auth: String,
}

/// The web app turned alerts off for one push subscription.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AppPushUnsubscribed {
    pub endpoint: String,
}

/// An InboundMessageReceived payload as the event handler reads it. It reads the
/// `to_chief` field that `InboundMessage` does not carry, so the channels' writers keep
/// their struct as it is; events without it read as false.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct ReceivedReply {
    pub text: String,
    /// True when the user typed the reply in the web app, for the chief.
    #[serde(default)]
    pub to_chief: bool,
    /// The file name, in `app-photos/`, of the photo the reply carries.
    #[serde(default)]
    pub photo: Option<String>,
}

/// Who wrote a row of the web app's thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AppSender {
    Chief,
    User,
}

impl AppSender {
    fn text(self) -> &'static str {
        match self {
            Self::Chief => "chief",
            Self::User => "user",
        }
    }

    fn parse(text: &str) -> Result<Self> {
        [Self::Chief, Self::User]
            .into_iter()
            .find(|sender| sender.text() == text)
            .ok_or_else(|| StoreError::Data(format!("invalid app message sender: {text}")))
    }
}

/// What became of a reply the user sent the chief from the web app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AppReplyState {
    Typed,
    WaitingChiefBusy,
    NotSentNoChief,
    NotSentPaused,
}

impl AppReplyState {
    const ALL: [Self; 4] = [
        Self::Typed,
        Self::WaitingChiefBusy,
        Self::NotSentNoChief,
        Self::NotSentPaused,
    ];

    /// The words the thread shows under the reply, which the store also keeps.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Typed => "typed",
            Self::WaitingChiefBusy => "waiting: chief busy",
            Self::NotSentNoChief => "not sent: no chief",
            Self::NotSentPaused => "not sent: messaging paused",
        }
    }

    fn parse(label: &str) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|state| state.label() == label)
            .ok_or_else(|| StoreError::Data(format!("invalid app reply state: {label}")))
    }
}

/// One row of the web app's thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AppMessage {
    /// The id of the event the row came from: a chief message's, or the reply's own.
    pub event_id: String,
    pub at_ms: i64,
    pub sender: AppSender,
    pub text: String,
    /// A reply's state; none for a chief message and for a reply not yet handled.
    pub state: Option<AppReplyState>,
    /// The photo a reply carries; none for a chief message and a reply without one.
    pub photo: Option<AppPhoto>,
}

/// The photo on a row of the thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AppPhoto {
    /// The upload's id: the file's name without its extension.
    pub id: String,
    /// False once the prune has deleted the file.
    pub kept: bool,
}

const TIMED_PROMPT_KEY_PREFIX: &str = "timed_prompt.";
const TIMED_PROMPT_KEY_SUFFIX: &str = ".last_day";

#[derive(Debug)]
pub struct PendingDelivery {
    pub event_id: String,
    pub event_version: EventVersion,
    pub event_type: String,
    pub payload: serde_json::Value,
    pub trace_id: String,
    /// The failed tries recorded for the row so far, kept across restarts.
    pub attempt_count: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProjectionBatch {
    pub applied: usize,
    pub through_event_version: EventVersion,
    pub snapshot_changed: bool,
}

/// Takes the store folder's lock, which the OS releases when this process ends. It is
/// taken before the log opens: opening the log a second time would fence the first Harold.
fn lock_store(root: &Path) -> Result<StoreLock> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(root.join(LOCK_FILE))?;
    match file.try_lock() {
        Ok(()) => Ok(StoreLock(file)),
        Err(std::fs::TryLockError::WouldBlock) => Err(StoreError::InUse),
        Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
    }
}

/// Refuses a store whose `harold-state.db` has read further than its log goes: its `log/`
/// was deleted, or replaced with an older log or another folder's. Harold would
/// otherwise wait for versions the log will not reach for a long time, and project and
/// deliver nothing meanwhile, without saying so.
async fn refuse_state_ahead_of_log(
    root: &Path,
    conn: &turso::Connection,
    stream: &EventLog,
) -> Result<()> {
    let cursor = last_processed_version_from(conn).await?;
    if cursor <= stream.version_bound() {
        return Ok(());
    }
    Err(StoreError::Data(format!(
        "{STATE_DATABASE} has read the event log up to version {cursor}, but the log in {} \
         ends before it; the store folder is inconsistent",
        root.join(event_log::LOG_DIRECTORY).display()
    )))
}

/// The store folder's lock, released when the store is dropped, or as soon as a later step
/// of opening the store fails. Closing the file alone does not always release it at once: a
/// child process another thread is starting holds a copy of the file until it runs its
/// program, and the lock lasts until every copy closes.
struct StoreLock(std::fs::File);

impl Drop for StoreLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// How long a store write may wait before the store is marked failing, and how long a
/// failing store waits between two tries.
pub(crate) const STORE_WRITE_LIMIT: Duration = Duration::from_secs(30);

/// Which store a write goes to, and so what marks the store failing: an event-log append
/// only by timing out, a delivery write to the state database by timing out or failing.
#[derive(Debug, Clone, Copy)]
enum WriteKind {
    Log,
    State,
}

impl WriteKind {
    fn reason(self) -> &'static str {
        match self {
            Self::Log => "write_timed_out",
            Self::State => "state_write_failed",
        }
    }
}

enum Health {
    Healthy,
    /// No write is let through until `next_try_at`; then one is, as a try. `trying` while
    /// that try runs. `alerted` when the chief was told, so the recovery is told too.
    Failing {
        next_try_at: Instant,
        trying: bool,
        alerted: bool,
    },
}

/// Whether a write was let through as normal or as a failing store's one try.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Admission {
    Normal,
    Try,
}

/// The store's health, which every gated write passes through. While the store is failing,
/// writes are refused at once, except one try 30 seconds after the last ended. The try's
/// result alone decides whether the store recovers.
struct StoreHealth {
    health: std::sync::Mutex<Health>,
    /// Where the chief is told, once `main` has handed the queue over.
    alerts: std::sync::OnceLock<AlertQueue>,
}

impl StoreHealth {
    fn new() -> Self {
        Self {
            health: std::sync::Mutex::new(Health::Healthy),
            alerts: std::sync::OnceLock::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Health> {
        self.health
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn is_failing(&self) -> bool {
        matches!(*self.lock(), Health::Failing { .. })
    }

    fn admit(&self) -> Result<Admission> {
        let mut health = self.lock();
        match &mut *health {
            Health::Healthy => Ok(Admission::Normal),
            Health::Failing {
                next_try_at,
                trying,
                ..
            } if !*trying && Instant::now() >= *next_try_at => {
                *trying = true;
                Ok(Admission::Try)
            }
            Health::Failing { .. } => Err(StoreError::Failing(FailedWrite::Refused)),
        }
    }

    /// Runs `write` if the store lets it through, for at most `STORE_WRITE_LIMIT`.
    async fn write<T>(&self, kind: WriteKind, write: impl Future<Output = Result<T>>) -> Result<T> {
        let admission = self.admit()?;
        let try_in_flight = (admission == Admission::Try).then(|| TryInFlight(Some(self)));
        let outcome = tokio::time::timeout(STORE_WRITE_LIMIT, write)
            .await
            .unwrap_or(Err(StoreError::Failing(FailedWrite::TimedOut)));
        let failed = match &outcome {
            Ok(_) => false,
            Err(StoreError::Failing(FailedWrite::TimedOut)) => true,
            Err(_) => matches!(kind, WriteKind::State),
        };
        match try_in_flight {
            Some(try_in_flight) => try_in_flight.end(outcome.is_ok()),
            None if failed => self.mark_failing(kind),
            None => {}
        }
        outcome
    }

    /// Marks a healthy store failing. A store already failing stays as it is.
    fn mark_failing(&self, kind: WriteKind) {
        // Read once, so the line queued and `alerted` agree if `main` hands the queue over
        // meanwhile.
        let alerts = self.alerts.get();
        {
            let mut health = self.lock();
            if matches!(*health, Health::Failing { .. }) {
                return;
            }
            *health = Health::Failing {
                next_try_at: Instant::now() + STORE_WRITE_LIMIT,
                trying: false,
                alerted: alerts.is_some(),
            };
        }
        warn!(
            parent: None,
            component = "store",
            reason = kind.reason(),
            "store failing"
        );
        if let Some(alerts) = alerts {
            alerts.firing(crate::alerts::STORE_FAILING);
        }
    }

    /// Ends the running try: the store recovers if it succeeded, or waits for the next.
    fn end_try(&self, succeeded: bool) {
        let alerted = {
            let mut health = self.lock();
            let alerted = matches!(*health, Health::Failing { alerted: true, .. });
            if !succeeded {
                *health = Health::Failing {
                    next_try_at: Instant::now() + STORE_WRITE_LIMIT,
                    trying: false,
                    alerted,
                };
                return;
            }
            *health = Health::Healthy;
            alerted
        };
        info!(
            parent: None,
            component = "store",
            reason = "try_succeeded",
            "store recovered"
        );
        if let Some(alerts) = self.alerts.get().filter(|_| alerted) {
            alerts.cleared(crate::alerts::STORE_FAILING);
        }
    }
}

/// A failing store's try while it runs. Dropped before it ends (its caller went away), it
/// ends the try as a failed one, so the store waits for the next try and logs nothing.
struct TryInFlight<'a>(Option<&'a StoreHealth>);

impl TryInFlight<'_> {
    fn end(mut self, succeeded: bool) {
        if let Some(health) = self.0.take() {
            health.end_try(succeeded);
        }
    }
}

impl Drop for TryInFlight<'_> {
    fn drop(&mut self) {
        if let Some(health) = self.0.take() {
            health.end_try(false);
        }
    }
}

pub struct HaroldStore {
    stream: EventLog,
    health: StoreHealth,
    state: Database,
    graces: GraceWindows,
    is_new: bool,
    /// The store's folder.
    root: PathBuf,
    #[cfg(test)]
    fail_projection_before_checkpoint: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    snapshot_read_gate: std::sync::Mutex<Option<SnapshotReadGate>>,
    #[cfg(test)]
    fail_next_monitor_append: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    fail_next_snapshot_load: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    state_write_fault: std::sync::Mutex<Option<WriteFault>>,
    /// Counts the changes to the web app's thread, so readers can wait for the next.
    app_thread: tokio::sync::watch::Sender<u64>,
    /// The store folder's lock. Declared last, so the log is dropped before it is released.
    _lock: StoreLock,
}

/// What a test reads of one `delivery_outbox` row.
#[cfg(test)]
pub(crate) struct OutboxRow {
    pub(crate) delivered_at_ms: Option<i64>,
    pub(crate) attempt_count: i64,
}

#[cfg(test)]
struct SnapshotReadGate {
    started: tokio::sync::oneshot::Sender<()>,
    resume: tokio::sync::oneshot::Receiver<()>,
}

#[cfg(test)]
pub(crate) struct SnapshotReadGateHandle {
    started: Option<tokio::sync::oneshot::Receiver<()>>,
    resume: Option<tokio::sync::oneshot::Sender<()>>,
}

#[cfg(test)]
impl SnapshotReadGateHandle {
    pub(crate) async fn wait_until_started(&mut self) {
        self.started
            .take()
            .expect("snapshot reader waits once")
            .await
            .expect("snapshot reader started");
    }

    pub(crate) fn resume(&mut self) {
        let _ = self
            .resume
            .take()
            .expect("snapshot reader resume once")
            .send(());
    }
}

impl HaroldStore {
    #[cfg(test)]
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_graces(path, GraceWindows::same(DEFAULT_HOOK_GRACE_MS)).await
    }

    pub(crate) async fn open_with_graces(
        path: impl AsRef<Path>,
        graces: GraceWindows,
    ) -> Result<Self> {
        let root = path.as_ref();
        tokio::fs::create_dir_all(root).await?;

        let lock = lock_store(root)?;
        let stream = EventLog::open(root).await?;

        let state_path = root.join(STATE_DATABASE);
        // A store is new when Harold creates its state database now.
        let is_new = !tokio::fs::try_exists(&state_path).await?;
        let state_path = state_path
            .to_str()
            .ok_or_else(|| StoreError::Data("Harold state path contains invalid UTF-8".into()))?;
        let state = turso::Builder::new_local(state_path).build().await?;
        let conn = state.connect()?;
        configure_state_database(&conn).await?;
        initialize_state_schema(&conn).await?;
        refuse_state_ahead_of_log(root, &conn, &stream).await?;

        Ok(Self {
            stream,
            health: StoreHealth::new(),
            state,
            graces,
            is_new,
            root: root.to_path_buf(),
            #[cfg(test)]
            fail_projection_before_checkpoint: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            snapshot_read_gate: std::sync::Mutex::new(None),
            #[cfg(test)]
            fail_next_monitor_append: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            fail_next_snapshot_load: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            state_write_fault: std::sync::Mutex::new(None),
            app_thread: tokio::sync::watch::Sender::new(0),
            _lock: lock,
        })
    }

    /// Where the web app keeps its photos: `app-photos/` beside the store's folder.
    pub(crate) fn app_photos_dir(&self) -> PathBuf {
        self.root
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .join(APP_PHOTOS_DIRECTORY)
    }

    /// Whether this open created the store's state database: a fresh start.
    pub(crate) fn is_new(&self) -> bool {
        self.is_new
    }

    pub(crate) fn stream(&self) -> &EventLog {
        &self.stream
    }

    /// Whether the store is marked failing: its writes are refused but for one try every
    /// 30 seconds.
    pub(crate) fn is_failing(&self) -> bool {
        self.health.is_failing()
    }

    /// Where the store tells the chief it is failing and has recovered. Set once, by `main`
    /// when the alert address is on; a later call is ignored.
    pub(crate) fn set_alert_queue(&self, queue: AlertQueue) {
        let _ = self.health.alerts.set(queue);
    }

    /// Appends `batch` to the event log through the store's health check. An empty batch
    /// writes nothing, so it skips the check: it is neither refused nor a try.
    async fn append(&self, batch: impl IntoIterator<Item = NewEvent>) -> Result<AppendResult> {
        let batch: Vec<NewEvent> = batch.into_iter().collect();
        if batch.is_empty() {
            return self.stream.append(batch).await;
        }
        self.health
            .write(WriteKind::Log, self.stream.append(batch))
            .await
    }

    /// Makes every later event-log flush hang or fail, until set to `None`.
    #[cfg(test)]
    pub(crate) fn fault_log_flush_for_test(&self, fault: Option<WriteFault>) {
        self.stream.fault_flush_for_test(fault);
    }

    pub(crate) async fn last_processed_version(&self) -> Result<EventVersion> {
        let conn = self.state.connect()?;
        last_processed_version_from(&conn).await
    }

    pub(crate) async fn project_unhandled_events(&self, limit: usize) -> Result<ProjectionBatch> {
        let cursor = self.last_processed_version().await?;
        let events = self.stream.load_after_version(cursor, limit).await?;
        let Some(last) = events.last() else {
            return Ok(ProjectionBatch {
                applied: 0,
                through_event_version: cursor,
                snapshot_changed: false,
            });
        };

        let conn = self.state.connect()?;
        conn.execute("BEGIN IMMEDIATE", ()).await?;
        let result = async {
            let mut snapshot_changed = false;
            let mut thread_changed = false;
            for event in &events {
                match event.r#type.as_str() {
                    "TurnCompleted" => {
                        stage_delivery_event(&conn, event).await?;
                        snapshot_changed |=
                            project_turn_completed(&conn, event, self.graces).await?;
                    }
                    "InboundMessageReceived" => {
                        stage_delivery_event(&conn, event).await?;
                        thread_changed |= project_app_reply(&conn, event).await?;
                    }
                    "AwayMessageRequested" | "SpokenLineRequested" => {
                        stage_delivery_event(&conn, event).await?;
                    }
                    "AppPushSubscribed" => project_app_push_subscribed(&conn, event).await?,
                    "AppPushUnsubscribed" => project_app_push_unsubscribed(&conn, event).await?,
                    "AgentPaneObserved"
                    | "AgentPaneDeparted"
                    | "AgentLifecycleObserved"
                    | "AgentScreenObserved"
                    | "AgentActivitySummaryGenerated" => {
                        snapshot_changed |= project_agent_event(&conn, event, self.graces).await?;
                    }
                    "AgentMonitorHealthChanged" => {
                        let health = serde_json::from_value::<AgentMonitorHealthChanged>(
                            event.payload.clone(),
                        )?;
                        upsert_monitor_health(&conn, &health, event.version).await?;
                        snapshot_changed = true;
                    }
                    "TtsMuteChanged" => {
                        let muted = event
                            .payload
                            .get("muted")
                            .and_then(serde_json::Value::as_bool)
                            .ok_or_else(|| {
                                StoreError::Data("TtsMuteChanged payload missing muted".into())
                            })?;
                        upsert_bool_setting(&conn, "tts_muted", muted, event.version).await?;
                        snapshot_changed = true;
                    }
                    "MessagingPausedChanged" => {
                        let paused = event
                            .payload
                            .get("paused")
                            .and_then(serde_json::Value::as_bool)
                            .ok_or_else(|| {
                                StoreError::Data(
                                    "MessagingPausedChanged payload missing paused".into(),
                                )
                            })?;
                        upsert_bool_setting(&conn, "messaging_paused", paused, event.version)
                            .await?;
                        snapshot_changed = true;
                    }
                    "TimedPromptHandled" => {
                        // Only Harold writes this event. A payload it cannot read is passed
                        // over, so it cannot block the events after it.
                        if let Ok(handled) =
                            serde_json::from_value::<TimedPromptHandled>(event.payload.clone())
                        {
                            let key = format!(
                                "{TIMED_PROMPT_KEY_PREFIX}{}{TIMED_PROMPT_KEY_SUFFIX}",
                                handled.name
                            );
                            upsert_text_setting(&conn, &key, &handled.day, event.version).await?;
                        }
                    }
                    _ => {
                        // Unknown stream facts stay visible to the existing permanent-delivery
                        // path; advancing without an outbox record would silently lose them.
                        stage_delivery_event(&conn, event).await?;
                    }
                }
            }

            #[cfg(test)]
            if self
                .fail_projection_before_checkpoint
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                return Err(StoreError::Data(
                    "test projection failure before checkpoint".into(),
                ));
            }

            let now_ms = now_ms();
            conn.execute(
                r#"
                INSERT INTO last_processed_event
                    (namespace, partition_key, last_processed_event_version, updated_at_ms)
                VALUES (?1, ?2, ?3, ?4)
                ON CONFLICT(namespace, partition_key) DO UPDATE SET
                    last_processed_event_version = excluded.last_processed_event_version,
                    updated_at_ms = excluded.updated_at_ms
                "#,
                (NAMESPACE, PARTITION_KEY, last.version.to_column(), now_ms),
            )
            .await?;
            conn.execute("COMMIT", ()).await?;
            Ok((snapshot_changed, thread_changed))
        }
        .await;
        let (snapshot_changed, thread_changed) = match result {
            Ok(changed) => changed,
            Err(error) => {
                let _ = conn.execute("ROLLBACK", ()).await;
                return Err(error);
            }
        };
        if thread_changed {
            self.app_thread_changed();
        }

        Ok(ProjectionBatch {
            applied: events.len(),
            through_event_version: last.version,
            snapshot_changed,
        })
    }

    pub(crate) async fn load_agent_snapshot(&self) -> Result<AgentSnapshot> {
        #[cfg(test)]
        if self
            .fail_next_snapshot_load
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(StoreError::Data("test snapshot load failure".into()));
        }
        let conn = self.state.connect()?;
        let mut snapshot = load_agent_snapshot_from_one_query(&conn).await?;
        snapshot.tts_muted = load_bool_setting(&conn, "tts_muted").await?;
        snapshot.messaging_paused = load_bool_setting(&conn, "messaging_paused").await?;
        #[cfg(test)]
        pause_snapshot_read_after_query(&self.snapshot_read_gate).await;
        Ok(snapshot)
    }

    /// The local date each timed prompt was last handled on, by prompt name, as far as
    /// the projection has read the stream.
    pub(crate) async fn timed_prompt_last_days(
        &self,
    ) -> Result<std::collections::HashMap<String, String>> {
        let conn = self.state.connect()?;
        let mut rows = conn.query("SELECT key, value FROM settings", ()).await?;
        let mut days = std::collections::HashMap::new();
        while let Some(row) = rows.next().await? {
            let key = required_text(&row, 0)?;
            let Some(name) = key
                .strip_prefix(TIMED_PROMPT_KEY_PREFIX)
                .and_then(|rest| rest.strip_suffix(TIMED_PROMPT_KEY_SUFFIX))
            else {
                continue;
            };
            days.insert(name.to_string(), required_text(&row, 1)?);
        }
        Ok(days)
    }

    #[cfg(test)]
    pub(crate) fn fail_projection_before_checkpoint_for_test(&self) {
        self.fail_projection_before_checkpoint
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// The outbox row for `event_id`, if there is one.
    #[cfg(test)]
    pub(crate) async fn outbox_row_for_test(&self, event_id: &str) -> Result<Option<OutboxRow>> {
        let conn = self.state.connect()?;
        let mut rows = conn
            .query(
                "SELECT delivered_at_ms, attempt_count FROM delivery_outbox WHERE event_id = ?1",
                (event_id,),
            )
            .await?;
        let Some(row) = rows.next().await? else {
            return Ok(None);
        };
        Ok(Some(OutboxRow {
            delivered_at_ms: row.get_value(0)?.as_integer().copied(),
            attempt_count: row
                .get_value(1)?
                .as_integer()
                .copied()
                .ok_or_else(|| StoreError::Data("outbox attempt_count is not an integer".into()))?,
        }))
    }

    #[cfg(test)]
    pub(crate) fn fail_next_monitor_append_for_test(&self) {
        self.fail_next_monitor_append
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    #[cfg(test)]
    pub(crate) fn fail_next_snapshot_load_for_test(&self) {
        self.fail_next_snapshot_load
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    #[cfg(test)]
    pub(crate) fn pause_snapshot_read_after_query_for_test(&self) -> SnapshotReadGateHandle {
        let (started, started_receiver) = tokio::sync::oneshot::channel();
        let (resume_sender, resume) = tokio::sync::oneshot::channel();
        *self
            .snapshot_read_gate
            .lock()
            .expect("snapshot read gate lock") = Some(SnapshotReadGate { started, resume });
        SnapshotReadGateHandle {
            started: Some(started_receiver),
            resume: Some(resume_sender),
        }
    }

    pub async fn next_pending_delivery(&self) -> Result<Option<PendingDelivery>> {
        let conn = self.state.connect()?;
        let mut rows = conn
            .query(
                r#"
                SELECT event_id, event_version, event_type, payload, trace_id, attempt_count
                FROM delivery_outbox
                WHERE delivered_at_ms IS NULL
                ORDER BY event_version
                LIMIT 1
                "#,
                (),
            )
            .await?;
        let Some(row) = rows.next().await? else {
            return Ok(None);
        };

        let text = |index| -> Result<String> {
            row.get_value(index)?
                .as_text()
                .map(ToString::to_string)
                .ok_or_else(|| StoreError::Data(format!("outbox column {index} is not text")))
        };
        let version = row
            .get_value(1)?
            .as_integer()
            .copied()
            .ok_or_else(|| StoreError::Data("outbox version is not an integer".into()))?;
        let attempt_count = row
            .get_value(5)?
            .as_integer()
            .copied()
            .ok_or_else(|| StoreError::Data("outbox attempt_count is not an integer".into()))?;
        let payload = serde_json::from_str(&text(3)?)?;
        Ok(Some(PendingDelivery {
            event_id: text(0)?,
            event_version: EventVersion::from_column(version)?,
            event_type: text(2)?,
            payload,
            trace_id: text(4)?,
            attempt_count,
        }))
    }

    pub async fn mark_delivered(&self, event_id: &str) -> Result<()> {
        let now_ms = (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64;
        self.delivery_write(
            r#"
            UPDATE delivery_outbox
            SET delivered_at_ms = ?1, last_error = NULL
            WHERE event_id = ?2
            "#,
            (now_ms, event_id),
        )
        .await
    }

    pub async fn record_delivery_failure(&self, event_id: &str, error: &str) -> Result<()> {
        self.delivery_write(
            r#"
            UPDATE delivery_outbox
            SET attempt_count = attempt_count + 1, last_error = ?1
            WHERE event_id = ?2
            "#,
            (error, event_id),
        )
        .await
    }

    pub async fn mark_undeliverable(&self, event_id: &str, error: &str) -> Result<()> {
        let now_ms = (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64;
        self.delivery_write(
            r#"
            UPDATE delivery_outbox
            SET attempt_count = attempt_count + 1,
                last_error = ?1,
                delivered_at_ms = ?2
            WHERE event_id = ?3
            "#,
            (error, now_ms, event_id),
        )
        .await
    }

    /// One of the delivery loop's writes to the state database, through the store's health
    /// check: one that fails or takes over 30 seconds marks the store failing.
    async fn delivery_write(&self, sql: &str, params: impl turso::IntoParams) -> Result<()> {
        self.health
            .write(WriteKind::State, async {
                let conn = self.state.connect()?;
                #[cfg(test)]
                {
                    let fault = *self.state_write_fault.lock().expect("state fault lock");
                    event_log::apply_fault_for_test(fault).await?;
                }
                conn.execute(sql, params).await?;
                Ok(())
            })
            .await
    }

    /// The web app's thread from `since_ms` on, oldest first; rows with the same time come
    /// in the order they were added.
    pub(crate) async fn app_messages_since(&self, since_ms: i64) -> Result<Vec<AppMessage>> {
        let conn = self.state.connect()?;
        let mut rows = conn
            .query(
                r#"
                SELECT m.event_id, m.at_ms, m.sender, m.text, m.state, m.photo, u.id
                FROM app_messages m
                LEFT JOIN app_photo_uploads u ON u.file = m.photo
                WHERE m.at_ms >= ?1
                ORDER BY m.at_ms, m.rowid
                "#,
                (since_ms,),
            )
            .await?;
        let mut messages = Vec::new();
        while let Some(row) = rows.next().await? {
            let kept = optional_text(&row, 6)?.is_some();
            messages.push(AppMessage {
                event_id: required_text(&row, 0)?,
                at_ms: required_integer(&row, 1)?,
                sender: AppSender::parse(&required_text(&row, 2)?)?,
                text: required_text(&row, 3)?,
                state: optional_text(&row, 4)?
                    .map(|label| AppReplyState::parse(&label))
                    .transpose()?,
                photo: optional_text(&row, 5)?.map(|file| AppPhoto {
                    id: file
                        .rsplit_once('.')
                        .map_or(file.as_str(), |(stem, _)| stem)
                        .to_owned(),
                    kept,
                }),
            });
        }
        Ok(messages)
    }

    /// Adds a chief message to the web app's thread. A second call for the same event adds
    /// nothing.
    pub(crate) async fn add_chief_app_message(
        &self,
        event_id: &str,
        at_ms: i64,
        text: &str,
    ) -> Result<()> {
        self.app_thread_write(
            r#"
            INSERT INTO app_messages (event_id, at_ms, sender, text)
            VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT(event_id) DO NOTHING
            "#,
            (event_id, at_ms, AppSender::Chief.text(), text),
        )
        .await
    }

    /// Shows what became of the reply that `event_id` stored.
    pub(crate) async fn set_app_reply_state(
        &self,
        event_id: &str,
        state: AppReplyState,
    ) -> Result<()> {
        self.app_thread_write(
            "UPDATE app_messages SET state = ?1 WHERE event_id = ?2",
            (state.label(), event_id),
        )
        .await
    }

    /// Moves every reply whose row says `waiting: chief busy` and whose time is at or
    /// before `before_ms` to `typed`, as the chief's turn end has come since. The ids of
    /// the rows it changed.
    pub(crate) async fn settle_waiting_app_replies(&self, before_ms: i64) -> Result<Vec<String>> {
        let conn = self.state.connect()?;
        let mut rows = conn
            .query(
                "SELECT event_id FROM app_messages WHERE state = ?1 AND at_ms <= ?2 ORDER BY at_ms, rowid",
                (AppReplyState::WaitingChiefBusy.label(), before_ms),
            )
            .await?;
        let mut settled = Vec::new();
        while let Some(row) = rows.next().await? {
            settled.push(required_text(&row, 0)?);
        }
        if settled.is_empty() {
            return Ok(settled);
        }
        self.app_thread_write(
            "UPDATE app_messages SET state = ?1 WHERE state = ?2 AND at_ms <= ?3",
            (
                AppReplyState::Typed.label(),
                AppReplyState::WaitingChiefBusy.label(),
                before_ms,
            ),
        )
        .await?;
        Ok(settled)
    }

    /// Deletes the web app's thread rows older than `before_ms`.
    pub(crate) async fn delete_app_messages_before(&self, before_ms: i64) -> Result<()> {
        self.app_thread_write("DELETE FROM app_messages WHERE at_ms < ?1", (before_ms,))
            .await
    }

    /// Records a photo file written to `app-photos/`: `id` is its name without the
    /// extension, `file` its name.
    pub(crate) async fn add_app_photo_upload(
        &self,
        id: &str,
        file: &str,
        at_ms: i64,
    ) -> Result<()> {
        self.delivery_write(
            "INSERT INTO app_photo_uploads (id, file, at_ms, used) VALUES (?1, ?2, ?3, 0)",
            (id, file, at_ms),
        )
        .await
    }

    /// Takes the upload `id` for a reply: its file name when the upload exists and no
    /// reply has taken it, none otherwise. Two replies naming one upload cannot both
    /// take it.
    pub(crate) async fn claim_app_photo_upload(&self, id: &str) -> Result<Option<String>> {
        let conn = self.state.connect()?;
        let mut rows = conn
            .query(
                "SELECT file FROM app_photo_uploads WHERE id = ?1 AND used = 0",
                (id,),
            )
            .await?;
        let file = match rows.next().await? {
            Some(row) => required_text(&row, 0)?,
            None => return Ok(None),
        };
        drop(rows);
        let claimed = self
            .health
            .write(WriteKind::State, async {
                Ok(conn
                    .execute(
                        "UPDATE app_photo_uploads SET used = 1 WHERE id = ?1 AND used = 0",
                        (id,),
                    )
                    .await?)
            })
            .await?;
        Ok((claimed > 0).then_some(file))
    }

    /// Gives an upload taken by `claim_app_photo_upload` back, as its reply was not stored.
    pub(crate) async fn release_app_photo_upload(&self, id: &str) -> Result<()> {
        self.delivery_write("UPDATE app_photo_uploads SET used = 0 WHERE id = ?1", (id,))
            .await
    }

    /// The file name of upload `id`, when its file is kept.
    pub(crate) async fn app_photo_file(&self, id: &str) -> Result<Option<String>> {
        let conn = self.state.connect()?;
        let mut rows = conn
            .query("SELECT file FROM app_photo_uploads WHERE id = ?1", (id,))
            .await?;
        rows.next()
            .await?
            .map(|row| required_text(&row, 0))
            .transpose()
    }

    /// The files of photos on rows older than `before_ms`, which the thread's prune is
    /// about to delete.
    pub(crate) async fn expired_app_photo_files(&self, before_ms: i64) -> Result<Vec<String>> {
        self.photo_files(
            "SELECT photo FROM app_messages WHERE photo IS NOT NULL AND at_ms < ?1",
            before_ms,
        )
        .await
    }

    /// The files of uploads no reply has taken, recorded before `before_ms`.
    pub(crate) async fn unused_app_photo_files(&self, before_ms: i64) -> Result<Vec<String>> {
        self.photo_files(
            "SELECT file FROM app_photo_uploads WHERE used = 0 AND at_ms < ?1",
            before_ms,
        )
        .await
    }

    async fn photo_files(&self, sql: &str, before_ms: i64) -> Result<Vec<String>> {
        let conn = self.state.connect()?;
        let mut rows = conn.query(sql, (before_ms,)).await?;
        let mut files = Vec::new();
        while let Some(row) = rows.next().await? {
            files.push(required_text(&row, 0)?);
        }
        Ok(files)
    }

    /// Adds a reply row as if projected at `at_ms`, with `photo` its file name.
    #[cfg(test)]
    pub(crate) async fn add_user_app_row_for_test(
        &self,
        event_id: &str,
        at_ms: i64,
        text: &str,
        photo: Option<&str>,
    ) -> Result<()> {
        self.app_thread_write(
            "INSERT INTO app_messages (event_id, at_ms, sender, text, photo) VALUES (?1, ?2, 'user', ?3, ?4)",
            (event_id, at_ms, text, photo),
        )
        .await
    }

    /// Forgets the upload of `file`, as its file was deleted.
    pub(crate) async fn delete_app_photo_upload(&self, file: &str) -> Result<()> {
        self.delivery_write("DELETE FROM app_photo_uploads WHERE file = ?1", (file,))
            .await
    }

    /// The push subscriptions the web app holds, by endpoint.
    pub(crate) async fn app_push_subscriptions(&self) -> Result<Vec<AppPushSubscribed>> {
        let conn = self.state.connect()?;
        let mut rows = conn
            .query(
                "SELECT endpoint, p256dh, auth FROM app_push_subscriptions ORDER BY endpoint",
                (),
            )
            .await?;
        let mut subscriptions = Vec::new();
        while let Some(row) = rows.next().await? {
            subscriptions.push(AppPushSubscribed {
                endpoint: required_text(&row, 0)?,
                p256dh: required_text(&row, 1)?,
                auth: required_text(&row, 2)?,
            });
        }
        Ok(subscriptions)
    }

    /// Changes whenever a thread row is added, relabelled or deleted.
    pub(crate) fn watch_app_thread(&self) -> tokio::sync::watch::Receiver<u64> {
        self.app_thread.subscribe()
    }

    /// A write to the web app's thread, through the store's health check like a delivery
    /// write; the thread's watchers are told once it succeeds.
    async fn app_thread_write(&self, sql: &str, params: impl turso::IntoParams) -> Result<()> {
        self.delivery_write(sql, params).await?;
        self.app_thread_changed();
        Ok(())
    }

    fn app_thread_changed(&self) {
        self.app_thread
            .send_modify(|version| *version = version.wrapping_add(1));
    }

    /// Makes every later delivery write to the state database hang or fail, until set to
    /// `None`.
    #[cfg(test)]
    pub(crate) fn fault_state_writes_for_test(&self, fault: Option<WriteFault>) {
        *self.state_write_fault.lock().expect("state fault lock") = fault;
    }
}

const AGENT_PANE_COLUMNS: &str = r#"
    pane_id, pane_pid, agent_pid, agent_started_at_ms, provider_id,
    tmux_target, session_name, window_index, pane_index, working_directory,
    provider_display_name, pane_observed_at_ms, hook_state, hook_observed_at_ms,
    screen_state, screen_classifier_id, screen_observed_at_ms, effective_state,
    explicit_work_summary, explicit_work_summary_updated_at_ms,
    screen_work_summary, screen_work_summary_updated_at_ms, work_summary,
    last_transition_at_ms, last_event_version,
    summary_basis_version, generated_work_summary, generated_summary_basis_version,
    lifecycle_adapter_id
"#;

async fn last_processed_version_from(conn: &turso::Connection) -> Result<EventVersion> {
    let mut rows = conn
        .query(
            r#"
            SELECT last_processed_event_version
            FROM last_processed_event
            WHERE namespace = ?1 AND partition_key = ?2
            "#,
            (NAMESPACE, PARTITION_KEY),
        )
        .await?;
    let Some(row) = rows.next().await? else {
        return Ok(EventVersion::start());
    };
    let version = row
        .get_value(0)?
        .as_integer()
        .copied()
        .ok_or_else(|| StoreError::Data("checkpoint version is not an integer".into()))?;
    EventVersion::from_column(version)
}

async fn stage_delivery_event(conn: &turso::Connection, event: &EventEnvelope) -> Result<()> {
    let payload = serde_json::to_string(&event.payload)?;
    conn.execute(
        r#"
        INSERT INTO delivery_outbox
            (event_id, event_version, event_type, payload, trace_id)
        VALUES (?1, ?2, ?3, ?4, ?5)
        ON CONFLICT(event_id) DO NOTHING
        "#,
        (
            event.id.to_string(),
            event.version.to_column(),
            event.r#type.as_str(),
            payload,
            event.id.to_string(),
        ),
    )
    .await?;
    Ok(())
}

async fn project_agent_event(
    conn: &turso::Connection,
    event: &EventEnvelope,
    graces: GraceWindows,
) -> Result<bool> {
    let agent_event = match event.r#type.as_str() {
        "AgentPaneObserved" => {
            AgentEvent::PaneObserved(serde_json::from_value(event.payload.clone())?)
        }
        "AgentPaneDeparted" => {
            AgentEvent::PaneDeparted(serde_json::from_value(event.payload.clone())?)
        }
        "AgentLifecycleObserved" => {
            AgentEvent::LifecycleObserved(serde_json::from_value(event.payload.clone())?)
        }
        "AgentScreenObserved" => {
            AgentEvent::ScreenObserved(serde_json::from_value(event.payload.clone())?)
        }
        "AgentActivitySummaryGenerated" => {
            AgentEvent::ActivitySummaryGenerated(serde_json::from_value(event.payload.clone())?)
        }
        _ => {
            return Err(StoreError::Data(format!(
                "unsupported agent projection event type: {}",
                event.r#type
            )));
        }
    };
    let pane_id = agent_event_pane_id(&agent_event);
    let current = load_agent_pane(conn, pane_id).await?;
    match reduce_agent_event(current, &agent_event, event.version, graces) {
        ProjectionChange::Upsert(projection) => {
            upsert_agent_pane(conn, &projection).await?;
            Ok(true)
        }
        ProjectionChange::Remove(incarnation) => {
            let removed = conn
                .execute(
                    "DELETE FROM agent_panes WHERE pane_id = ?1",
                    (incarnation.pane_id.as_str(),),
                )
                .await?;
            Ok(removed > 0)
        }
        ProjectionChange::Ignore => Ok(false),
    }
}

async fn project_turn_completed(
    conn: &turso::Connection,
    event: &EventEnvelope,
    graces: GraceWindows,
) -> Result<bool> {
    let Ok(turn) = serde_json::from_value::<TurnCompleted>(event.payload.clone()) else {
        // The delivery outbox remains the compatibility boundary for malformed notification
        // payloads. Projection must not let one poison delivery block later durable events.
        return Ok(false);
    };
    let Some(incarnation) = turn.agent_incarnation else {
        return Ok(false);
    };
    let work_summary = match turn.work_summary {
        CompletionSummaryUpdate::Unchanged => WorkSummaryUpdate::Unchanged,
        CompletionSummaryUpdate::Set(summary) => WorkSummaryUpdate::Set(summary),
    };
    let current = load_agent_pane(conn, &incarnation.pane_id).await?;
    let observed_at_ms = current
        .as_ref()
        .filter(|projection| projection.pane.incarnation == incarnation)
        .map_or_else(
            || {
                (event.created_at.unix_timestamp_nanos() / 1_000_000)
                    .try_into()
                    .map_err(|_| {
                        StoreError::Data(
                            "TurnCompleted timestamp is outside i64 milliseconds".into(),
                        )
                    })
            },
            |projection| Ok(projection.pane.observed_at_ms),
        )?;
    match reduce_turn_completed(
        current,
        &incarnation,
        &work_summary,
        observed_at_ms,
        event.version,
        graces,
    ) {
        ProjectionChange::Upsert(projection) => {
            upsert_agent_pane(conn, &projection).await?;
            Ok(true)
        }
        ProjectionChange::Remove(_) | ProjectionChange::Ignore => Ok(false),
    }
}

/// Adds the user's row to the web app's thread for a reply typed there for the chief.
/// Whether a row was added. A payload it cannot read adds none; its delivery is staged anyway.
async fn project_app_reply(conn: &turso::Connection, event: &EventEnvelope) -> Result<bool> {
    let Ok(reply) = serde_json::from_value::<ReceivedReply>(event.payload.clone()) else {
        return Ok(false);
    };
    if !reply.to_chief {
        return Ok(false);
    }
    let at_ms: i64 = (event.created_at.unix_timestamp_nanos() / 1_000_000)
        .try_into()
        .map_err(|_| {
            StoreError::Data("InboundMessageReceived timestamp is outside i64 milliseconds".into())
        })?;
    let added = conn
        .execute(
            r#"
            INSERT INTO app_messages (event_id, at_ms, sender, text, photo)
            VALUES (?1, ?2, ?3, ?4, ?5)
            ON CONFLICT(event_id) DO NOTHING
            "#,
            (
                event.id.to_string(),
                at_ms,
                AppSender::User.text(),
                reply.text,
                reply.photo,
            ),
        )
        .await?;
    Ok(added > 0)
}

/// Keeps the subscription, replacing the keys an endpoint had. Only Harold writes this event,
/// so a payload it cannot read is passed over rather than block the events after it.
async fn project_app_push_subscribed(
    conn: &turso::Connection,
    event: &EventEnvelope,
) -> Result<()> {
    let Ok(subscribed) = serde_json::from_value::<AppPushSubscribed>(event.payload.clone()) else {
        return Ok(());
    };
    let subscribed_at_ms: i64 = (event.created_at.unix_timestamp_nanos() / 1_000_000)
        .try_into()
        .map_err(|_| {
            StoreError::Data("AppPushSubscribed timestamp is outside i64 milliseconds".into())
        })?;
    conn.execute(
        r#"
        INSERT INTO app_push_subscriptions
            (endpoint, p256dh, auth, subscribed_at_ms, last_event_version)
        VALUES (?1, ?2, ?3, ?4, ?5)
        ON CONFLICT(endpoint) DO UPDATE SET
            p256dh = excluded.p256dh,
            auth = excluded.auth,
            subscribed_at_ms = excluded.subscribed_at_ms,
            last_event_version = excluded.last_event_version
        "#,
        (
            subscribed.endpoint,
            subscribed.p256dh,
            subscribed.auth,
            subscribed_at_ms,
            event.version.to_column(),
        ),
    )
    .await?;
    Ok(())
}

/// Forgets the endpoint's subscription. A payload it cannot read is passed over, as above.
async fn project_app_push_unsubscribed(
    conn: &turso::Connection,
    event: &EventEnvelope,
) -> Result<()> {
    let Ok(unsubscribed) = serde_json::from_value::<AppPushUnsubscribed>(event.payload.clone())
    else {
        return Ok(());
    };
    conn.execute(
        "DELETE FROM app_push_subscriptions WHERE endpoint = ?1",
        (unsubscribed.endpoint,),
    )
    .await?;
    Ok(())
}

fn agent_event_pane_id(event: &AgentEvent) -> &str {
    match event {
        AgentEvent::PaneObserved(event) => &event.pane.incarnation.pane_id,
        AgentEvent::PaneDeparted(event) => &event.incarnation.pane_id,
        AgentEvent::LifecycleObserved(event) => &event.incarnation.pane_id,
        AgentEvent::ScreenObserved(event) => &event.incarnation.pane_id,
        AgentEvent::ActivitySummaryGenerated(event) => &event.incarnation.pane_id,
        AgentEvent::MonitorHealthChanged(_) => unreachable!("health events are not pane events"),
    }
}

async fn upsert_monitor_health(
    conn: &turso::Connection,
    health: &AgentMonitorHealthChanged,
    event_version: EventVersion,
) -> Result<()> {
    conn.execute(
        r#"
        INSERT INTO agent_monitor_health
            (component, healthy, reason_code, observed_at_ms, last_event_version)
        VALUES (?1, ?2, ?3, ?4, ?5)
        ON CONFLICT(component) DO UPDATE SET
            healthy = excluded.healthy,
            reason_code = excluded.reason_code,
            observed_at_ms = excluded.observed_at_ms,
            last_event_version = excluded.last_event_version
        "#,
        (
            health.component.as_str(),
            i64::from(health.healthy),
            health.reason_code.as_str(),
            health.observed_at_ms,
            event_version.to_column(),
        ),
    )
    .await?;
    Ok(())
}

async fn upsert_agent_pane(
    conn: &turso::Connection,
    projection: &AgentPaneProjection,
) -> Result<()> {
    let pane = &projection.pane;
    conn.execute(
        r#"
        INSERT INTO agent_panes (
            pane_id, pane_pid, agent_pid, agent_started_at_ms, provider_id,
            tmux_target, session_name, window_index, pane_index, working_directory,
            provider_display_name, pane_observed_at_ms, hook_state, hook_observed_at_ms,
            screen_state, screen_classifier_id, screen_observed_at_ms, effective_state,
            explicit_work_summary, explicit_work_summary_updated_at_ms,
            screen_work_summary, screen_work_summary_updated_at_ms, work_summary,
            last_transition_at_ms, last_event_version,
            summary_basis_version, generated_work_summary, generated_summary_basis_version,
            lifecycle_adapter_id
        ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
            ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29
        ) ON CONFLICT(pane_id) DO UPDATE SET
            pane_pid = excluded.pane_pid,
            agent_pid = excluded.agent_pid,
            agent_started_at_ms = excluded.agent_started_at_ms,
            provider_id = excluded.provider_id,
            tmux_target = excluded.tmux_target,
            session_name = excluded.session_name,
            window_index = excluded.window_index,
            pane_index = excluded.pane_index,
            working_directory = excluded.working_directory,
            provider_display_name = excluded.provider_display_name,
            pane_observed_at_ms = excluded.pane_observed_at_ms,
            hook_state = excluded.hook_state,
            hook_observed_at_ms = excluded.hook_observed_at_ms,
            screen_state = excluded.screen_state,
            screen_classifier_id = excluded.screen_classifier_id,
            screen_observed_at_ms = excluded.screen_observed_at_ms,
            effective_state = excluded.effective_state,
            explicit_work_summary = excluded.explicit_work_summary,
            explicit_work_summary_updated_at_ms = excluded.explicit_work_summary_updated_at_ms,
            screen_work_summary = excluded.screen_work_summary,
            screen_work_summary_updated_at_ms = excluded.screen_work_summary_updated_at_ms,
            work_summary = excluded.work_summary,
            last_transition_at_ms = excluded.last_transition_at_ms,
            last_event_version = excluded.last_event_version,
            summary_basis_version = excluded.summary_basis_version,
            generated_work_summary = excluded.generated_work_summary,
            generated_summary_basis_version = excluded.generated_summary_basis_version,
            lifecycle_adapter_id = excluded.lifecycle_adapter_id
        "#,
        turso::params![
            pane.incarnation.pane_id.as_str(),
            i64::from(pane.incarnation.pane_pid),
            i64::from(pane.incarnation.agent_pid),
            pane.incarnation.agent_started_at_ms,
            pane.incarnation.provider_id.as_str(),
            pane.tmux_target.as_str(),
            pane.session_name.as_str(),
            i64::from(pane.window_index),
            i64::from(pane.pane_index),
            pane.working_directory.as_str(),
            pane.provider_display_name.as_str(),
            pane.observed_at_ms,
            projection.hook_state.map(observed_state_text),
            projection.hook_observed_at_ms,
            projection.screen_state.map(observed_state_text),
            projection.screen_classifier_id.as_deref(),
            projection.screen_observed_at_ms,
            effective_state_text(projection.effective_state),
            projection.explicit_work_summary.as_deref(),
            projection.explicit_work_summary_updated_at_ms,
            projection.screen_work_summary.as_deref(),
            projection.screen_work_summary_updated_at_ms,
            projection.work_summary.as_deref(),
            projection.last_transition_at_ms,
            projection.last_event_version.to_column(),
            projection.summary_basis_version.to_column(),
            projection.generated_work_summary.as_deref(),
            projection
                .generated_summary_basis_version
                .map(EventVersion::to_column),
            projection.lifecycle_adapter_id.as_deref(),
        ],
    )
    .await?;
    Ok(())
}

async fn load_agent_snapshot_from_one_query(conn: &turso::Connection) -> Result<AgentSnapshot> {
    let mut rows = conn
        .query(
            r#"
            WITH checkpoint AS (
                SELECT COALESCE((
                    SELECT last_processed_event_version
                    FROM last_processed_event
                    WHERE namespace = ?1 AND partition_key = ?2
                ), 0) AS through_event_version
            ), snapshot_rows AS (
                SELECT
                    'checkpoint' AS row_kind,
                    checkpoint.through_event_version,
                    NULL AS component,
                    NULL AS healthy,
                    NULL AS reason_code,
                    NULL AS health_observed_at_ms,
                    NULL AS health_last_event_version,
                    NULL AS pane_id,
                    NULL AS pane_pid,
                    NULL AS agent_pid,
                    NULL AS agent_started_at_ms,
                    NULL AS provider_id,
                    NULL AS tmux_target,
                    NULL AS session_name,
                    NULL AS window_index,
                    NULL AS pane_index,
                    NULL AS working_directory,
                    NULL AS provider_display_name,
                    NULL AS pane_observed_at_ms,
                    NULL AS hook_state,
                    NULL AS hook_observed_at_ms,
                    NULL AS screen_state,
                    NULL AS screen_classifier_id,
                    NULL AS screen_observed_at_ms,
                    NULL AS effective_state,
                    NULL AS explicit_work_summary,
                    NULL AS explicit_work_summary_updated_at_ms,
                    NULL AS screen_work_summary,
                    NULL AS screen_work_summary_updated_at_ms,
                    NULL AS work_summary,
                    NULL AS last_transition_at_ms,
                    NULL AS pane_last_event_version,
                    NULL AS summary_basis_version,
                    NULL AS generated_work_summary,
                    NULL AS generated_summary_basis_version,
                    NULL AS lifecycle_adapter_id
                FROM checkpoint

                UNION ALL

                SELECT
                    'health',
                    checkpoint.through_event_version,
                    health.component,
                    health.healthy,
                    health.reason_code,
                    health.observed_at_ms,
                    health.last_event_version,
                    NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
                    NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
                    NULL, NULL, NULL, NULL, NULL, NULL, NULL
                FROM checkpoint, agent_monitor_health AS health

                UNION ALL

                SELECT
                    'pane',
                    checkpoint.through_event_version,
                    NULL, NULL, NULL, NULL, NULL,
                    pane.pane_id,
                    pane.pane_pid,
                    pane.agent_pid,
                    pane.agent_started_at_ms,
                    pane.provider_id,
                    pane.tmux_target,
                    pane.session_name,
                    pane.window_index,
                    pane.pane_index,
                    pane.working_directory,
                    pane.provider_display_name,
                    pane.pane_observed_at_ms,
                    pane.hook_state,
                    pane.hook_observed_at_ms,
                    pane.screen_state,
                    pane.screen_classifier_id,
                    pane.screen_observed_at_ms,
                    pane.effective_state,
                    pane.explicit_work_summary,
                    pane.explicit_work_summary_updated_at_ms,
                    pane.screen_work_summary,
                    pane.screen_work_summary_updated_at_ms,
                    pane.work_summary,
                    pane.last_transition_at_ms,
                    pane.last_event_version,
                    pane.summary_basis_version,
                    pane.generated_work_summary,
                    pane.generated_summary_basis_version,
                    pane.lifecycle_adapter_id
                FROM checkpoint, agent_panes AS pane
            )
            SELECT * FROM snapshot_rows
            ORDER BY row_kind, component, pane_id
            "#,
            (NAMESPACE, PARTITION_KEY),
        )
        .await?;

    let mut snapshot = AgentSnapshot {
        through_event_version: EventVersion::start(),
        server_time_ms: now_ms(),
        monitor_health: Vec::new(),
        panes: Vec::new(),
        tts_muted: false,
        messaging_paused: false,
    };
    while let Some(row) = rows.next().await? {
        snapshot.through_event_version = EventVersion::from_column(required_integer(&row, 1)?)?;
        match required_text(&row, 0)?.as_str() {
            "checkpoint" => {}
            "health" => snapshot.monitor_health.push(MonitorHealthProjection {
                component: required_text(&row, 2)?,
                healthy: required_integer(&row, 3)? != 0,
                reason_code: required_text(&row, 4)?,
                observed_at_ms: required_integer(&row, 5)?,
                last_event_version: EventVersion::from_column(required_integer(&row, 6)?)?,
            }),
            "pane" => snapshot.panes.push(agent_pane_from_row_at(&row, 7)?),
            row_kind => {
                return Err(StoreError::Data(format!(
                    "invalid snapshot row kind: {row_kind}"
                )));
            }
        }
    }
    Ok(snapshot)
}

async fn load_agent_pane(
    conn: &turso::Connection,
    pane_id: &str,
) -> Result<Option<AgentPaneProjection>> {
    let mut rows = conn
        .query(
            format!("SELECT {AGENT_PANE_COLUMNS} FROM agent_panes WHERE pane_id = ?1"),
            (pane_id,),
        )
        .await?;
    rows.next()
        .await?
        .map(|row| agent_pane_from_row(&row))
        .transpose()
}

fn agent_pane_from_row(row: &turso::Row) -> Result<AgentPaneProjection> {
    agent_pane_from_row_at(row, 0)
}

fn agent_pane_from_row_at(row: &turso::Row, offset: usize) -> Result<AgentPaneProjection> {
    Ok(AgentPaneProjection {
        pane: crate::agent::domain::AgentPaneObservation {
            incarnation: crate::agent::domain::AgentIncarnation {
                pane_id: required_text(row, offset)?,
                pane_pid: required_u32(row, offset + 1)?,
                agent_pid: required_u32(row, offset + 2)?,
                agent_started_at_ms: required_integer(row, offset + 3)?,
                provider_id: required_text(row, offset + 4)?,
            },
            tmux_target: required_text(row, offset + 5)?,
            session_name: required_text(row, offset + 6)?,
            window_index: required_u32(row, offset + 7)?,
            pane_index: required_u32(row, offset + 8)?,
            working_directory: required_text(row, offset + 9)?,
            provider_display_name: required_text(row, offset + 10)?,
            observed_at_ms: required_integer(row, offset + 11)?,
        },
        hook_state: optional_text(row, offset + 12)?
            .map(parse_observed_state)
            .transpose()?,
        hook_observed_at_ms: optional_integer(row, offset + 13)?,
        screen_state: optional_text(row, offset + 14)?
            .map(parse_observed_state)
            .transpose()?,
        screen_classifier_id: optional_text(row, offset + 15)?,
        screen_observed_at_ms: optional_integer(row, offset + 16)?,
        effective_state: parse_effective_state(&required_text(row, offset + 17)?)?,
        explicit_work_summary: optional_text(row, offset + 18)?,
        explicit_work_summary_updated_at_ms: optional_integer(row, offset + 19)?,
        screen_work_summary: optional_text(row, offset + 20)?,
        screen_work_summary_updated_at_ms: optional_integer(row, offset + 21)?,
        work_summary: optional_text(row, offset + 22)?,
        last_transition_at_ms: required_integer(row, offset + 23)?,
        last_event_version: EventVersion::from_column(required_integer(row, offset + 24)?)?,
        summary_basis_version: EventVersion::from_column(required_integer(row, offset + 25)?)?,
        generated_work_summary: optional_text(row, offset + 26)?,
        generated_summary_basis_version: optional_integer(row, offset + 27)?
            .map(EventVersion::from_column)
            .transpose()?,
        lifecycle_adapter_id: optional_text(row, offset + 28)?,
    })
}

fn required_text(row: &turso::Row, index: usize) -> Result<String> {
    row.get_value(index)?
        .as_text()
        .map(ToString::to_string)
        .ok_or_else(|| StoreError::Data(format!("projection column {index} is not text")))
}

fn optional_text(row: &turso::Row, index: usize) -> Result<Option<String>> {
    let value = row.get_value(index)?;
    if value.is_null() {
        return Ok(None);
    }
    value
        .as_text()
        .map(|text| Some(text.to_string()))
        .ok_or_else(|| StoreError::Data(format!("projection column {index} is not text")))
}

fn required_integer(row: &turso::Row, index: usize) -> Result<i64> {
    row.get_value(index)?
        .as_integer()
        .copied()
        .ok_or_else(|| StoreError::Data(format!("projection column {index} is not an integer")))
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
        .ok_or_else(|| StoreError::Data(format!("projection column {index} is not an integer")))
}

fn required_u32(row: &turso::Row, index: usize) -> Result<u32> {
    u32::try_from(required_integer(row, index)?)
        .map_err(|_| StoreError::Data(format!("projection column {index} is not a u32")))
}

#[cfg(test)]
async fn pause_snapshot_read_after_query(gate: &std::sync::Mutex<Option<SnapshotReadGate>>) {
    let snapshot_gate = gate.lock().expect("snapshot read gate lock").take();
    if let Some(SnapshotReadGate { started, resume }) = snapshot_gate {
        let _ = started.send(());
        let _ = resume.await;
    }
}

fn observed_state_text(state: ObservedAgentState) -> &'static str {
    match state {
        ObservedAgentState::Busy => "busy",
        ObservedAgentState::Idle => "idle",
    }
}

fn parse_observed_state(value: String) -> Result<ObservedAgentState> {
    match value.as_str() {
        "busy" => Ok(ObservedAgentState::Busy),
        "idle" => Ok(ObservedAgentState::Idle),
        _ => Err(StoreError::Data(format!(
            "invalid observed agent state: {value}"
        ))),
    }
}

fn effective_state_text(state: EffectiveAgentState) -> &'static str {
    match state {
        EffectiveAgentState::Busy => "busy",
        EffectiveAgentState::Idle => "idle",
        EffectiveAgentState::Unknown => "unknown",
    }
}

fn parse_effective_state(value: &str) -> Result<EffectiveAgentState> {
    match value {
        "busy" => Ok(EffectiveAgentState::Busy),
        "idle" => Ok(EffectiveAgentState::Idle),
        "unknown" => Ok(EffectiveAgentState::Unknown),
        _ => Err(StoreError::Data(format!(
            "invalid effective agent state: {value}"
        ))),
    }
}

fn now_ms() -> i64 {
    (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
}

async fn configure_state_database(conn: &turso::Connection) -> Result<()> {
    conn.busy_timeout(Duration::from_millis(5_000))?;
    let mut rows = conn.query("PRAGMA journal_mode = WAL", ()).await?;
    while rows.next().await?.is_some() {}
    conn.execute("PRAGMA synchronous = NORMAL", ()).await?;
    Ok(())
}

async fn initialize_state_schema(conn: &turso::Connection) -> Result<()> {
    conn.execute("BEGIN IMMEDIATE", ()).await?;
    let result = apply_migrations(conn).await;
    if let Err(error) = result {
        let _ = conn.execute("ROLLBACK", ()).await;
        return Err(error);
    }
    if let Err(error) = conn.execute("COMMIT", ()).await {
        let _ = conn.execute("ROLLBACK", ()).await;
        return Err(error.into());
    }
    Ok(())
}

async fn upsert_text_setting(
    conn: &turso::Connection,
    key: &str,
    value: &str,
    event_version: EventVersion,
) -> Result<()> {
    conn.execute(
        r#"
        INSERT INTO settings (key, value, last_event_version)
        VALUES (?1, ?2, ?3)
        ON CONFLICT(key) DO UPDATE SET
            value = excluded.value,
            last_event_version = excluded.last_event_version
        "#,
        (key, value, event_version.to_column()),
    )
    .await?;
    Ok(())
}

async fn upsert_bool_setting(
    conn: &turso::Connection,
    key: &str,
    value: bool,
    event_version: EventVersion,
) -> Result<()> {
    let value = if value { "true" } else { "false" };
    upsert_text_setting(conn, key, value, event_version).await
}

async fn load_bool_setting(conn: &turso::Connection, key: &str) -> Result<bool> {
    let mut rows = conn
        .query("SELECT value FROM settings WHERE key = ?1", [key])
        .await?;
    Ok(match rows.next().await? {
        Some(row) => row.get_value(0)?.as_text().map(String::as_str) == Some("true"),
        None => false,
    })
}

async fn apply_migrations(conn: &turso::Connection) -> Result<()> {
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
        let name = row.get_value(0)?.as_text().cloned().unwrap_or_default();
        let checksum = row.get_value(1)?.as_text().cloned().unwrap_or_default();
        applied.insert(name, checksum);
    }
    drop(rows);

    if applied
        .keys()
        .any(|name| !MIGRATIONS.iter().any(|(known, _)| known == name))
    {
        return Err(StoreError::Data("incompatible state schema".into()));
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

/// The monitor settings that decide how long a report stands against the screen reading.
pub(crate) fn graces_from(monitor: &crate::settings::AgentMonitorSettings) -> GraceWindows {
    GraceWindows {
        hook_ms: monitor.hook_grace_ms,
        report_ms: monitor.report_grace_ms,
    }
}

pub async fn open_store(path: impl AsRef<Path>) -> Result<Arc<HaroldStore>> {
    let graces = graces_from(&crate::settings::get_settings().agent_monitor);
    Ok(Arc::new(HaroldStore::open_with_graces(path, graces).await?))
}

#[cfg(test)]
pub async fn append_turn_completed(store: &HaroldStore, event: &TurnCompleted) -> Result<()> {
    store
        .append([NewEvent::new("TurnCompleted", json!(event))])
        .await?;
    Ok(())
}

pub async fn append_tts_mute_changed(store: &HaroldStore, muted: bool) -> Result<()> {
    store
        .append([NewEvent::new("TtsMuteChanged", json!({ "muted": muted }))])
        .await?;
    Ok(())
}

pub async fn append_messaging_paused_changed(store: &HaroldStore, paused: bool) -> Result<()> {
    store
        .append([NewEvent::new(
            "MessagingPausedChanged",
            json!({ "paused": paused }),
        )])
        .await?;
    Ok(())
}

pub async fn append_inbound_message(store: &HaroldStore, event: &InboundMessage) -> Result<()> {
    store
        .append([NewEvent::new("InboundMessageReceived", json!(event))])
        .await?;
    Ok(())
}

/// Stores a reply the user typed in the web app for the chief.
#[cfg(test)]
pub(crate) async fn append_app_reply(store: &HaroldStore, text: &str) -> Result<()> {
    append_app_reply_with_photo(store, text, None).await
}

/// Stores a reply for the chief that may carry a photo: `photo` is its file name in
/// `app-photos/`, which the payload then names.
pub(crate) async fn append_app_reply_with_photo(
    store: &HaroldStore,
    text: &str,
    photo: Option<&str>,
) -> Result<()> {
    let mut payload = json!({ "text": text, "to_chief": true });
    if let Some(photo) = photo {
        payload["photo"] = json!(photo);
    }
    store
        .append([NewEvent::new("InboundMessageReceived", payload)])
        .await?;
    Ok(())
}

pub(crate) async fn append_app_push_subscribed(
    store: &HaroldStore,
    event: &AppPushSubscribed,
) -> Result<()> {
    store
        .append([NewEvent::new("AppPushSubscribed", json!(event))])
        .await?;
    Ok(())
}

pub(crate) async fn append_app_push_unsubscribed(
    store: &HaroldStore,
    event: &AppPushUnsubscribed,
) -> Result<()> {
    store
        .append([NewEvent::new("AppPushUnsubscribed", json!(event))])
        .await?;
    Ok(())
}

pub async fn append_away_message_requested(
    store: &HaroldStore,
    event: &AwayMessageRequested,
) -> Result<()> {
    store
        .append([NewEvent::new("AwayMessageRequested", json!(event))])
        .await?;
    Ok(())
}

pub async fn append_spoken_line_requested(
    store: &HaroldStore,
    event: &SpokenLineRequested,
) -> Result<()> {
    store
        .append([NewEvent::new("SpokenLineRequested", json!(event))])
        .await?;
    Ok(())
}

pub(crate) async fn append_timed_prompt_handled(
    store: &HaroldStore,
    event: &TimedPromptHandled,
) -> Result<()> {
    // The tests' switch for a failing append, shared with the monitor's appends.
    fail_monitor_append_for_test(store)?;
    store
        .append([NewEvent::new("TimedPromptHandled", json!(event))])
        .await?;
    Ok(())
}

#[allow(
    dead_code,
    reason = "the serialized monitor runtime is the sole appender in the next monitor slice"
)]
pub(crate) async fn append_agent_events(
    store: &HaroldStore,
    events: Vec<AgentEvent>,
) -> Result<AppendResult> {
    fail_monitor_append_for_test(store)?;
    let events = events
        .into_iter()
        .filter_map(normalize_agent_event)
        .map(agent_new_event)
        .collect::<Result<Vec<_>>>()?;
    store.append(events).await
}

pub(crate) async fn append_monitor_turn_completed(
    store: &HaroldStore,
    pane: Option<AgentPaneObservation>,
    turn: &TurnCompleted,
) -> Result<AppendResult> {
    fail_monitor_append_for_test(store)?;
    let mut events = Vec::with_capacity(usize::from(pane.is_some()) + 1);
    if let Some(pane) = pane {
        let pane = normalize_pane_observation(pane);
        events.push(agent_new_event(AgentEvent::PaneObserved(
            crate::agent::domain::AgentPaneObserved { pane },
        ))?);
    }
    events.push(NewEvent::new("TurnCompleted", json!(turn)));
    store.append(events).await
}

#[cfg(test)]
fn fail_monitor_append_for_test(store: &HaroldStore) -> Result<()> {
    if store
        .fail_next_monitor_append
        .swap(false, std::sync::atomic::Ordering::SeqCst)
    {
        return Err(StoreError::Data("test monitor append failure".into()));
    }
    Ok(())
}

#[cfg(not(test))]
fn fail_monitor_append_for_test(_store: &HaroldStore) -> Result<()> {
    Ok(())
}

#[allow(
    dead_code,
    reason = "called by the runtime-facing agent append boundary above"
)]
fn normalize_agent_event(event: AgentEvent) -> Option<AgentEvent> {
    match event {
        AgentEvent::PaneObserved(mut observed) => {
            observed.pane = normalize_pane_observation(observed.pane);
            Some(AgentEvent::PaneObserved(observed))
        }
        AgentEvent::LifecycleObserved(mut lifecycle) => {
            if let WorkSummaryUpdate::Set(summary) = lifecycle.work_summary {
                lifecycle.work_summary = normalize_work_summary(&summary)
                    .map_or(WorkSummaryUpdate::Clear, WorkSummaryUpdate::Set);
            }
            Some(AgentEvent::LifecycleObserved(lifecycle))
        }
        AgentEvent::ScreenObserved(screen) => normalize_screen_event(screen),
        AgentEvent::ActivitySummaryGenerated(mut generated) => {
            generated.description = normalize_work_summary(&generated.description)?;
            Some(AgentEvent::ActivitySummaryGenerated(generated))
        }
        AgentEvent::MonitorHealthChanged(mut health) => {
            health.component = normalize_health_code(&health.component, 64, "invalid_component");
            health.reason_code = normalize_health_code(&health.reason_code, 160, "invalid_reason");
            Some(AgentEvent::MonitorHealthChanged(health))
        }
        event => Some(event),
    }
}

fn normalize_health_code(input: &str, max_len: usize, fallback: &str) -> String {
    let valid = !input.is_empty()
        && input.len() <= max_len
        && input.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte)
        });
    if valid { input.into() } else { fallback.into() }
}

pub(crate) fn normalize_pane_observation(mut pane: AgentPaneObservation) -> AgentPaneObservation {
    pane.tmux_target = sanitize_bounded_metadata(&pane.tmux_target, 256);
    pane.session_name = sanitize_bounded_metadata(&pane.session_name, 256);
    pane.provider_display_name = sanitize_bounded_metadata(&pane.provider_display_name, 256);
    pane.working_directory = sanitize_bounded_metadata(&pane.working_directory, 1_024);
    pane
}

#[allow(
    dead_code,
    reason = "called by the runtime-facing agent append boundary above"
)]
fn normalize_screen_event(mut screen: AgentScreenObserved) -> Option<AgentEvent> {
    screen.fallback_summary = screen
        .fallback_summary
        .as_deref()
        .and_then(normalize_work_summary);
    (screen.state.is_some() || screen.fallback_summary.is_some())
        .then_some(AgentEvent::ScreenObserved(screen))
}

#[allow(
    dead_code,
    reason = "called by the runtime-facing agent append boundary above"
)]
fn agent_new_event(event: AgentEvent) -> Result<NewEvent> {
    let (event_type, payload) = match event {
        AgentEvent::PaneObserved(event) => ("AgentPaneObserved", serde_json::to_value(event)?),
        AgentEvent::PaneDeparted(event) => ("AgentPaneDeparted", serde_json::to_value(event)?),
        AgentEvent::LifecycleObserved(event) => {
            ("AgentLifecycleObserved", serde_json::to_value(event)?)
        }
        AgentEvent::ScreenObserved(event) => ("AgentScreenObserved", serde_json::to_value(event)?),
        AgentEvent::MonitorHealthChanged(event) => {
            ("AgentMonitorHealthChanged", serde_json::to_value(event)?)
        }
        AgentEvent::ActivitySummaryGenerated(event) => (
            "AgentActivitySummaryGenerated",
            serde_json::to_value(event)?,
        ),
    };
    Ok(NewEvent::new(event_type, payload))
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
