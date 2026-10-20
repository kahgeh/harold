use std::sync::Arc;
use std::time::Duration;

use super::*;
use crate::agent::domain::EffectiveAgentState;
use crate::timed_prompts::InputBox;
use crate::tools::watch::test_support::{
    FakeAgents, FakeChief, FakePods, FakeRunner, FakeTyping, Step, TempFolder, agent, answer,
};
use crate::tools::watch::validate::Checked;

const CHIEF_PANE: &str = "%1";
const MINUTE: Duration = Duration::from_secs(60);

pub(in crate::tools::watch) struct Harness {
    pub(in crate::tools::watch) folder: TempFolder,
    /// The store's folder, removed when the harness drops.
    pub(in crate::tools::watch) _data: TempFolder,
    pub(in crate::tools::watch) store: Arc<WatchStore>,
    pub(in crate::tools::watch) scheduler: Arc<Scheduler>,
    pub(in crate::tools::watch) runner: Arc<FakeRunner>,
    pub(in crate::tools::watch) typing: Arc<FakeTyping>,
    pub(in crate::tools::watch) agents: Arc<FakeAgents>,
    pub(in crate::tools::watch) chief: Arc<FakeChief>,
    pub(in crate::tools::watch) pods: Arc<FakePods>,
    pub(in crate::tools::watch) stop: signal::Sender<()>,
}

pub(in crate::tools::watch) fn settings() -> WatchSettings {
    WatchSettings::default()
}

pub(in crate::tools::watch) async fn harness(settings: WatchSettings, steps: Vec<Step>) -> Harness {
    let folder = TempFolder::with_skill(
        "pod-watch",
        "---\nname: pod-watch\nallowed-tools: Bash(bin/pod slots:*)\n---\nCheck.\n",
    );
    let data = TempFolder::new();
    let store = Arc::new(WatchStore::open(&data.0).await.unwrap());
    let runner = FakeRunner::new(steps);
    let typing = FakeTyping::new();
    let agents = FakeAgents::with(vec![agent(
        CHIEF_PANE,
        "/work/chief-of-pods",
        EffectiveAgentState::Idle,
    )]);
    let chief = FakeChief::at(Some(CHIEF_PANE));
    let pods = Arc::new(FakePods::default());
    let (stop, shutdown) = signal::channel(());
    let scheduler = build(
        &store, &runner, &typing, &agents, &chief, &pods, settings, shutdown,
    );
    Harness {
        folder,
        _data: data,
        store,
        scheduler,
        runner,
        typing,
        agents,
        chief,
        pods,
        stop,
    }
}

#[allow(clippy::too_many_arguments)]
fn build(
    store: &Arc<WatchStore>,
    runner: &Arc<FakeRunner>,
    typing: &Arc<FakeTyping>,
    agents: &Arc<FakeAgents>,
    chief: &Arc<FakeChief>,
    pods: &Arc<FakePods>,
    settings: WatchSettings,
    shutdown: signal::Receiver<()>,
) -> Arc<Scheduler> {
    Scheduler::new(Deps {
        store: Arc::clone(store),
        runner: runner.clone(),
        typing: typing.clone(),
        agents: agents.clone(),
        pods: pods.clone(),
        chief_pane: chief.clone(),
        settings,
        offset: UtcOffset::UTC,
        shutdown,
    })
}

impl Harness {
    pub(in crate::tools::watch) fn checked(&self, name: &str, every: u32, minutes: u32) -> Checked {
        Checked {
            name: name.into(),
            folder: self.folder.0.clone(),
            skill: "pod-watch".into(),
            report_to: ReportTo::Chief,
            every_minutes: every,
            for_minutes: minutes,
            every_run: false,
            run_budget_usd_micros: None,
        }
    }

    pub(in crate::tools::watch) async fn start(&self, name: &str, every: u32, minutes: u32) {
        self.scheduler
            .start(self.checked(name, every, minutes))
            .await
            .unwrap();
    }

