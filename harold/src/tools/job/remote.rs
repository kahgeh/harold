//! The master's side of the jobs of slaves. The master reads each slave's last jobs from
//! the feed, which the slave link fills. When a slave's job has ended, the master counts
//! the end, logs it and types the end line into the agent's pane on this machine. It
//! records each step in the `told` table of `jobs.db`, so a restart or a repeated snapshot
//! does not do a step twice. A slave counts and types nothing for its own jobs.

use super::check::{Checker, end_line_text_pid_on, end_line_text_unit};
use super::obs;
use super::store::{SHOW_AFTER_END_MS, ToldKey};
use crate::agent::pane_route::parse_host_pane_id;
use crate::harold::AgentJob;

/// How long a slave's job stays `unknown` before `job host not reached` is logged.
pub(super) const UNREACHED_AFTER_MS: i64 = 30 * 60_000;

/// The end line of a job whose agent pane is on a Rex host: the master cannot type there.
const REMOTE_PANE: &str = "remote_pane";

#[allow(dead_code)]
fn key_of(job: &AgentJob) -> ToldKey {
    ToldKey {
        host: job.host.clone(),
        name: job.name.clone(),
        started_at_ms: job.started_at_ms,
    }
}

impl Checker {
    /// Handles the jobs of the slaves. Returns whether an end line was done.
    pub(super) async fn pass_slaves(&self, now: i64) -> bool {
        let mut changed = false;
        for slave in self.feed.slave_jobs() {
            for job in &slave.jobs {
                match job.state.as_str() {
                    "ended" => changed |= self.slave_job_ended(job, now).await,
                    "running" => {
                        if let Some(closed) = slave.closed_at_ms
                            && now - closed >= UNREACHED_AFTER_MS
                        {
                            self.log_unreached(job, closed).await;
                        }
                    }
                    _ => {}
                }
            }
        }
        self.log_slave_untold(now).await;
        changed
    }

    async fn slave_job_ended(&self, job: &AgentJob, now: i64) -> bool {
        let key = key_of(job);
        let Ok(told) = self.store.told(&key).await else {
            return false;
        };
        if told.as_ref().is_none_or(|told| told.ended_seen_ms == 0) {
            // The first time the master sees this end: the log line comes once.
            if !matches!(self.store.told_seen_end(&key, now).await, Ok(true)) {
                return false;
            }
            obs::slave_job_ended(
                job,
                (job.ended_at_ms - job.started_at_ms) / 1_000,
                (job.ended_at_ms - job.due_at_ms).max(0) / 1_000,
            );
        } else if told.is_some_and(|told| told.end_line.is_some()) {
            return false;
        }
        let ran_ms = job.ended_at_ms - job.started_at_ms;
        let line = if job.unit.is_empty() {
            end_line_text_pid_on(&job.name, &job.host, job.pid, ran_ms)
        } else {
            end_line_text_unit(
                &job.name,
                &job.host,
                ran_ms,
                job.exit_known.then_some(job.exit_code),
            )
        };
        let memory_key = (key.host.clone(), key.name.clone(), key.started_at_ms);
        let outcome = if parse_host_pane_id(&job.pane_id).is_some() {
            REMOTE_PANE
        } else {
            let waited = self
                .memory
                .lock()
                .is_ok_and(|memory| memory.waited_slave.contains(&memory_key));
            match self.type_outcome(&job.pane_id, &line, waited).await {
                Some(outcome) => outcome,
                None => {
                    if let Ok(mut memory) = self.memory.lock() {
                        memory.waited_slave.insert(memory_key);
                    }
                    return false;
                }
            }
        };
        if self.store.told_set_line(&key, outcome).await.is_err() {
            return false;
        }
        crate::telemetry::count_job_end_on("ended", outcome, &job.host);
        if let Ok(mut memory) = self.memory.lock() {
            memory.waited_slave.remove(&memory_key);
        }
        true
    }

    /// Logs `job host not reached` once for each job and outage. The record keeps the time
    /// that the link closed, so a restart does not log the same outage again.
    async fn log_unreached(&self, job: &AgentJob, closed_at_ms: i64) {
        let key = key_of(job);
        let Ok(told) = self.store.told(&key).await else {
            return;
        };
        if told.is_some_and(|told| told.unreached_mark_ms == closed_at_ms) {
            return;
        }
        if self
            .store
            .told_set_unreached(&key, closed_at_ms)
            .await
            .is_ok()
        {
            obs::host_not_reached(job);
        }
    }

    /// Logs `job end not told` once for each slave job whose line is `no_pane` or `failed`,
    /// 10 to 20 minutes after the master saw the end.
    async fn log_slave_untold(&self, now: i64) {
        let Ok(found) = self
            .store
            .told_untold(now, SHOW_AFTER_END_MS, 2 * SHOW_AFTER_END_MS)
            .await
        else {
            return;
        };
        for (key, reason) in found {
            let memory_key = (key.host.clone(), key.name.clone(), key.started_at_ms);
            let first = self
                .memory
                .lock()
                .is_ok_and(|mut memory| memory.logged_slave.insert(memory_key));
            if first {
                obs::slave_end_not_told(&key.host, &key.name, &reason);
            }
        }
    }
}

#[cfg(test)]
#[path = "remote_tests.rs"]
mod tests;
