//! The check of the running jobs, every 15 seconds, and the end line.
//!
//! A running job has ended when its pid has no process, or the process has another start
//! time. Harold cannot read the exit code of a process that it did not start, so the end
//! line gives none.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::obs;
use super::process::ProcessPort;
use super::service::Clock;
use super::store::{JobRow, JobStore, SHOW_AFTER_END_MS};
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

pub(super) struct Checker {
    store: Arc<JobStore>,
    processes: Arc<dyn ProcessPort>,
    typing: Arc<dyn PaneTyping>,
    now_ms: Clock,
    memory: Mutex<Memory>,
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
        typing: Arc<dyn PaneTyping>,
        now_ms: Clock,
    ) -> Self {
        Self {
            store,
            processes,
            typing,
            now_ms,
            memory: Mutex::new(Memory::default()),
        }
    }

    /// One check: ends the jobs whose process is gone, types the lines that wait, logs the
    /// ends that nobody can be told of, and counts the jobs past their expected end.
    pub(super) async fn pass(&self) -> Report {
        let now = (self.now_ms)();
        let mut report = Report::default();
        let Ok(running) = self.store.running().await else {
            return report;
        };
        for job in running {
            if self.processes.start_time(job.pid) == Some(job.process_start_s) {
                if now > job.due_at_ms {
                    report.past_due += 1;
                }
                continue;
            }
            if matches!(self.store.mark_ended(job.id, now).await, Ok(true)) {
                report.changed = true;
                obs::job_ended(
                    &job,
                    (now - job.started_at_ms) / 1_000,
                    (now - job.due_at_ms).max(0) / 1_000,
                );
            }
        }
        if let Ok(waiting) = self.store.waiting_for_line().await {
            for job in waiting {
                report.changed |= self.tell(&job).await;
            }
        }
        self.log_untold(now).await;
        let _ = self.store.prune(now).await;
        report
    }

    /// Tries once to type the end line. Returns whether an outcome was recorded.
    async fn tell(&self, job: &JobRow) -> bool {
        let line = end_line_text(&job.name, job.pid, job.ended_at_ms - job.started_at_ms);
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