    pub(in crate::tools::watch) async fn row(&self, name: &str) -> WatchRow {
        self.store.get(name).await.unwrap().unwrap()
    }
}

/// Lets `how_long` of paused time pass, so every timer inside it fires.
pub(in crate::tools::watch) async fn pass(how_long: Duration) {
    tokio::time::sleep(how_long).await;
}

#[test]
fn the_grid_never_runs_one_slot_twice() {
    let started = 1_000;
    let every = 10 * MINUTE_MS;
    assert_eq!(next_on_grid(started, 10, started), started + every);
    assert_eq!(
        next_on_grid(started, 10, started + every - 1),
        started + every
    );
    assert_eq!(
        next_on_grid(started, 10, started + every),
        started + 2 * every
    );
    assert_eq!(
        next_on_grid(started, 10, started + 3 * every + 5),
        started + 4 * every
    );
}

#[tokio::test(start_paused = true)]
async fn a_quiet_watch_types_only_its_closing_line() {
    let h = harness(settings(), vec![answer("QUIET", 0.01)]).await;
    h.start("after-release", 10, 120).await;

    pass(121 * MINUTE).await;

    assert_eq!(h.runner.calls(), 12, "runs at 0, 10 ... 110 minutes");
    assert_eq!(
        h.typing.typed(),
        ["WATCH after-release ended: 12 runs, 0 reports, $0.12"]
    );
    let row = h.row("after-release").await;
    assert_eq!(
        (row.running, row.stop_reason.as_deref()),
        (false, Some("ended"))
    );
    assert_eq!((row.runs, row.reports), (12, 0));
    assert_eq!(row.cost_usd_micros, 120_000);
}

