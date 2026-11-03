//! One-time reminders. A reminder runs no model: at its time Harold types
//! `REMIND <name>: <text>` once, with the same checks as a watch report (one agent in the
//! folder, idle, an empty input box, tried again every 5 seconds).
//!
//! Each waiting reminder has a task. The store holds the truth, so a restart gives each
//! waiting reminder a new task. A reminder not typed within 120 minutes of its time is
//! dropped. The task stores `typed` as soon as it sees the line typed, and a restart reads
//! only waiting reminders, so a reminder is never typed twice.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::sync::watch as signal;

use super::clock::{LocalOffset, clock_ms, local_ms, local_text};
use super::obs::{self, ReminderEnd, ReportObs};
use super::report::{ReportSlot, Target, Typist};
use super::scheduler::{RemindRefusal, interrupted};
use super::store::{NewReminder, ReminderRow, ReminderState, StoreError, WatchStore};
use super::validate::{CheckedRemind, ReportTo};

const MINUTE_MS: i64 = 60_000;

/// A reminder not typed this long after its time is dropped: the timed prompts' give-up
/// time.
const GIVE_UP_MS: i64 = 120 * MINUTE_MS;

/// True when a reminder due at `due_ms` is too late to type at `now_ms`: from exactly
/// 120 minutes after its time.
fn too_late(now_ms: i64, due_ms: i64) -> bool {
    now_ms - due_ms >= GIVE_UP_MS
}

/// A waiting task wakes at least this often. It reads the local offset again, so a `--at`
/// time follows a change of daylight saving time, and it sees the wall clock jump after
/// the computer sleeps.
const WAKE_EVERY: Duration = Duration::from_secs(60);

/// A waiting reminder as `list` shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ReminderInfo {
    pub name: String,
    pub report_to: String,
    /// When it is due, with the local offset in force now.
    pub due_at_ms: i64,
}

struct Task {
    created_at_ms: i64,
    cancel: signal::Sender<bool>,
}

pub(super) struct Reminders {
    store: Arc<WatchStore>,
    typist: Arc<Typist>,
    local: Arc<dyn LocalOffset>,
    retry: Duration,
    shutdown: signal::Receiver<()>,
    tasks: Mutex<HashMap<String, Task>>,
}

/// Who a reminder types to: the chief, or the agent in the folder that set it.
fn target(row: &ReminderRow) -> Target {
    if row.report_to == ReportTo::Chief.as_str() {
        Target::Chief
    } else {
        Target::Folder(row.folder.clone())
    }
}

impl Reminders {
    pub(super) fn new(
        store: Arc<WatchStore>,
        typist: Arc<Typist>,
        local: Arc<dyn LocalOffset>,
        retry: Duration,
        shutdown: signal::Receiver<()>,
    ) -> Arc<Self> {
        obs::reminder_counters();
        Arc::new(Self {
            store,
            typist,
            local,
            retry,
            shutdown,
            tasks: Mutex::default(),
        })
    }

    fn tasks(&self) -> MutexGuard<'_, HashMap<String, Task>> {
        self.tasks.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Stores a reminder and starts its task. Returns when it is due, in milliseconds.
    /// The caller holds the scheduler's start lock, so a watch cannot take the name
    /// meanwhile.
    pub(super) async fn set(
        self: &Arc<Self>,
        req: CheckedRemind,
        now_ms: i64,
    ) -> Result<i64, RemindRefusal> {
        let watch = self
            .store
            .get(&req.name)
            .await
            .map_err(|_| RemindRefusal::Unavailable)?;
        if watch.is_some_and(|row| row.is_live()) {
            return Err(RemindRefusal::NameInUse);
        }
        let waiting = self
            .store
            .reminder(&req.name)
            .await
            .map_err(|_| RemindRefusal::Unavailable)?;
        if waiting.is_some_and(|row| row.state == ReminderState::Waiting) {
            return Err(RemindRefusal::NameInUse);
        }
        let offset = self.local.offset().await;
        let due_at_ms = match (req.in_minutes, &req.due_local) {
            (Some(minutes), _) => now_ms + i64::from(minutes) * MINUTE_MS,
            (None, Some(text)) => local_ms(text, offset).ok_or(RemindRefusal::Unavailable)?,
            (None, None) => return Err(RemindRefusal::Unavailable),
        };
        let reminder = NewReminder {
            name: req.name.clone(),
            folder: req.folder.to_string_lossy().into_owned(),
            report_to: req.report_to.as_str().to_owned(),
            text: req.text,
            // `--in` is a fixed instant; `--at` keeps its local time and is read again
            // with the offset in force at each wake-up.
            due_at_ms: req.in_minutes.map(|_| due_at_ms),
            due_local: req.in_minutes.is_none().then_some(req.due_local).flatten(),
        };
        self.store
            .put_reminder(&reminder, now_ms)
            .await
            .map_err(|_| RemindRefusal::Unavailable)?;
        let row = self
            .store
            .reminder(&req.name)
            .await
            .ok()
            .flatten()
            .ok_or(RemindRefusal::Unavailable)?;
        obs::reminder_set(&row.name, &local_text(due_at_ms, offset));
        self.spawn(row);
        Ok(due_at_ms)
    }

    /// Cancels a waiting reminder. `false` when no reminder of that name waits.
    pub(super) async fn cancel(&self, name: &str) -> Result<bool, StoreError> {
        let Some(row) = self.store.reminder(name).await? else {
            return Ok(false);
        };
        if row.state != ReminderState::Waiting {
            return Ok(false);
        }
        self.finish(&row, ReminderState::Cancelled, 0).await
    }

