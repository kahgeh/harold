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
use super::clock::{clock_ms, day_bounds, local_text};
use super::facts::{self, Sources, WatchEntry};
use super::obs::{self, EndReason, How, RunObs, Timing};
use super::report::{ReportSlot, Target, Typist};
use super::runner::{Exit, ModelJob, ModelResult, ModelRunner};
use super::store::{NewWatch, RunRecord, StoreError, Streak, WatchRow, WatchStore};
use super::validate::{Checked, ReportTo, read_skill, to_micros};
use crate::settings::WatchSettings;
use crate::tools::{Agents, ChiefPane, PaneTyping, Pods};

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
    pub offset: UtcOffset,
    pub shutdown: signal::Receiver<()>,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum StartRefusal {
    TooManyWatches,
    DailyBudgetSpent,
    Unavailable,
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
    /// One model at a time across all watches.
    model: tokio::sync::Mutex<()>,
    state: Mutex<State>,
    /// One `start` at a time, so the limit and a replacement are checked and stored together.
    start_lock: tokio::sync::Mutex<()>,
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

/// How many runs the watch's time holds: `--for` over `--every`, rounded up.
pub(super) fn total_runs(row: &WatchRow) -> u32 {
    let every = i64::from(row.every_minutes) * MINUTE_MS;
    let span = row.ends_at_ms - row.started_at_ms;
    ((span + every - 1) / every).max(1) as u32
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
        Arc::new(Self {
            deps,
            typist,
            model: tokio::sync::Mutex::new(()),
            state: Mutex::default(),
            start_lock: tokio::sync::Mutex::new(()),
            model_wait: MODEL_WAIT,
            retry: RETRY,
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn today(&self) -> (i64, i64) {
        day_bounds(clock_ms(), self.deps.offset)
    }

    fn daily_budget_micros(&self) -> Option<i64> {
        self.deps.settings.daily_budget_usd.map(to_micros)
    }

    // -----------------------------------------------------------------------
    // Commands
    // -----------------------------------------------------------------------

    /// Starts a watch, or replaces a running one of the same name. Returns when it ends.
    pub(super) async fn start(self: &Arc<Self>, checked: Checked) -> Result<i64, StartRefusal> {
        let _one_start = self.start_lock.lock().await;
        let store = &self.deps.store;
        let running = store
            .running()
            .await
            .map_err(|_| StartRefusal::Unavailable)?;
        let replaced = running.iter().any(|row| row.name == checked.name);
        let others = running
            .iter()
            .filter(|row| row.name != checked.name)
            .count();
        if others >= MAX_WATCHES {
            return Err(StartRefusal::TooManyWatches);
        }
        let now = clock_ms();
        let (day_start, day_end) = day_bounds(now, self.deps.offset);
        if let Some(daily) = self.daily_budget_micros() {
            let spent = store
                .spent_micros(day_start, day_end)
                .await
                .map_err(|_| StartRefusal::Unavailable)?;
            let stopped = store
                .budget_stopped_since(day_start)
                .await
                .map_err(|_| StartRefusal::Unavailable)?;
            if spent >= daily || stopped {
                return Err(StartRefusal::DailyBudgetSpent);
            }
        }
        let ends_at_ms = now + i64::from(checked.for_minutes) * MINUTE_MS;
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
        );
        Ok(ends_at_ms)
    }

    /// Ends a running watch at once. A run in progress is stopped, its report is not
    /// typed, and no closing line is typed.
    pub(super) async fn stop(self: &Arc<Self>, name: &str) -> Result<(), StopRefusal> {
        let row = self
            .deps
            .store
            .get(name)
            .await
            .map_err(|_| StopRefusal::Unavailable)?
            .ok_or(StopRefusal::UnknownWatch)?;
        if !row.running {
            return Err(StopRefusal::NotRunning);
        }
        self.end(&row, EndReason::Stopped, None).await;
        Ok(())
    }

    /// Every running watch, and every watch that ended today.
    pub(super) async fn list(&self) -> Result<Listing, StoreError> {
        let (day_start, day_end) = self.today();
        let mut watches = self.deps.store.running().await?;
        watches.extend(self.deps.store.ended_since(day_start).await?);
        let spent_usd_micros = self.deps.store.spent_micros(day_start, day_end).await?;
        Ok(Listing {
            watches,
            spent_usd_micros,
        })
    }

    /// Resumes every watch that was running when Harold stopped. One whose time ran out
    /// meanwhile ends now.
    pub(super) async fn resume(self: &Arc<Self>) {
        let Ok(running) = self.deps.store.running().await else {
            return;
        };
        for row in running {
            if clock_ms() >= row.ends_at_ms {
                self.end(&row, EndReason::Ended, None).await;
            } else {
                self.spawn_watch(&row, How::Resumed);
            }
        }
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

    fn spawn_watch(self: &Arc<Self>, row: &WatchRow, how: How) {
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
        obs::watch_started(&obs::Started {
            watch: &row.name,
            folder: &row.folder,
            skill: &row.skill,
            ends_at: &local_text(row.ends_at_ms, self.deps.offset),
            every_minutes: row.every_minutes,
            every_run: row.every_run,
            how,
            model: &self.deps.settings.model,
        });
        tokio::spawn(Arc::clone(self).watch_task(
            row.name.clone(),
            row.started_at_ms,
            cancelled,
            slot,
        ));
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
            let Ok(Some(row)) = self.deps.store.get(&name).await else {
                return;
            };
            if !row.running || row.started_at_ms != started_at_ms {
                return;
            }
            let now = clock_ms();
            if now >= row.ends_at_ms {
                self.end(&row, EndReason::Ended, None).await;
                return;
            }
            if now < row.next_run_at_ms {
                let until = row.next_run_at_ms.min(row.ends_at_ms);
                let wait = Duration::from_millis((until - now) as u64);
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
            let (day_start, day_end) = self.today();
            let spent = self
                .deps
                .store
                .spent_micros(day_start, day_end)
                .await
                .unwrap_or(0);
            if spent + budget.unwrap_or(0) > daily {
                self.stop_all_for_budget().await;
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
            offset: self.deps.offset,
        };
        let now = clock_ms();
        let total = total_runs(row);
        let facts = facts::gather(
            &sources,
            now,
            WatchEntry {
                run: run_no,
                runs_left: total.saturating_sub(run_no),
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

    /// Ends a running watch: stores why, cancels its task, and types its closing line.
    /// `Stopped` types no closing line.
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

    /// The daily budget would be passed: every running watch stops with its own closing
    /// line.
    async fn stop_all_for_budget(self: &Arc<Self>) {
        let Ok(running) = self.deps.store.running().await else {
            return;
        };
        for row in running {
            self.end(&row, EndReason::DailyBudgetSpent, None).await;
        }
    }
}

fn report_line(name: &str, run: u32, total: u32, text: &str) -> String {
    format!("WATCH {name} {run}/{total}: {text}")
}

/// Completes when the watch is cancelled or Harold shuts down.
async fn interrupted(cancelled: &mut signal::Receiver<bool>, shutdown: &mut signal::Receiver<()>) {
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
