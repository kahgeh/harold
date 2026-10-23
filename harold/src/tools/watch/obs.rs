//! Every span, counter and log line of the watch tool. The scheduler calls only these
//! functions, so no other log site exists. None takes the skill's text, its
//! `allowed-tools`, its `add-dirs` folders (only their number), the facts, a command's output, the answer or a report: nothing here
//! could carry them.

use std::sync::atomic::{AtomicU32, Ordering};

use opentelemetry::KeyValue;
use tracing::field::Empty;
use tracing::{Span, info, info_span, warn};

use super::answer::{Cause, Failure};
use super::runner::Exit;
use crate::telemetry::{self, TRACE_TARGET};

/// How a watch started: the `how` of `watch started`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum How {
    Started,
    Replaced,
    Resumed,
}

impl How {
    fn as_str(self) -> &'static str {
        match self {
            How::Started => "started",
            How::Replaced => "replaced",
            How::Resumed => "resumed",
        }
    }
}

/// Why a watch ended: the `reason` of `watch stopped`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EndReason {
    Ended,
    Stopped,
    DailyBudgetSpent,
    RunsFailing,
}

impl EndReason {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            EndReason::Ended => "ended",
            EndReason::Stopped => "stopped",
            EndReason::DailyBudgetSpent => "daily_budget_spent",
            EndReason::RunsFailing => "runs_failing",
        }
    }
}

/// Why a waiting report left: the `why` of `watch report replaced`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Why {
    NewerReport,
    WatchEnded,
}

impl Why {
    fn as_str(self) -> &'static str {
        match self {
            Why::NewerReport => "newer_report",
            Why::WatchEnded => "watch_ended",
        }
    }
}

pub(super) struct Started<'a> {
    pub watch: &'a str,
    pub folder: &'a str,
    pub skill: &'a str,
    pub ends_at: &'a str,
    pub every_minutes: u32,
    pub every_run: bool,
    pub how: How,
    pub model: &'a str,
}

/// `watch started`, and the counters' zero for the folder.
pub(super) fn watch_started(started: &Started<'_>) {
    telemetry::register_watch_counters(started.folder);
    info!(
        watch = started.watch,
        folder = started.folder,
        skill = started.skill,
        ends_at = started.ends_at,
        every_minutes = started.every_minutes,
        every_run = started.every_run,
        how = started.how.as_str(),
        model = started.model,
        "watch started"
    );
}

/// `watch stopped`.
pub(super) fn watch_stopped(watch: &str, reason: EndReason, runs: u32, cost_usd: f64) {
    info!(
        watch,
        reason = reason.as_str(),
        runs,
        cost_usd,
        "watch stopped"
    );
}

/// One run: its `harold.watch.run` span and the lines written inside it.
pub(super) struct RunObs {
    span: Span,
    watch: String,
    folder: String,
    run: u32,
    /// How many folders the skill's `add-dirs` gave the run; never the paths.
    add_dirs: AtomicU32,
}

impl RunObs {
    pub(super) fn start(watch: &str, folder: &str, run: u32) -> Self {
        let span = info_span!(
            target: TRACE_TARGET,
            "harold.watch.run",
            watch,
            run,
            outcome = Empty,
            reason = Empty,
            cost_usd = Empty,
        );
        Self {
            span,
            watch: watch.to_owned(),
            folder: folder.to_owned(),
            run,
            add_dirs: AtomicU32::new(0),
        }
    }

    /// The number of `add-dirs` folders of the skill the run read. Written to
    /// `harold.log` on the run's last line only.
    pub(super) fn set_add_dirs(&self, count: usize) {
        self.add_dirs
            .store(u32::try_from(count).unwrap_or(u32::MAX), Ordering::Relaxed);
    }

    pub(super) fn span(&self) -> &Span {
        &self.span
    }

    /// The span of the Claude Code process.
    pub(super) fn model_span(&self) -> Span {
        info_span!(
            target: TRACE_TARGET,
            parent: &self.span,
            "harold.watch.model",
            watch = self.watch.as_str(),
            run = self.run,
            result = Empty,
        )
    }

    /// A skill whose `allowed-tools` is a YAML list: the run goes on with no listed tools.
    pub(super) fn tools_not_read(&self) {
        self.span.in_scope(|| {
            warn!(
                watch = self.watch.as_str(),
                reason = "yaml_list",
                "watch skill tools not read"
            );
        });
    }