    /// At start-up: a task for each reminder that was waiting when Harold stopped.
    pub(super) async fn resume_all(self: &Arc<Self>) {
        let Ok(waiting) = self.store.waiting_reminders().await else {
            return;
        };
        for row in waiting {
            self.spawn(row);
        }
    }

    /// Every waiting reminder, oldest first.
    pub(super) async fn waiting(&self) -> Result<Vec<ReminderRow>, StoreError> {
        self.store.waiting_reminders().await
    }

    /// Every waiting reminder with when it is due now.
    pub(super) async fn listed(&self) -> Result<Vec<ReminderInfo>, StoreError> {
        let mut listed = Vec::new();
        for row in self.waiting().await? {
            if let Some(due_at_ms) = self.due_at_ms(&row).await {
                listed.push(ReminderInfo {
                    name: row.name,
                    report_to: row.report_to,
                    due_at_ms,
                });
            }
        }
        Ok(listed)
    }

    /// When `row` is due, with the local offset in force now.
    async fn due_at_ms(&self, row: &ReminderRow) -> Option<i64> {
        match (row.due_at_ms, &row.due_local) {
            (Some(due), _) => Some(due),
            (None, Some(text)) => local_ms(text, self.local.offset().await),
            (None, None) => None,
        }
    }

    fn spawn(self: &Arc<Self>, row: ReminderRow) {
        let (cancel, cancelled) = signal::channel(false);
        let old = self.tasks().insert(
            row.name.clone(),
            Task {
                created_at_ms: row.created_at_ms,
                cancel,
            },
        );
        if let Some(old) = old {
            let _ = old.cancel.send(true);
        }
        tokio::spawn(Arc::clone(self).task(row, cancelled));
    }

    /// Stores how a waiting reminder ended, stops its task, and logs it. `due_at_ms` gives
    /// `late_s` for a typed reminder. `false` when the reminder no longer waits.
    async fn finish(
        &self,
        row: &ReminderRow,
        state: ReminderState,
        due_at_ms: i64,
    ) -> Result<bool, StoreError> {
        let now = clock_ms();
        let finished = self
            .store
            .finish_reminder(&row.name, row.created_at_ms, state, now)
            .await?;
        if finished.is_none() {
            return Ok(false);
        }
        let task = {
            let mut tasks = self.tasks();
            match tasks.get(&row.name) {
                Some(task) if task.created_at_ms == row.created_at_ms => tasks.remove(&row.name),
                _ => None,
            }
        };
        if let Some(task) = task {
            let _ = task.cancel.send(true);
        }
        match state {
            ReminderState::Typed => {
                obs::reminder_typed(&row.name, ((now - due_at_ms) / 1000).max(0));
            }
            ReminderState::Dropped => obs::reminder_ended(&row.name, ReminderEnd::Dropped),
            ReminderState::Cancelled => obs::reminder_ended(&row.name, ReminderEnd::Cancelled),
            ReminderState::Waiting => {}
        }
        Ok(true)
    }

    /// One reminder: sleeps until it is due, then types it once, or drops it.
    async fn task(self: Arc<Self>, row: ReminderRow, mut cancelled: signal::Receiver<bool>) {
        let mut shutdown = self.shutdown.clone();
        // Sleep until due. The due time is read again at each wake-up.
        let due = loop {
            if *cancelled.borrow() {
                return;
            }
            let Some(due) = self.due_at_ms(&row).await else {
                // The store's checks make this impossible; drop it rather than wait forever.
                let _ = self.finish(&row, ReminderState::Dropped, 0).await;
                return;
            };
            let now = clock_ms();
            if now >= due {
                break due;
            }
            let wait = Duration::from_millis((due - now) as u64).min(WAKE_EVERY);
            tokio::select! {
                () = tokio::time::sleep(wait) => {}
                () = interrupted(&mut cancelled, &mut shutdown) => return,
            }
        };
        if too_late(clock_ms(), due) {
            let _ = self.finish(&row, ReminderState::Dropped, due).await;
            return;
        }
        let slot = ReportSlot::new(Arc::clone(&self.typist), target(&row), self.retry);
        // The slot checks the wall clock before each try, with the same rule as
        // `too_late`: `clock_ms() >= due + GIVE_UP_MS` gives up.
        let mut typed = slot.last_line(
            format!("REMIND {}: {}", row.name, row.text),
            ReportObs::new(&row.name, 0),
            due + GIVE_UP_MS,
        );
        tokio::select! {
            biased;
            answer = &mut typed => {
                let state = if answer == Ok(true) {
                    ReminderState::Typed
                } else {
                    ReminderState::Dropped
                };
                let _ = self.finish(&row, state, due).await;
            }
            () = interrupted(&mut cancelled, &mut shutdown) => {
                // Stopped, or Harold stops. The slot makes no new try, but a try in
                // progress can still type the line: wait for its result (one try at
                // most). After a shutdown the reminder still waits in the store, so a
                // typed line is stored as typed and a restart does not type it again.
                // After a stop the store already holds `cancelled`, so `finish` changes
                // nothing: the outcome stays `cancelled` even when that try typed it.
                slot.close(None);
                if (&mut typed).await == Ok(true) {
                    let _ = self.finish(&row, ReminderState::Typed, due).await;
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "reminders_tests.rs"]
mod tests;
