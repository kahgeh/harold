//! The check of the running jobs, every 15 seconds, and the end line.
//!
//! A running job has ended when its pid has no process, or the process has another start
//! time. Harold cannot read the exit code of a process that it did not start, so the end
//! line gives none.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::awake::KeepAwake;
use super::feed::JobsFeed;
use super::obs;
use super::process::ProcessPort;
use super::service::Clock;
use super::store::{JobRow, JobStore, SHOW_AFTER_END_MS};
use super::unit::UnitPort;
use crate::tools::{PaneTyping, Typing};

/// How often the check runs.
pub(super) const PERIOD: Duration = Duration::from_secs(15);

/// The most characters of the job name in the end line.
const LINE_NAME_MAX: usize = 48;

/// What one pass found.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct Report {
    /// The running jobs past their expected end.
    pub(super) past_due: u32,
    /// A job ended, or an end line was done: the snapshot needs a new version.
    pub(super) changed: bool,
}

/// What the check remembers between passes in this run of Harold.
#[derive(Default)]
struct Memory {
    /// Jobs whose line met a busy pane at least once.
    waited: HashSet<i64>,
    /// Jobs whose `job end not told` line is logged.
    logged: HashSet<i64>,
}

/// What the check found for one running job.
enum Still {
    Runs,
    /// Ended, with the exit code when it is known (a unit only).
    Ended(Option<i32>),
}

pub(super) struct Checker {
    store: Arc<JobStore>,
    processes: Arc<dyn ProcessPort>,
    units: Arc<dyn UnitPort>,
    typing: Arc<dyn PaneTyping>,
    now_ms: Clock,
    feed: Arc<JobsFeed>,
    memory: Mutex<Memory>,
    /// The keep-awake files. `None` when `[job] keep_awake_folder` is empty.
    awake: Option<Arc<KeepAwake>>,
}

/// `JOB <name> ended after <n> m: exit 0.` for a unit. The text is `failed (exit 2)` for a
/// non-zero code and `exit unknown` when the code is not known. A host adds ` on <host>`.
pub(super) fn end_line_text_unit(
    name: &str,
    host: &str,
    ran_ms: i64,
    exit_code: Option<i32>,
) -> String {
    let name: String = name.chars().take(LINE_NAME_MAX).collect();
    let on = if host.is_empty() {
        String::new()
    } else {
        format!(" on {host}")
    };
    let minutes = ran_ms / 60_000;
    let ran = if minutes == 0 {
        "<1 m".to_string()
    } else {
        format!("{minutes} m")
    };
    let exit = match exit_code {
        Some(0) => "exit 0".to_string(),
        Some(code) => format!("failed (exit {code})"),
        None => "exit unknown".to_string(),
    };
    format!("JOB {name}{on} ended after {ran}: {exit}.")
}

/// `JOB <name> ended after <n> m (pid <pid>). Check its output.` The name is cut to 48
/// characters. A run under 1 minute reads `<1 m`.
pub(super) fn end_line_text(name: &str, pid: u32, ran_ms: i64) -> String {
    let name: String = name.chars().take(LINE_NAME_MAX).collect();
    let minutes = ran_ms / 60_000;
    let ran = if minutes == 0 {
        "<1 m".to_string()
    } else {
        format!("{minutes} m")
    };
    format!("JOB {name} ended after {ran} (pid {pid}). Check its output.")
}

impl Checker {
    pub(super) fn new(
        store: Arc<JobStore>,
        processes: Arc<dyn ProcessPort>,
        units: Arc<dyn UnitPort>,
        typing: Arc<dyn PaneTyping>,
        now_ms: Clock,
        feed: Arc<JobsFeed>,
    ) -> Self {
        Self {
            store,
            processes,
            units,
            typing,
            now_ms,
            feed,
            memory: Mutex::new(Memory::default()),
            awake: None,
        }
    }

    /// Writes a keep-awake file for each job that runs, and deletes it at the end.
    pub(super) fn with_awake(mut self, awake: Option<Arc<KeepAwake>>) -> Self {
        self.awake = awake;
        self
    }