    /// A run that could not take the model within 60 seconds.
    pub(super) fn skipped(&self) {
        self.span.record("outcome", "skipped");
        self.span.record("reason", "model_busy");
        self.span.in_scope(|| {
            warn!(
                watch = self.watch.as_str(),
                run = self.run,
                reason = "model_busy",
                "watch run skipped"
            );
        });
        self.count_run("skipped");
    }

    /// A run that gave `quiet` or `report`.
    pub(super) fn finished(&self, outcome: &str, cost_usd: Option<f64>, duration_ms: u64) {
        self.span.record("outcome", outcome);
        self.record_cost(cost_usd);
        self.span.in_scope(|| {
            info!(
                watch = self.watch.as_str(),
                run = self.run,
                outcome,
                cost_usd,
                duration_ms,
                add_dirs = self.add_dirs.load(Ordering::Relaxed),
                "watch run finished"
            );
        });
        self.count_run(outcome);
    }

    /// A failed run. `cost_usd` is what the run is counted at. `exit` and `cause` are
    /// given for a `process_failed` or `max_turns_reached` run that exited; the fields not known are left out.
    pub(super) fn failed(
        &self,
        reason: Failure,
        cost_usd: Option<f64>,
        exit: Option<Exit>,
        cause: Option<&Cause>,
    ) {
        let reason = reason.as_str();
        self.span.record("outcome", reason);
        self.span.record("reason", reason);
        self.record_cost(cost_usd);
        self.span.in_scope(|| {
            warn!(
                watch = self.watch.as_str(),
                run = self.run,
                reason,
                cost_usd,
                add_dirs = self.add_dirs.load(Ordering::Relaxed),
                exit_code = exit.and_then(|e| e.code),
                signal = exit.and_then(|e| e.signal),
                subtype = cause.and_then(|c| c.subtype.as_deref()),
                is_error = cause.and_then(|c| c.is_error),
                num_turns = cause.and_then(|c| c.num_turns),
                terminal_reason = cause.and_then(|c| c.terminal_reason.as_deref()),
                api_error_status = cause.and_then(|c| c.api_error_status),
                "watch run failed"
            );
        });
        self.count_run(reason);
    }

    /// A run cut short by `stop`, a replacement, or the budget. Counted at its run budget
    /// when it has one.
    pub(super) fn cancelled(&self, cost_usd: Option<f64>) {
        self.span.record("outcome", "stopped");
        self.record_cost(cost_usd);
    }

    fn record_cost(&self, cost_usd: Option<f64>) {
        if let Some(cost) = cost_usd {
            self.span.record("cost_usd", cost);
            telemetry::instruments()
                .watch_cost_usd
                .add(cost, &[KeyValue::new("folder", self.folder.clone())]);
        }
    }

    fn count_run(&self, outcome: &str) {
        telemetry::instruments().watch_runs.add(
            1,
            &[
                KeyValue::new("folder", self.folder.clone()),
                KeyValue::new("outcome", outcome.to_owned()),
            ],
        );
    }

    /// The report this run made, for the lines about it.
    pub(super) fn report(&self) -> ReportObs {
        ReportObs::new(&self.watch, self.run)
    }
}

/// The log lines of one report, from its creation to typed, replaced or dropped.
pub(super) struct ReportObs {
    watch: String,
    run: u32,
    last_reason: Option<String>,
}

impl ReportObs {
    /// The report of run `run`, or of a closing line (`run` is then the last run).
    pub(super) fn new(watch: &str, run: u32) -> Self {
        Self {
            watch: watch.to_owned(),
            run,
            last_reason: None,
        }
    }

    /// `watch report waiting`, logged when the reason changes.
    pub(super) fn waiting(&mut self, reason: &str) {
        if self.last_reason.as_deref() == Some(reason) {
            return;
        }
        self.last_reason = Some(reason.to_owned());
        info!(
            watch = self.watch.as_str(),
            run = self.run,
            reason,
            "watch report waiting"
        );
    }

    /// `watch report typed`.
    pub(super) fn typed(self, pane: &str) {
        info!(
            watch = self.watch.as_str(),
            run = self.run,
            pane,
            "watch report typed"
        );
    }

    /// `watch report replaced`: by a newer report, or when the watch ended.
    pub(super) fn left(self, why: Why) {
        info!(
            watch = self.watch.as_str(),
            run = self.run,
            why = why.as_str(),
            "watch report replaced"
        );
    }
}
