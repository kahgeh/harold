//! The watches' schedule: when each runs, what a run does, and how a watch ends.
//!
//! Each running watch has a task that sleeps until its next run. One model runs at a
//! time across all of them (`model`), and a run that cannot take it within 60 seconds is
//! skipped. The store holds the truth, so a restart resumes every running watch.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use time::UtcOffset;
use tokio::sync::watch as signal;
use tokio::time::Instant;
use tracing::Instrument as _;

use super::answer::{self, Answer, Cause, Failure};
use super::clock::{LocalOffset, WAKE_EVERY, clock_ms, day_bounds, local_text};
use super::facts::{self, Sources, WatchEntry};
use super::obs::{self, EndReason, How, RunObs, Timing};
use super::reminders::{ReminderInfo, Reminders};
use super::report::{ReportSlot, Target, Typist};
use super::runner::{Exit, ModelJob, ModelResult, ModelRunner};
use super::store::{
    NewWatch, ReminderState, RunRecord, StoreError, Streak, WatchRow, WatchState, WatchStore,
};
use super::validate::{
    Caller, Checked, CheckedPause, CheckedRemind, CheckedResume, ReportTo, Target as Aim,
    read_skill, to_micros,
};
use crate::settings::WatchSettings;
use crate::timed_prompts::{LocalNow, parse_local_now};
use crate::tools::{Agents, ChiefPane, PaneTyping, Pods, WatchCounts};

/// The answer or the failure, the reported cost, and how the process ended with what
/// Claude Code reported (for a run that exited).
/// The answer, the reported cost, why a run that exited failed, and the milliseconds the
/// Claude Code process ran (0 when none started).
type ModelOutcome = (
    Result<Answer, Failure>,
    Option<f64>,
    Option<(Exit, Cause)>,
    u64,
);

/// At most this many watches run at once.
pub(super) const MAX_WATCHES: usize = 5;

/// Failed runs in a row that stop a watch.
const FAILURES_THAT_STOP: u32 = 3;

/// A run that cannot take the model within this long of its time is skipped.
pub(super) const MODEL_WAIT: Duration = Duration::from_secs(60);

/// How often a waiting report is tried again.
pub(super) const RETRY: Duration = Duration::from_secs(5);

const MINUTE_MS: i64 = 60_000;