#[tokio::test(start_paused = true)]
async fn every_run_types_a_quiet_line_for_each_run() {
    let h = harness(settings(), vec![answer("QUIET", 0.01)]).await;
    let mut checked = h.checked("w", 10, 30);
    checked.every_run = true;
    h.scheduler.start(checked).await.unwrap();

    pass(31 * MINUTE).await;

    assert_eq!(
        h.typing.typed(),
        [
            "WATCH w 1/3: quiet",
            "WATCH w 2/3: quiet",
            "WATCH w 3/3: quiet",
            "WATCH w ended: 3 runs, 0 reports, $0.03"
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn a_report_is_typed_within_five_seconds_of_its_run() {
    let h = harness(
        settings(),
        vec![
            answer("QUIET", 0.01),
            answer("REPORT harold: watch-jobs merged", 0.01),
        ],
    )
    .await;
    h.start("after-release", 10, 120).await;

    pass(10 * MINUTE + Duration::from_secs(5)).await;

    assert_eq!(
        h.typing.typed(),
        ["WATCH after-release 2/12: harold: watch-jobs merged"]
    );
    let lines = h.typing.lines.lock().unwrap().clone();
    assert_eq!(lines[0].0, CHIEF_PANE);
}

#[tokio::test(start_paused = true)]
async fn a_report_for_a_busy_chief_is_replaced_by_the_next_and_one_line_reaches_it() {
    let h = harness(
        settings(),
        vec![
            answer("REPORT first", 0.01),
            answer("REPORT second", 0.01),
            answer("QUIET", 0.01),
        ],
    )
    .await;
    h.agents.set_state(CHIEF_PANE, EffectiveAgentState::Busy);
    h.start("w", 10, 25).await;

    pass(12 * MINUTE).await;
    assert!(h.typing.typed().is_empty(), "the chief is busy");
    h.agents.set_state(CHIEF_PANE, EffectiveAgentState::Idle);
    pass(Duration::from_secs(6)).await;

    assert_eq!(h.typing.typed(), ["WATCH w 2/3: second"]);
}

#[tokio::test(start_paused = true)]
async fn half_a_sentence_in_the_box_keeps_the_report_waiting() {
    let h = harness(settings(), vec![answer("REPORT x", 0.01)]).await;
    h.agents.set_box(CHIEF_PANE, InputBox::HoldsText);
    h.start("w", 10, 120).await;

    pass(3 * MINUTE).await;
    assert!(h.typing.typed().is_empty());
    h.agents.set_box(CHIEF_PANE, InputBox::NotFound);
    pass(MINUTE).await;
    assert!(
        h.typing.typed().is_empty(),
        "a prompt or menu is not an empty box"
    );
    h.agents.set_box(CHIEF_PANE, InputBox::Empty);
    pass(Duration::from_secs(6)).await;

    assert_eq!(h.typing.typed(), ["WATCH w 1/12: x"]);
}

#[tokio::test(start_paused = true)]
async fn a_waiting_report_is_dropped_when_the_watch_ends_and_only_the_closing_line_is_typed() {
    let h = harness(settings(), vec![answer("REPORT x", 0.01)]).await;
    h.agents.set_state(CHIEF_PANE, EffectiveAgentState::Busy);
    h.start("w", 10, 10).await;

    pass(11 * MINUTE).await;
    h.agents.set_state(CHIEF_PANE, EffectiveAgentState::Idle);
    pass(Duration::from_secs(6)).await;

    assert_eq!(
        h.typing.typed(),
        ["WATCH w ended: 1 runs, 1 reports, $0.01"]
    );
}

#[tokio::test(start_paused = true)]
async fn a_watch_reports_to_its_own_folder_agent_when_asked() {
    let h = harness(settings(), vec![answer("REPORT built", 0.01)]).await;
    let worker = h.folder.0.to_string_lossy().into_owned();
    h.agents
        .agents
        .lock()
        .unwrap()
        .push(agent("%7", &worker, EffectiveAgentState::Idle));
    let mut checked = h.checked("mine", 10, 120);
    checked.report_to = ReportTo::SelfPane;
    h.scheduler.start(checked).await.unwrap();

    pass(Duration::from_secs(10)).await;

    let lines = h.typing.lines.lock().unwrap().clone();
    assert_eq!(
        lines,
        [("%7".to_string(), "WATCH mine 1/12: built".to_string())]
    );
}

#[tokio::test(start_paused = true)]
async fn start_with_a_running_name_replaces_it_and_runs_start_over() {
    let h = harness(settings(), vec![answer("QUIET", 0.01)]).await;
    h.start("w", 10, 120).await;
    pass(25 * MINUTE).await;
    assert_eq!(h.row("w").await.runs, 3);

    let ends = h.scheduler.start(h.checked("w", 5, 60)).await.unwrap();

    pass(Duration::from_secs(2)).await;
    let row = h.row("w").await;
    assert_eq!((row.runs, row.every_minutes, row.ends_at_ms), (1, 5, ends));
    assert!(row.running);
    assert_eq!(h.store.running().await.unwrap().len(), 1);
    // The replaced watch's task is gone: only the new schedule runs.
    let before = h.runner.calls();
    pass(5 * MINUTE).await;
    assert_eq!(h.runner.calls(), before + 1);
}

#[tokio::test(start_paused = true)]
async fn a_sixth_watch_is_refused_but_a_replacement_is_not() {
    let h = harness(settings(), vec![answer("QUIET", 0.01)]).await;
    for index in 0..5 {
        h.start(&format!("w{index}"), 10, 120).await;
    }
    assert_eq!(
        h.scheduler.start(h.checked("w5", 10, 120)).await,
        Err(StartRefusal::TooManyWatches)
    );
    assert!(h.scheduler.start(h.checked("w3", 10, 120)).await.is_ok());
}

#[tokio::test(start_paused = true)]
async fn stop_ends_a_watch_at_once_and_a_run_in_progress_types_nothing() {
    let h = harness(
        settings(),
        vec![Step::Answer {
            result: "REPORT late".into(),
            cost: 0.01,
            after: Duration::from_secs(50),
        }],
    )
    .await;
    h.start("w", 10, 120).await;
    pass(Duration::from_secs(10)).await;
    assert_eq!(h.runner.calls(), 1);

    h.scheduler.stop("w").await.unwrap();
    pass(2 * MINUTE).await;

    assert!(h.typing.typed().is_empty(), "no report and no closing line");
    let row = h.row("w").await;
    assert_eq!(
        (row.running, row.stop_reason.as_deref()),
        (false, Some("stopped"))
    );
    assert_eq!(h.scheduler.stop("w").await, Err(StopRefusal::NotRunning));
    assert_eq!(
        h.scheduler.stop("nope").await,
        Err(StopRefusal::UnknownWatch)
    );
    pass(30 * MINUTE).await;
    assert_eq!(h.runner.calls(), 1, "a stopped watch runs no more");
}

#[tokio::test(start_paused = true)]
async fn an_unreadable_answer_is_a_failed_run_and_types_nothing() {
    let h = harness(settings(), vec![answer("All good!", 0.01)]).await;
    h.start("w", 10, 120).await;

    pass(Duration::from_secs(5)).await;

    assert!(h.typing.typed().is_empty());
    let row = h.row("w").await;
    assert_eq!(row.last_outcome.as_deref(), Some("unreadable_answer"));
    assert_eq!(row.failed_in_a_row, 1);
}

#[tokio::test(start_paused = true)]
async fn a_long_dirty_answer_is_cleaned_and_cut_before_it_is_typed() {
    let dirty = format!("REPORT \u{1b}[31m{}\u{1b}[0m\nsecond line", "é".repeat(400));
    let h = harness(settings(), vec![answer(&dirty, 0.01)]).await;
    h.start("w", 10, 120).await;

    pass(Duration::from_secs(10)).await;

    let typed = h.typing.typed();
    assert_eq!(typed.len(), 1);
    assert_eq!(typed[0], format!("WATCH w 1/12: {}", "é".repeat(300)));
}

#[tokio::test(start_paused = true)]
async fn three_failed_runs_in_a_row_stop_the_watch_with_the_last_reason() {
    let h = harness(settings(), vec![Step::NoStart]).await;
    h.start("w", 10, 120).await;

    pass(25 * MINUTE).await;

    assert_eq!(h.runner.calls(), 3);
    assert_eq!(
        h.typing.typed(),
        ["WATCH w stopped: 3 runs in a row failed (process_failed)"]
    );
    let row = h.row("w").await;
    assert_eq!(row.stop_reason.as_deref(), Some("runs_failing"));
    pass(60 * MINUTE).await;
    assert_eq!(h.runner.calls(), 3);
}

#[tokio::test(start_paused = true)]
async fn a_good_run_between_failures_resets_the_count() {
    let h = harness(
        settings(),
        vec![
            Step::NoStart,
            Step::NoStart,
            answer("QUIET", 0.01),
            Step::NoStart,
            Step::NoStart,
        ],
    )
    .await;
    h.start("w", 10, 120).await;

    pass(45 * MINUTE).await;

    assert_eq!(h.row("w").await.failed_in_a_row, 2);
    assert!(h.row("w").await.running);
}

#[tokio::test(start_paused = true)]
async fn a_timeout_counts_at_the_run_budget_when_there_is_one_else_its_cost_is_unknown() {
    let h = harness(settings(), vec![Step::Hang]).await;
    let mut with_budget = h.checked("budgeted", 10, 120);
    with_budget.run_budget_usd_micros = Some(30_000);
    h.scheduler.start(with_budget).await.unwrap();
    // One model at a time: the second watch starts when the first one's run is over.
    pass(3 * MINUTE).await;
    h.start("unbudgeted", 10, 120).await;
    pass(3 * MINUTE).await;

    let budgeted = h.row("budgeted").await;
    assert_eq!(budgeted.last_outcome.as_deref(), Some("timeout"));
    assert_eq!(budgeted.cost_usd_micros, 30_000);
    let unbudgeted = h.row("unbudgeted").await;
    assert_eq!(unbudgeted.last_outcome.as_deref(), Some("timeout"));
    assert_eq!(unbudgeted.cost_usd_micros, 0);
    let (from, to) = (0, i64::MAX);
    assert_eq!(h.store.spent_micros(from, to).await.unwrap(), 30_000);
}

#[tokio::test(start_paused = true)]
async fn a_run_over_its_budget_and_a_deleted_skill_are_failed_runs() {
    let over = r#"{"subtype":"error_max_budget_usd","is_error":true,"total_cost_usd":0.05}"#;
    let h = harness(
        settings(),
        vec![Step::Raw {
            stdout: over.into(),
            ok: false,
        }],
    )
    .await;
    h.start("w", 10, 120).await;
    pass(Duration::from_secs(5)).await;
    let row = h.row("w").await;
    assert_eq!(row.last_outcome.as_deref(), Some("run_budget_reached"));
    assert_eq!(row.cost_usd_micros, 50_000);

    let h = harness(settings(), vec![answer("QUIET", 0.01)]).await;
    h.start("gone", 10, 120).await;
    pass(Duration::from_secs(5)).await;
    std::fs::remove_file(h.folder.0.join(".claude/skills/pod-watch/SKILL.md")).unwrap();
    pass(35 * MINUTE).await;
    assert_eq!(h.runner.calls(), 1, "no model run without a skill");
    assert_eq!(
        h.typing.typed(),
        ["WATCH gone stopped: 3 runs in a row failed (skill_missing)"]
    );
}

#[tokio::test(start_paused = true)]
async fn the_skill_is_read_afresh_at_each_run() {
    let h = harness(settings(), vec![answer("QUIET", 0.01)]).await;
    h.start("w", 10, 120).await;
    pass(Duration::from_secs(5)).await;
    h.folder.write_skill(
        "pod-watch",
        "---\nallowed-tools: Write, Bash(git push:*)\n---\nNEW RULES\n",
    );

    pass(10 * MINUTE).await;

    let jobs = h.runner.jobs.lock().unwrap().clone();
    assert_eq!(
        jobs[0].allowed_tools.as_deref(),
        Some("Bash(bin/pod slots:*)")
    );
    assert!(jobs[0].system_prompt.ends_with("Check.\n"));
    assert_eq!(
        jobs[1].allowed_tools.as_deref(),
        Some("Write, Bash(git push:*)")
    );
    assert!(jobs[1].system_prompt.ends_with("NEW RULES\n"));
}

#[tokio::test(start_paused = true)]
async fn the_job_has_the_folder_model_budget_and_facts() {
    let h = harness(
        WatchSettings {
            run_budget_usd: Some(0.05),
            ..settings()
        },
        vec![answer("QUIET", 0.01)],
    )
    .await;
    h.start("w", 10, 120).await;
    let mut asked = h.checked("own", 10, 120);
    asked.run_budget_usd_micros = Some(30_000);
    h.scheduler.start(asked).await.unwrap();
    pass(Duration::from_secs(5)).await;

    let jobs = h.runner.jobs.lock().unwrap().clone();
    assert_eq!(jobs.len(), 2);
    assert_eq!(jobs[0].folder, h.folder.0);
    assert_eq!(jobs[0].model, "haiku");
    assert_eq!(jobs[0].timeout, RUN_LIMIT);
    let budgets: Vec<_> = jobs.iter().map(|job| job.run_budget_usd_micros).collect();
    assert!(
        budgets.contains(&Some(50_000)),
        "the setting applies: {budgets:?}"
    );
    assert!(
        budgets.contains(&Some(30_000)),
        "the flag wins: {budgets:?}"
    );
    let facts: serde_json::Value = serde_json::from_slice(&jobs[0].facts_json).unwrap();
    assert_eq!(facts["chief"], true);
    assert_eq!(facts["watch"]["run"], 1);
    assert_eq!(facts["watch"]["runs_left"], 11);
    assert_eq!(facts["agents"][0]["input_box"], "empty");
}

#[tokio::test(start_paused = true)]
async fn no_budget_anywhere_means_no_run_limit_on_the_job() {
    let h = harness(settings(), vec![answer("QUIET", 0.01)]).await;
    h.start("w", 10, 120).await;
    pass(Duration::from_secs(5)).await;
    assert_eq!(h.runner.jobs.lock().unwrap()[0].run_budget_usd_micros, None);
}

#[tokio::test(start_paused = true)]
async fn only_one_model_runs_at_a_time_and_a_run_that_waits_over_a_minute_is_skipped() {
    let h = harness(
        settings(),
        vec![Step::Answer {
            result: "QUIET".into(),
            cost: 0.01,
            after: Duration::from_secs(100),
        }],
    )
    .await;
    h.start("a", 10, 120).await;
    h.start("b", 10, 120).await;

    pass(Duration::from_secs(70)).await;
    assert_eq!(h.row("b").await.last_outcome.as_deref(), Some("skipped"));
    assert_eq!(h.row("b").await.failed_in_a_row, 0);
    pass(Duration::from_secs(60)).await;
    assert_eq!(*h.runner.max_running.lock().unwrap(), 1);
    assert_eq!(h.row("a").await.last_outcome.as_deref(), Some("quiet"));
}

#[tokio::test(start_paused = true)]
async fn a_set_daily_budget_stops_every_watch_before_a_run_that_could_pass_it() {
    let h = harness(
        WatchSettings {
            daily_budget_usd: Some(0.05),
            ..settings()
        },
        vec![answer("QUIET", 0.03)],
    )
    .await;
    let mut first = h.checked("a", 10, 120);
    first.run_budget_usd_micros = Some(30_000);
    h.scheduler.start(first).await.unwrap();
    let mut second = h.checked("b", 10, 120);
    second.run_budget_usd_micros = Some(30_000);
    h.scheduler.start(second).await.unwrap();

    pass(Duration::from_secs(30)).await;

    // a's run spent 0.03; b's run (budget 0.03) could pass 0.05.
    let mut typed = h.typing.typed();
    typed.sort();
    assert_eq!(
        typed,
        [
            "WATCH a stopped: daily budget of $0.05 spent",
            "WATCH b stopped: daily budget of $0.05 spent"
        ]
    );
    assert_eq!(h.runner.calls(), 1);
    assert_eq!(
        h.row("a").await.stop_reason.as_deref(),
        Some("daily_budget_spent")
    );
    assert_eq!(
        h.scheduler.start(h.checked("c", 10, 120)).await,
        Err(StartRefusal::DailyBudgetSpent)
    );

    // The next local day lets `start` work again.
    pass(Duration::from_secs(25 * 3600)).await;
    assert!(h.scheduler.start(h.checked("c", 10, 120)).await.is_ok());
}

#[tokio::test(start_paused = true)]
async fn a_spent_day_refuses_start_even_when_nothing_was_projected() {
    let h = harness(
        WatchSettings {
            daily_budget_usd: Some(0.05),
            ..settings()
        },
        vec![answer("QUIET", 0.06)],
    )
    .await;
    h.start("a", 10, 10).await;
    pass(Duration::from_secs(5)).await;
    assert_eq!(
        h.scheduler.start(h.checked("b", 10, 10)).await,
        Err(StartRefusal::DailyBudgetSpent)
    );
}

#[tokio::test(start_paused = true)]
async fn without_a_daily_budget_nothing_stops_for_cost_and_list_shows_the_spend() {
    let h = harness(settings(), vec![answer("QUIET", 5.0)]).await;
    h.start("a", 10, 60).await;
    h.start("b", 10, 60).await;

    pass(35 * MINUTE).await;

    assert!(h.row("a").await.running && h.row("b").await.running);
    let listing = h.scheduler.list().await.unwrap();
    assert_eq!(listing.watches.len(), 2);
    assert_eq!(listing.spent_usd_micros, 8 * 5_000_000);
    assert!(h.scheduler.start(h.checked("c", 10, 60)).await.is_ok());
}

#[tokio::test(start_paused = true)]
async fn list_shows_running_watches_and_those_that_ended_today() {
    let h = harness(settings(), vec![answer("QUIET", 0.01)]).await;
    h.start("long", 10, 120).await;
    h.start("short", 10, 5).await;
    h.start("gone", 10, 120).await;
    pass(6 * MINUTE).await;
    h.scheduler.stop("gone").await.unwrap();

    let listing = h.scheduler.list().await.unwrap();

    let by_name = |name: &str| listing.watches.iter().find(|w| w.name == name).unwrap();
    assert!(by_name("long").running);
    assert_eq!(by_name("short").stop_reason.as_deref(), Some("ended"));
    assert_eq!(by_name("gone").stop_reason.as_deref(), Some("stopped"));
    assert_eq!(listing.watches.len(), 3);
}

/// A second scheduler on the first one's store, as after a Harold restart.
fn restarted(h: &Harness) -> (Arc<Scheduler>, Arc<FakeRunner>, signal::Sender<()>) {
    let runner = FakeRunner::new(vec![answer("QUIET", 0.01)]);
    let (stop, shutdown) = signal::channel(());
    let scheduler = build(
        &h.store,
        &runner,
        &h.typing,
        &h.agents,
        &h.chief,
        &h.pods,
        settings(),
        shutdown,
    );
    (scheduler, runner, stop)
}

#[tokio::test(start_paused = true)]
async fn a_restart_before_the_next_run_resumes_the_watch_with_the_same_end() {
    let h = harness(settings(), vec![answer("QUIET", 0.01)]).await;
    h.start("w", 10, 120).await;
    pass(25 * MINUTE).await;
    let before = h.row("w").await;
    assert_eq!(
        (before.runs, before.next_run_at_ms - before.started_at_ms),
        (3, 30 * MINUTE_MS)
    );
    // Harold stops: every task ends. The store keeps the watch.
    let _ = h.stop.send(());
    pass(Duration::from_secs(1)).await;
    pass(3 * MINUTE).await;
    assert_eq!(h.runner.calls(), 3);

    let (scheduler, runner, _stop) = restarted(&h);
    scheduler.resume().await;
    pass(Duration::from_secs(30)).await;
    assert_eq!(runner.calls(), 0, "run 4 is due at 30 minutes, not at once");
    pass(3 * MINUTE).await;
    assert_eq!(runner.calls(), 1);
    let after = h.row("w").await;
    assert_eq!((after.ends_at_ms, after.runs), (before.ends_at_ms, 4));
}

#[tokio::test(start_paused = true)]
async fn a_restart_after_a_missed_run_runs_it_once_at_once_then_keeps_the_grid() {
    let h = harness(settings(), vec![answer("QUIET", 0.01)]).await;
    h.start("w", 10, 120).await;
    pass(25 * MINUTE).await;
    let before = h.row("w").await;
    let _ = h.stop.send(());
    pass(Duration::from_secs(1)).await;
    // Down from 25 to 40 minutes: run 4 (30) was missed.
    pass(15 * MINUTE).await;

    let (scheduler, runner, _stop) = restarted(&h);
    scheduler.resume().await;
    pass(Duration::from_secs(5)).await;
    assert_eq!(runner.calls(), 1, "run 4 at once, once");
    pass(4 * MINUTE).await;
    assert_eq!(runner.calls(), 1);
    pass(7 * MINUTE).await;
    assert_eq!(runner.calls(), 2, "run 5 at 50 minutes");
    let after = h.row("w").await;
    assert_eq!(after.ends_at_ms, before.ends_at_ms);
    assert!(after.running);
}

#[tokio::test(start_paused = true)]
async fn a_watch_whose_time_ran_out_while_harold_was_down_ends_on_resume() {
    let h = harness(settings(), vec![answer("QUIET", 0.01)]).await;
    h.start("w", 10, 20).await;
    pass(Duration::from_secs(5)).await;
    let _ = h.stop.send(());
    pass(Duration::from_secs(1)).await;
    pass(40 * MINUTE).await;

    let (scheduler, _runner, _stop) = restarted(&h);
    scheduler.resume().await;
    pass(Duration::from_secs(10)).await;

    assert_eq!(h.row("w").await.stop_reason.as_deref(), Some("ended"));
}