    /// One check: ends the jobs whose process is gone, types the lines that wait, logs the
    /// ends that nobody can be told of, and counts the jobs past their expected end.
    pub(super) async fn pass(&self) -> Report {
        let now = (self.now_ms)();
        let mut report = Report::default();
        let Ok(running) = self.store.running().await else {
            return report;
        };
        let mut still_running: Vec<String> = Vec::new();
        for job in running {
            let exit_code = match self.still_runs(&job).await {
                Still::Runs => {
                    if now > job.due_at_ms {
                        report.past_due += 1;
                    }
                    still_running.push(job.name.clone());
                    continue;
                }
                Still::Ended(exit_code) => exit_code,
            };
            if matches!(
                self.store.mark_ended_with(job.id, now, exit_code).await,
                Ok(true)
            ) {
                report.changed = true;
                obs::job_ended(
                    &job,
                    (now - job.started_at_ms) / 1_000,
                    (now - job.due_at_ms).max(0) / 1_000,
                );
            }
        }
        if let Some(awake) = &self.awake {
            awake.tick(&still_running, now);
        }
        if let Ok(waiting) = self.store.waiting_for_line().await {
            for job in waiting {
                report.changed |= self.tell(&job).await;
            }
        }
        self.log_untold(now).await;
        let _ = self.store.prune(now).await;
        self.feed.refresh(&self.store, now).await;
        report
    }

    /// Whether the job's process or unit still runs. A `systemctl` that fails is not an end.
    async fn still_runs(&self, job: &JobRow) -> Still {
        if job.unit.is_empty() {
            return if self.processes.start_time(job.pid) == Some(job.process_start_s) {
                Still::Runs
            } else {
                Still::Ended(None)
            };
        }
        let Ok(state) = self.units.show(&job.unit, job.user_unit).await else {
            return Still::Runs;
        };
        if state.is_running() {
            return Still::Runs;
        }
        let exit_code = state.exit_code();
        if state.exists() {
            // The ended unit stays loaded: stop it once so that it does not stay.
            self.units.stop(&job.unit, job.user_unit).await;
        }
        Still::Ended(exit_code)
    }

    /// Tries once to type the end line. Returns whether an outcome was recorded.
    async fn tell(&self, job: &JobRow) -> bool {
        let ran_ms = job.ended_at_ms - job.started_at_ms;
        let line = if job.unit.is_empty() {
            end_line_text(&job.name, job.pid, ran_ms)
        } else {
            end_line_text_unit(&job.name, "", ran_ms, job.exit_code)
        };
        let outcome = match self.typing.type_line(&job.pane_id, &line).await {
            Typing::Typed { .. } => {
                let waited = self
                    .memory
                    .lock()
                    .is_ok_and(|memory| memory.waited.contains(&job.id));
                if waited { "typed_at_turn_end" } else { "typed" }
            }
            Typing::NoPane => "no_pane",
            // The text is in the input box and Enter failed: typing again would repeat it.
            Typing::EnterFailed => "failed",
            Typing::PaneBusy(_) => {
                if let Ok(mut memory) = self.memory.lock() {
                    memory.waited.insert(job.id);
                }
                return false;
            }
        };
        if self.store.set_end_line(job.id, outcome).await.is_err() {
            return false;
        }
        crate::telemetry::count_job_end("ended", outcome);
        if let Ok(mut memory) = self.memory.lock() {
            memory.waited.remove(&job.id);
        }
        true
    }

    /// Logs `job end not told` once for each job that ended at least 10 minutes ago and
    /// whose line is `no_pane` or `failed`. A busy pane may still get its line, so it logs
    /// nothing. The check looks at the jobs that ended 10 to 20 minutes ago.
    async fn log_untold(&self, now: i64) {
        let Ok(recent) = self.store.shown(now - SHOW_AFTER_END_MS).await else {
            return;
        };
        for job in recent {
            let Some(reason) = job
                .end_line
                .as_deref()
                .filter(|outcome| matches!(*outcome, "no_pane" | "failed"))
            else {
                continue;
            };
            let age = now - job.ended_at_ms;
            if !(SHOW_AFTER_END_MS..2 * SHOW_AFTER_END_MS).contains(&age) {
                continue;
            }
            let first = self
                .memory
                .lock()
                .is_ok_and(|mut memory| memory.logged.insert(job.id));
            if first {
                obs::end_not_told(&job, reason);
            }
        }
    }
}

#[cfg(test)]
#[path = "check_tests.rs"]
mod tests;