/// Everything the scheduler reads or does outside this module.
pub(super) struct Deps {
    pub store: Arc<WatchStore>,
    pub runner: Arc<dyn ModelRunner>,
    pub typing: Arc<dyn PaneTyping>,
    pub agents: Arc<dyn Agents>,
    pub pods: Arc<dyn Pods>,
    pub chief_pane: Arc<dyn ChiefPane>,
    pub settings: WatchSettings,
    pub local: Arc<dyn LocalOffset>,
    pub shutdown: signal::Receiver<()>,
    /// Where the scheduler pushes the watch counts for the host.
    pub counts: Arc<signal::Sender<WatchCounts>>,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum StartRefusal {
    TooManyWatches,
    DailyBudgetSpent,
    DailyBudgetUnset,
    /// A waiting reminder has the name.
    NameInUse,
    Unavailable,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum PauseRefusal {
    UnknownWatch,
    NotRunning,
    BudgetDay,
    Unavailable,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ResumeRefusal {
    UnknownWatch,
    NotPaused,
    BudgetDay,
    Unavailable,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum RemindRefusal {
    NameInUse,
    Unavailable,
}

/// The watches a pause or a resume changed, and the ones it left alone.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Changed {
    pub done: Vec<String>,
    pub skipped: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum StopRefusal {
    UnknownWatch,
    NotRunning,
    Unavailable,
}

/// What `list` shows.
#[derive(Debug, PartialEq)]
pub(super) struct Listing {
    pub watches: Vec<WatchRow>,
    pub spent_usd_micros: i64,
    /// Every waiting reminder.
    pub reminders: Vec<ReminderInfo>,
}

struct Live {
    started_at_ms: i64,
    cancel: signal::Sender<bool>,
    slot: Arc<ReportSlot>,
}

#[derive(Default)]
struct State {
    live: HashMap<String, Live>,
}

pub(super) struct Scheduler {
    deps: Deps,
    typist: Arc<Typist>,
    reminders: Arc<Reminders>,
    /// One model at a time across all watches.
    model: tokio::sync::Mutex<()>,
    state: Mutex<State>,
    /// One `start` at a time, so the limit and a replacement are checked and stored together.
    start_lock: tokio::sync::Mutex<()>,
    /// One count push at a time, so an older read never overwrites a newer one.
    counts_lock: tokio::sync::Mutex<()>,
    model_wait: Duration,
    retry: Duration,
}

/// Where a run ended up, for its watch's task.
enum Flow {
    Continue,
    /// The watch is over (or was cancelled): its task ends.
    Over,
}

/// The first time on the watch's grid (`started + k * every`) after `now`.
pub(super) fn next_on_grid(started_ms: i64, every_minutes: u32, now_ms: i64) -> i64 {
    let every = i64::from(every_minutes) * MINUTE_MS;
    started_ms + ((now_ms - started_ms).max(0) / every + 1) * every
}

/// How many runs the watch's time holds: `--for` over `--every`, rounded up. `None` for
/// a watch with no end.
pub(super) fn total_runs(row: &WatchRow) -> Option<u32> {
    let every = i64::from(row.every_minutes) * MINUTE_MS;
    let span = row.ends_at_ms? - row.started_at_ms;
    Some(((span + every - 1) / every).max(1) as u32)
}

/// The `how` of a pause or a resume that a person or an agent asked for.
fn asked_by(caller: Caller) -> How {
    match caller {
        Caller::Cli => How::Cli,
        Caller::ControlTower => How::ControlTower,
    }
}

/// True for a capped watch: the daily budget paused it.
fn is_capped(row: &WatchRow) -> bool {
    row.state == WatchState::Paused && row.pause_how.as_deref() == Some(How::BudgetDay.as_str())
}

/// The counts of the live watches. A capped watch is not counted as paused.
fn counts_of(live: &[WatchRow]) -> WatchCounts {
    let mut counts = WatchCounts::default();
    for row in live {
        match row.state {
            WatchState::Running => counts.running += 1,
            WatchState::Paused if is_capped(row) => counts.capped += 1,
            WatchState::Paused => counts.paused += 1,
            WatchState::Stopped => {}
        }
    }
    counts
}

/// `HH:MM` of a local time text `YYYY-MM-DD HH:MM:SS`.
fn clock_of(text: &str) -> &str {
    text.get(11..16).unwrap_or(text)
}

fn counted(count: u32, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

fn dollars(micros: i64) -> String {
    format!("${:.2}", micros as f64 / 1_000_000.0)
}

impl Scheduler {
    pub(super) fn new(deps: Deps) -> Arc<Self> {
        let typist = Arc::new(Typist {
            typing: Arc::clone(&deps.typing),
            chief: Arc::clone(&deps.chief_pane),
            agents: Arc::clone(&deps.agents),
        });
        let reminders = Reminders::new(
            Arc::clone(&deps.store),
            Arc::clone(&typist),
            Arc::clone(&deps.local),
            RETRY,
            deps.shutdown.clone(),
        );
        Arc::new(Self {
            deps,
            typist,
            reminders,
            model: tokio::sync::Mutex::new(()),
            state: Mutex::default(),
            start_lock: tokio::sync::Mutex::new(()),
            counts_lock: tokio::sync::Mutex::new(()),
            model_wait: MODEL_WAIT,
            retry: RETRY,
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The local zone offset now, read at each use.
    async fn offset(&self) -> UtcOffset {
        self.deps.local.offset().await
    }

    /// The local date and time of day now, for a reminder's `at_local`.
    pub(super) async fn local_now(&self) -> LocalNow {
        let text = local_text(clock_ms(), self.offset().await);
        // `local_text` always prints `YYYY-MM-DD HH:MM:SS` from a valid time, so the
        // parse cannot fail.
        parse_local_now(&text).expect("local_text prints a valid local date and time")
    }

    async fn today(&self) -> (i64, i64) {
        day_bounds(clock_ms(), self.offset().await)
    }

    fn daily_budget_micros(&self) -> Option<i64> {
        self.deps.settings.daily_budget_usd.map(to_micros)
    }

    // -----------------------------------------------------------------------
    // Commands
    // -----------------------------------------------------------------------

    /// Starts a watch, or replaces a running or paused one of the same name. Returns when
    /// it ends: `None` for a watch with no end.
    pub(super) async fn start(
        self: &Arc<Self>,
        checked: Checked,
    ) -> Result<Option<i64>, StartRefusal> {
        // A watch with no end always has a daily cap.
        if checked.for_minutes.is_none() && self.daily_budget_micros().is_none() {
            return Err(StartRefusal::DailyBudgetUnset);
        }
        let _one_start = self.start_lock.lock().await;
        let store = &self.deps.store;
        let reminder = store
            .reminder(&checked.name)
            .await
            .map_err(|_| StartRefusal::Unavailable)?;
        if reminder.is_some_and(|row| row.state == ReminderState::Waiting) {
            return Err(StartRefusal::NameInUse);
        }
        let live = store.live().await.map_err(|_| StartRefusal::Unavailable)?;
        let replaced = live.iter().any(|row| row.name == checked.name);
        let others = live.iter().filter(|row| row.name != checked.name).count();
        if others >= MAX_WATCHES {
            return Err(StartRefusal::TooManyWatches);
        }
        let now = clock_ms();
        let (day_start, day_end) = day_bounds(now, self.offset().await);
        if let Some(daily) = self.daily_budget_micros() {
            let spent = store
                .spent_micros(day_start, day_end)
                .await
                .map_err(|_| StartRefusal::Unavailable)?;
            let stopped = store
                .budget_stopped_since(day_start)
                .await
                .map_err(|_| StartRefusal::Unavailable)?;
            let capped = live
                .iter()
                .any(|row| is_capped(row) && row.paused_until_ms.is_some_and(|at| at > now));
            if spent >= daily || stopped || capped {
                return Err(StartRefusal::DailyBudgetSpent);
            }
        }
        let ends_at_ms = checked
            .for_minutes
            .map(|minutes| now + i64::from(minutes) * MINUTE_MS);
        let watch = NewWatch {
            name: checked.name.clone(),
            folder: checked.folder.to_string_lossy().into_owned(),
            skill: checked.skill.clone(),
            report_to: checked.report_to.as_str().to_owned(),
            every_minutes: checked.every_minutes,
            every_run: checked.every_run,
            run_budget_usd_micros: checked.run_budget_usd_micros,
            run_limit_secs: checked.run_limit_secs,
            started_at_ms: now,
            ends_at_ms,
            next_run_at_ms: now,
        };
        store
            .put_watch(&watch)
            .await
            .map_err(|_| StartRefusal::Unavailable)?;
        let row = store
            .get(&checked.name)
            .await
            .ok()
            .flatten()
            .ok_or(StartRefusal::Unavailable)?;
        self.spawn_watch(
            &row,
            if replaced {
                How::Replaced
            } else {
                How::Started
            },
        )
        .await;
        Ok(ends_at_ms)
    }

    /// Holds a running watch, or every running watch. `--all` skips a capped watch and
    /// names it, and skips a paused watch without a name.
    pub(super) async fn pause(
        self: &Arc<Self>,
        checked: CheckedPause,
    ) -> Result<Changed, PauseRefusal> {
        let _one_command = self.start_lock.lock().await;
        let until = checked
            .for_minutes
            .map(|minutes| clock_ms() + i64::from(minutes) * MINUTE_MS);
        let how = asked_by(checked.caller);
        let store = &self.deps.store;
        let mut changed = Changed::default();
        let named = matches!(checked.target, Aim::Name(_));
        let rows = match checked.target {
            Aim::Name(name) => {
                let row = store
                    .get(&name)
                    .await
                    .map_err(|_| PauseRefusal::Unavailable)?
                    .ok_or(PauseRefusal::UnknownWatch)?;
                if is_capped(&row) {
                    return Err(PauseRefusal::BudgetDay);
                }
                if row.state != WatchState::Running {
                    return Err(PauseRefusal::NotRunning);
                }
                vec![row]
            }
            Aim::All => {
                let live = store.live().await.map_err(|_| PauseRefusal::Unavailable)?;
                let (running, paused): (Vec<_>, Vec<_>) = live
                    .into_iter()
                    .partition(|row| row.state == WatchState::Running);
                changed.skipped = paused
                    .into_iter()
                    .filter(is_capped)
                    .map(|row| row.name)
                    .collect();
                running
            }
        };
        changed.done = self.pause_rows(rows, named, until, how).await?;
        Ok(changed)
    }

    /// Pauses each row and returns the names it paused. For a named watch, a store error
    /// is `Unavailable` and a watch that stopped running meanwhile is `NotRunning`. For
    /// `--all`, such a watch is skipped, and a store error is logged.
    async fn pause_rows(
        self: &Arc<Self>,
        rows: Vec<WatchRow>,
        named: bool,
        until: Option<i64>,
        how: How,
    ) -> Result<Vec<String>, PauseRefusal> {
        let mut done = Vec::new();
        for row in rows {
            match self.hold(&row, until, how).await {
                Ok(Some(_)) => done.push(row.name),
                Ok(None) if named => return Err(PauseRefusal::NotRunning),
                Err(_) if named => return Err(PauseRefusal::Unavailable),
                Ok(None) => {}
                Err(_) => store_failed(&row.name, true),
            }
        }
        Ok(done)
    }

    /// Releases a paused watch, or every paused watch. A capped watch is never resumed by
    /// hand: `--all` skips it and names it.
    pub(super) async fn resume_watches(
        self: &Arc<Self>,
        checked: CheckedResume,
    ) -> Result<Changed, ResumeRefusal> {
        let _one_command = self.start_lock.lock().await;
        let how = asked_by(checked.caller);
        let store = &self.deps.store;
        let mut changed = Changed::default();
        let named = matches!(checked.target, Aim::Name(_));
        let rows = match checked.target {
            Aim::Name(name) => {
                let row = store
                    .get(&name)
                    .await
                    .map_err(|_| ResumeRefusal::Unavailable)?
                    .ok_or(ResumeRefusal::UnknownWatch)?;
                if is_capped(&row) {
                    return Err(ResumeRefusal::BudgetDay);
                }
                if row.state != WatchState::Paused {
                    return Err(ResumeRefusal::NotPaused);
                }
                vec![row]
            }
            Aim::All => {
                let live = store.live().await.map_err(|_| ResumeRefusal::Unavailable)?;
                let (capped, paused): (Vec<_>, Vec<_>) = live
                    .into_iter()
                    .filter(|row| row.state == WatchState::Paused)
                    .partition(is_capped);
                changed.skipped = capped.into_iter().map(|row| row.name).collect();
                paused
            }
        };
        changed.done = self.resume_rows(rows, named, how).await?;
        Ok(changed)
    }

    /// Resumes each row and returns the names it resumed. For a named watch, a store error
    /// is `Unavailable` and a watch that is no longer paused is `NotPaused`. For `--all`,
    /// such a watch is skipped, and a store error is logged.
    async fn resume_rows(
        self: &Arc<Self>,
        rows: Vec<WatchRow>,
        named: bool,
        how: How,
    ) -> Result<Vec<String>, ResumeRefusal> {
        let mut done = Vec::new();
        for row in rows {
            match self.release(&row, how).await {
                Ok(Some(_)) => {
                    // A new task runs the first check at once.
                    self.spawn_task(&row);
                    done.push(row.name);
                }
                Ok(None) if named => return Err(ResumeRefusal::NotPaused),
                Err(_) if named => return Err(ResumeRefusal::Unavailable),
                Ok(None) => {}
                Err(_) => store_failed(&row.name, false),
            }
        }
        Ok(done)
    }

    /// Stores a reminder. Returns the time it is due, in milliseconds. A live watch or a
    /// waiting reminder with the name refuses it.
    pub(super) async fn remind(
        self: &Arc<Self>,
        checked: CheckedRemind,
    ) -> Result<i64, RemindRefusal> {
        // The start lock keeps a start and a reminder from taking one name together.
        let _one_command = self.start_lock.lock().await;
        self.reminders.set(checked, clock_ms()).await
    }

    /// Ends a running or paused watch at once. A run in progress is stopped, its report is
    /// not typed, and no closing line is typed. With no live watch of that name, it
    /// cancels a waiting reminder of that name.
    pub(super) async fn stop(self: &Arc<Self>, name: &str) -> Result<(), StopRefusal> {
        let row = self
            .deps
            .store
            .get(name)
            .await
            .map_err(|_| StopRefusal::Unavailable)?;
        if let Some(row) = row.as_ref().filter(|row| row.is_live()) {
            self.end(row, EndReason::Stopped, None).await;
            return Ok(());
        }
        if self
            .reminders
            .cancel(name)
            .await
            .map_err(|_| StopRefusal::Unavailable)?
        {
            return Ok(());
        }
        match row {
            Some(_) => Err(StopRefusal::NotRunning),
            None => Err(StopRefusal::UnknownWatch),
        }
    }

    /// Every running or paused watch, every watch that ended today, and every waiting
    /// reminder.
    pub(super) async fn list(&self) -> Result<Listing, StoreError> {
        let (day_start, day_end) = self.today().await;
        let mut watches = self.deps.store.live().await?;
        watches.extend(self.deps.store.ended_since(day_start).await?);
        let spent_usd_micros = self.deps.store.spent_micros(day_start, day_end).await?;
        Ok(Listing {
            watches,
            spent_usd_micros,
            reminders: self.reminders.listed().await?,
        })
    }

    /// Resumes every watch that was running or paused when Harold stopped. A paused watch
    /// stays paused until its pause ends. One whose time ran out meanwhile ends now. Each
    /// waiting reminder gets its task again.
    pub(super) async fn resume(self: &Arc<Self>) {
        if let Ok(live) = self.deps.store.live().await {
            for row in live {
                if row.ends_at_ms.is_some_and(|end| clock_ms() >= end) {
                    self.end(&row, EndReason::Ended, None).await;
                } else {
                    self.spawn_watch(&row, How::Resumed).await;
                }
            }
        }
        self.reminders.resume_all().await;
        // Once at start, so the host has the counts of the stored watches.
        self.push_counts().await;
    }

    // -----------------------------------------------------------------------
    // One watch
    // -----------------------------------------------------------------------

    fn target(row: &WatchRow) -> Target {
        if row.report_to == ReportTo::Chief.as_str() {
            Target::Chief
        } else {
            Target::Folder(row.folder.clone())
        }
    }

    /// Reads the counts from the store and sends them to the host when they changed. A
    /// store that cannot be read leaves the last counts in place.
    async fn push_counts(&self) {
        let _one_push = self.counts_lock.lock().await;
        let Ok(live) = self.deps.store.live().await else {
            return;
        };
        let counts = counts_of(&live);
        let changed = self.deps.counts.send_if_modified(|current| {
            if *current == counts {
                return false;
            }
            *current = counts;
            true
        });
        if changed {
            obs::watch_counts_sent(counts.running, counts.paused, counts.capped);
        }
    }

    async fn spawn_watch(self: &Arc<Self>, row: &WatchRow, how: How) {
        let ends_at = match row.ends_at_ms {
            Some(end) => local_text(end, self.offset().await),
            None => "none".to_owned(),
        };
        self.spawn_task(row);
        obs::watch_started(&obs::Started {
            watch: &row.name,
            folder: &row.folder,
            skill: &row.skill,
            ends_at: &ends_at,
            every_minutes: row.every_minutes,
            every_run: row.every_run,
            how,
            model: &self.deps.settings.model,
        });
        self.push_counts().await;
    }

    /// Gives the watch a new task and a new report slot. The old task is cancelled (a run
    /// in progress types nothing) and a report waiting in the old slot is dropped.
    fn spawn_task(self: &Arc<Self>, row: &WatchRow) -> Arc<ReportSlot> {
        let (cancel, cancelled) = signal::channel(false);
        let slot = ReportSlot::new(Arc::clone(&self.typist), Self::target(row), self.retry);
        let old = self.state().live.insert(
            row.name.clone(),
            Live {
                started_at_ms: row.started_at_ms,
                cancel,
                slot: Arc::clone(&slot),
            },
        );
        if let Some(old) = old {
            let _ = old.cancel.send(true);
            old.slot.close(None);
        }
        tokio::spawn(Arc::clone(self).watch_task(
            row.name.clone(),
            row.started_at_ms,
            cancelled,
            Arc::clone(&slot),
        ));
        slot
    }

    /// Pauses a running watch until `until` (or a resume), and gives it a task that only
    /// waits. Returns the new report slot, or `None` when the watch was not running.
    async fn hold(
        self: &Arc<Self>,
        row: &WatchRow,
        until: Option<i64>,
        how: How,
    ) -> Result<Option<Arc<ReportSlot>>, StoreError> {
        let Some(paused) = self
            .deps
            .store
            .pause(&row.name, row.started_at_ms, until, how.as_str())
            .await?
        else {
            return Ok(None);
        };
        let slot = self.spawn_task(&paused);
        let until_text = match until {
            Some(at) => Some(local_text(at, self.offset().await)),
            None => None,
        };
        obs::watch_paused(&row.name, how, until_text.as_deref());
        self.push_counts().await;
        Ok(Some(slot))
    }

    /// Stores a paused watch as running, due at once. Returns the watch, or `None` when it
    /// was not paused.
    async fn release(&self, row: &WatchRow, how: How) -> Result<Option<WatchRow>, StoreError> {
        let Some(resumed) = self
            .deps
            .store
            .resume(&row.name, row.started_at_ms, clock_ms())
            .await?
        else {
            return Ok(None);
        };
        obs::watch_resumed(&row.name, how);
        self.push_counts().await;
        Ok(Some(resumed))
    }

    async fn watch_task(
        self: Arc<Self>,
        name: String,
        started_at_ms: i64,
        mut cancelled: signal::Receiver<bool>,
        slot: Arc<ReportSlot>,
    ) {
        let mut shutdown = self.deps.shutdown.clone();
        loop {
            if *cancelled.borrow() {
                return;
            }
            let row = match self.deps.store.get(&name).await {
                Ok(Some(row)) => row,
                Ok(None) => return,
                Err(_) => {
                    // A store that fails for a short time must not end the watch.
                    tracing::warn!(
                        watch = name.as_str(),
                        reason = "store_failed",
                        "watch store read failed"
                    );
                    tokio::select! {
                        () = tokio::time::sleep(self.retry) => {}
                        () = interrupted(&mut cancelled, &mut shutdown) => return,
                    }
                    continue;
                }
            };
            // A resume or a stop can cancel this task while it reads the store.
            if *cancelled.borrow() {
                return;
            }
            if !row.is_live() || row.started_at_ms != started_at_ms {
                return;
            }
            let now = clock_ms();
            if row.ends_at_ms.is_some_and(|end| now >= end) {
                self.end(&row, EndReason::Ended, None).await;
                return;
            }
            if row.state == WatchState::Paused {
                match self
                    .wait_while_paused(&row, &mut cancelled, &mut shutdown)
                    .await
                {
                    Flow::Continue => continue,
                    Flow::Over => return,
                }
            }
            if now < row.next_run_at_ms {
                let until = row
                    .ends_at_ms
                    .map_or(row.next_run_at_ms, |end| row.next_run_at_ms.min(end));
                // Tokio's clock stops while the computer sleeps: wake often and read the
                // wall clock again.
                let wait = Duration::from_millis((until - now) as u64).min(WAKE_EVERY);
                tokio::select! {
                    () = tokio::time::sleep(wait) => {}
                    () = interrupted(&mut cancelled, &mut shutdown) => return,
                }
                continue;
            }
            match self.run(&row, &mut cancelled, &mut shutdown, &slot).await {
                Flow::Continue => {}
                Flow::Over => return,
            }
        }
    }

    /// Waits while `row` is paused: until its pause ends, or its end comes, or the task is
    /// cancelled. A pause that ends resumes the watch, and its first check runs at once.
    /// One wait is at most `WAKE_EVERY`, because tokio's clock stops while the computer
    /// sleeps. The caller then reads the row again and checks the wall clock.
    async fn wait_while_paused(
        &self,
        row: &WatchRow,
        cancelled: &mut signal::Receiver<bool>,
        shutdown: &mut signal::Receiver<()>,
    ) -> Flow {
        let wake = [row.paused_until_ms, row.ends_at_ms]
            .into_iter()
            .flatten()
            .min();
        let Some(wake) = wake else {
            // No end and no pause end: only a resume (a new task) or a stop goes on.
            interrupted(cancelled, shutdown).await;
            return Flow::Over;
        };
        let wait = Duration::from_millis((wake - clock_ms()).max(0) as u64).min(WAKE_EVERY);
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            () = interrupted(cancelled, shutdown) => return Flow::Over,
        }
        if !row.paused_until_ms.is_some_and(|until| clock_ms() >= until) {
            return Flow::Continue;
        }
        let how = if is_capped(row) {
            How::BudgetDay
        } else {
            How::Timed
        };
        if self.release(row, how).await.is_err() {
            store_failed(&row.name, false);
            // Wait before the next try, so a store that fails at once is not asked in a
            // tight loop.
            tokio::select! {
                () = tokio::time::sleep(self.retry) => {}
                () = interrupted(cancelled, shutdown) => return Flow::Over,
            }
        }
        Flow::Continue
    }

    /// One run of `row`, inside its `harold.watch.run` span.
    async fn run(
        self: &Arc<Self>,
        row: &WatchRow,
        cancelled: &mut signal::Receiver<bool>,
        shutdown: &mut signal::Receiver<()>,
        slot: &Arc<ReportSlot>,
    ) -> Flow {
        let run_no = row.runs + 1;
        let obs = RunObs::start(&row.name, &row.folder, run_no);
        let span = obs.span().clone();
        self.run_in_span(row, run_no, &obs, cancelled, shutdown, slot)
            .instrument(span)
            .await
    }

    async fn run_in_span(
        self: &Arc<Self>,
        row: &WatchRow,
        run_no: u32,
        obs: &RunObs,
        cancelled: &mut signal::Receiver<bool>,
        shutdown: &mut signal::Receiver<()>,
        slot: &Arc<ReportSlot>,
    ) -> Flow {
        let began = Instant::now();
        let began_at_ms = clock_ms();
        let budget = row
            .run_budget_usd_micros
            .or(self.deps.settings.run_budget_usd.map(to_micros));
        // One model at a time. Waiting more than a minute skips the run.
        let guard = tokio::select! {
            taken = tokio::time::timeout(self.model_wait, self.model.lock()) => taken,
            () = interrupted(cancelled, shutdown) => return Flow::Over,
        };
        let wait_ms = began.elapsed().as_millis() as u64;
        let Ok(_guard) = guard else {
            obs.skipped();
            let next = next_on_grid(row.started_at_ms, row.every_minutes, clock_ms());
            let record = RunRecord {
                started_at_ms: clock_ms(),
                outcome: "skipped".into(),
                cost_usd_micros: None,
                streak: Streak::Unchanged,
                next_run_at_ms: next,
            };
            self.record(row, &record).await;
            return Flow::Continue;
        };

        // A daily budget: every watch stops before a run that could pass it.
        if let Some(daily) = self.daily_budget_micros() {
            let (day_start, day_end) = self.today().await;
            let spent = self
                .deps
                .store
                .spent_micros(day_start, day_end)
                .await
                .unwrap_or(0);
            if spent + budget.unwrap_or(0) > daily {
                self.stop_all_for_budget(daily).await;
                return Flow::Over;
            }
        }

        let (answer, reported_cost, why, run_ms) = {
            let model = self
                .model_result(row, run_no, budget, obs, cancelled, shutdown)
                .await;
            match model {
                Some(model) => model,
                None => {
                    obs.cancelled(budget.map(|m| m as f64 / 1e6));
                    let record = RunRecord {
                        started_at_ms: began_at_ms,
                        outcome: "stopped".into(),
                        cost_usd_micros: budget,
                        streak: Streak::Unchanged,
                        next_run_at_ms: row.next_run_at_ms,
                    };
                    let _ = self
                        .deps
                        .store
                        .record_orphan_run(&row.name, &row.folder, run_no, &record)
                        .await;
                    return Flow::Over;
                }
            }
        };
        // What the run counts: its reported cost, else its run budget, else unknown.
        let cost_micros = reported_cost.map(to_micros).or(budget);
        let cost_usd = cost_micros.map(|m| m as f64 / 1_000_000.0);
        let duration_ms = began.elapsed().as_millis() as u64;
        let (outcome, streak) = match &answer {
            Ok(Answer::Quiet) => ("quiet".to_owned(), Streak::Worked),
            Ok(Answer::Report(_)) => ("report".to_owned(), Streak::Worked),
            Err(failure) => (failure.as_str().to_owned(), Streak::Failed),
        };
        let timing = Timing { wait_ms, run_ms };
        match &answer {
            Ok(_) => obs.finished(&outcome, cost_usd, duration_ms, timing),
            Err(failure @ (Failure::ProcessFailed | Failure::MaxTurnsReached)) => match &why {
                Some((exit, cause)) => {
                    obs.failed(*failure, cost_usd, timing, Some(*exit), Some(cause));
                }
                None => obs.failed(*failure, cost_usd, timing, None, None),
            },
            Err(failure) => obs.failed(*failure, cost_usd, timing, None, None),
        }
        let now = clock_ms();
        let record = RunRecord {
            started_at_ms: began_at_ms,
            outcome,
            cost_usd_micros: cost_micros,
            streak,
            next_run_at_ms: next_on_grid(row.started_at_ms, row.every_minutes, now),
        };
        let Some(updated) = self.record(row, &record).await else {
            // Stopped or replaced while it ran: nothing is typed.
            let _ = self
                .deps
                .store
                .record_orphan_run(&row.name, &row.folder, run_no, &record)
                .await;
            return Flow::Over;
        };

        let total = total_runs(row);
        match answer {
            Ok(Answer::Report(line)) => {
                slot.offer(report_line(&row.name, run_no, total, &line), obs.report())
            }
            Ok(Answer::Quiet) if row.every_run => {
                slot.offer(report_line(&row.name, run_no, total, "quiet"), obs.report());
            }
            _ => {}
        }
        if updated.failed_in_a_row >= FAILURES_THAT_STOP {
            self.end(&updated, EndReason::RunsFailing, None).await;
            return Flow::Over;
        }
        Flow::Continue
    }

    /// The skill, the facts and the model. `None` when the run was interrupted. The
    /// answer is the model's, or the failure that stopped it before or during the run.
    async fn model_result(
        &self,
        row: &WatchRow,
        run_no: u32,
        budget: Option<i64>,
        obs: &RunObs,
        cancelled: &mut signal::Receiver<bool>,
        shutdown: &mut signal::Receiver<()>,
    ) -> Option<ModelOutcome> {
        let folder = std::path::Path::new(&row.folder);
        let Ok(skill) = read_skill(folder, &row.skill, &self.deps.settings.skill_folders) else {
            return Some((Err(Failure::SkillMissing), None, None, 0));
        };
        obs.set_add_dirs(skill.add_dirs.len());
        if skill.tools_unread {
            obs.tools_not_read();
        }
        let sources = Sources {
            agents: &self.deps.agents,
            pods: &self.deps.pods,
            chief_pane: &self.deps.chief_pane,
            offset: self.offset().await,
        };
        let now = clock_ms();
        let facts = facts::gather(
            &sources,
            now,
            WatchEntry {
                run: run_no,
                runs_left: total_runs(row).map(|total| total.saturating_sub(run_no)),
                previous_outcome: row.last_outcome.clone(),
            },
        )
        .await;
        let job = ModelJob {
            folder: folder.to_path_buf(),
            model: self.deps.settings.model.clone(),
            system_prompt: answer::system_prompt(&skill.body),
            allowed_tools: skill.allowed_tools,
            add_dirs: skill.add_dirs,
            run_budget_usd_micros: budget,
            facts_json: serde_json::to_vec(&facts).unwrap_or_default(),
            timeout: Duration::from_secs(u64::from(row.run_limit_secs)),
        };
        let model_span = obs.model_span();
        let process_began = Instant::now();
        let result = tokio::select! {
            result = self.deps.runner.run(&job).instrument(model_span.clone()) => result,
            () = interrupted(cancelled, shutdown) => return None,
        };
        let run_ms = process_began.elapsed().as_millis() as u64;
        model_span.record(
            "result",
            match &result {
                ModelResult::Exited { .. } => "exited",
                ModelResult::Killed => "killed",
                ModelResult::FailedToStart => "failed_to_start",
            },
        );
        Some(match result {
            ModelResult::FailedToStart => (Err(Failure::ProcessFailed), None, None, run_ms),
            ModelResult::Killed => (Err(Failure::Timeout), None, None, run_ms),
            ModelResult::Exited { stdout, ok, exit } => {
                let read = answer::read_output(&stdout, ok);
                let why = read.cause.map(|cause| (exit, cause));
                (read.answer, read.cost_usd, why, run_ms)
            }
        })
    }

    /// Records a run in the store, logging a store failure once. `None` when the watch is
    /// no longer running.
    async fn record(&self, row: &WatchRow, record: &RunRecord) -> Option<WatchRow> {
        match self
            .deps
            .store
            .record_run(&row.name, row.started_at_ms, record)
            .await
        {
            Ok(updated) => updated,
            Err(_) => {
                tracing::warn!(
                    watch = row.name.as_str(),
                    reason = "store_failed",
                    "watch run not recorded"
                );
                None
            }
        }
    }

    // -----------------------------------------------------------------------
    // Ending
    // -----------------------------------------------------------------------

    /// Ends a running or paused watch: stores why, cancels its task, and types its closing
    /// line. `Stopped` types no closing line.
    async fn end(self: &Arc<Self>, row: &WatchRow, reason: EndReason, daily: Option<i64>) {
        let ended = match self
            .deps
            .store
            .end(&row.name, row.started_at_ms, reason.as_str(), clock_ms())
            .await
        {
            Ok(Some(ended)) => ended,
            _ => return,
        };
        self.push_counts().await;
        let live = {
            let mut state = self.state();
            match state.live.get(&row.name) {
                Some(live) if live.started_at_ms == row.started_at_ms => {
                    state.live.remove(&row.name)
                }
                _ => None,
            }
        };
        if let Some(live) = &live {
            let _ = live.cancel.send(true);
        }
        obs::watch_stopped(
            &ended.name,
            reason,
            ended.runs,
            ended.cost_usd_micros as f64 / 1_000_000.0,
        );
        // A watch that ran out while Harold was down has no task: its closing line still
        // goes out, through a slot of its own.
        let slot = match live {
            Some(live) => live.slot,
            None if reason == EndReason::Stopped => return,
            None => ReportSlot::new(Arc::clone(&self.typist), Self::target(row), self.retry),
        };
        let closing = match reason {
            EndReason::Stopped => None,
            EndReason::Ended => {
                let skipped = self
                    .deps
                    .store
                    .skipped_runs(&ended.name, ended.started_at_ms)
                    .await
                    .unwrap_or(0)
                    .min(ended.runs);
                let skipped_note = if skipped > 0 {
                    format!("{skipped} skipped, ")
                } else {
                    String::new()
                };
                Some(format!(
                    "WATCH {} ended: {}, {skipped_note}{}, {}",
                    ended.name,
                    counted(ended.runs - skipped, "run", "runs"),
                    counted(ended.reports, "report", "reports"),
                    dollars(ended.cost_usd_micros)
                ))
            }
            EndReason::RunsFailing => Some(format!(
                "WATCH {} stopped: {} runs in a row failed ({})",
                ended.name,
                FAILURES_THAT_STOP,
                ended.last_outcome.as_deref().unwrap_or("unknown")
            )),
            EndReason::DailyBudgetSpent => Some(format!(
                "WATCH {} stopped: daily budget of {} spent",
                ended.name,
                dollars(daily.or(self.daily_budget_micros()).unwrap_or(0))
            )),
        };
        slot.close(closing.map(|line| (line, obs::ReportObs::new(&ended.name, ended.runs))));
    }

    /// The daily budget would be passed. Every running watch with an end stops with its
    /// own closing line. Every running watch with no end becomes a capped watch: paused
    /// until the next local day, with one line that tells so.
    async fn stop_all_for_budget(self: &Arc<Self>, daily: i64) {
        let Ok(running) = self.deps.store.running().await else {
            return;
        };
        let (_, next_day) = self.today().await;
        let resumes_at = local_text(next_day, self.offset().await);
        for row in running {
            if row.ends_at_ms.is_some() {
                self.end(&row, EndReason::DailyBudgetSpent, Some(daily))
                    .await;
                continue;
            }
            let slot = match self.hold(&row, Some(next_day), How::BudgetDay).await {
                Ok(Some(slot)) => slot,
                Ok(None) => continue,
                Err(_) => {
                    store_failed(&row.name, true);
                    continue;
                }
            };
            let line = format!(
                "WATCH {} paused: daily budget of {} spent, resumes at {}",
                row.name,
                dollars(daily),
                clock_of(&resumes_at)
            );
            slot.offer(line, obs::ReportObs::new(&row.name, row.runs));
        }
    }
}

/// A run's report line. A watch with no end shows no total.
fn report_line(name: &str, run: u32, total: Option<u32>, text: &str) -> String {
    match total {
        Some(total) => format!("WATCH {name} {run}/{total}: {text}"),
        None => format!("WATCH {name} {run}: {text}"),
    }
}

/// Logs a store error that left the watch `name` as it was: not paused (`paused` false:
/// not resumed).
fn store_failed(name: &str, paused: bool) {
    if paused {
        tracing::warn!(watch = name, reason = "store_failed", "watch not paused");
    } else {
        tracing::warn!(watch = name, reason = "store_failed", "watch not resumed");
    }
}

/// Completes when the watch or reminder is cancelled or Harold shuts down.
pub(super) async fn interrupted(
    cancelled: &mut signal::Receiver<bool>,
    shutdown: &mut signal::Receiver<()>,
) {
    tokio::select! {
        _ = cancelled.wait_for(|cancelled| *cancelled) => {}
        // A change or a dropped sender both mean shutdown.
        _ = shutdown.changed() => {}
    }
}

/// Runs until Harold shuts down. The watches' own tasks end on the same signal.
pub(super) async fn run(scheduler: Arc<Scheduler>, mut shutdown: signal::Receiver<()>) {
    scheduler.resume().await;
    let _ = shutdown.changed().await;
}

#[cfg(test)]
#[path = "scheduler_tests.rs"]
pub(super) mod tests;

#[cfg(test)]
#[path = "observability_tests.rs"]
mod observability_tests;
