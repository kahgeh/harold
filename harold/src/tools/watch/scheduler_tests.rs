use std::sync::Arc;
use std::time::Duration;

use super::*;
use crate::agent::domain::EffectiveAgentState;
use crate::timed_prompts::InputBox;
use crate::tools::watch::clock;
use crate::tools::watch::store::WatchState;
use crate::tools::watch::test_support::{
    FakeAgents, FakeChief, FakePods, FakeRunner, FakeTyping, ScriptedOffset, Step, TempFolder,
    agent, answer,
};
use crate::tools::watch::validate::{Caller, Checked, CheckedPause, CheckedResume, Target as Aim};

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
    harness_on(settings, steps, &ScriptedOffset::at(UtcOffset::UTC)).await
}

async fn harness_with_offset(local: &Arc<ScriptedOffset>) -> Harness {
    harness_on(settings(), vec![answer("QUIET", 0.01)], local).await
}

pub(in crate::tools::watch) async fn harness_on(
    settings: WatchSettings,
    steps: Vec<Step>,
    local: &Arc<ScriptedOffset>,
) -> Harness {
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
    let scheduler = build_on(
        local, &store, &runner, &typing, &agents, &chief, &pods, settings, shutdown,
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
    let local = ScriptedOffset::at(UtcOffset::UTC);
    build_on(
        &local, store, runner, typing, agents, chief, pods, settings, shutdown,
    )
}

#[allow(clippy::too_many_arguments)]
pub(in crate::tools::watch) fn build_on(
    local: &Arc<ScriptedOffset>,
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
        local: local.clone(),
        shutdown,
        counts: Arc::new(signal::channel(WatchCounts::default()).0),
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
            cron: None,
            for_minutes: Some(minutes),
            every_run: false,
            run_budget_usd_micros: None,
            run_limit_secs: 120,
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

/// Moves the test clock to 00:01 UTC, the harness's offset, of the next day, so a test that
/// lets minutes or hours pass reads one day's spend, budget and endings.
async fn start_just_after_midnight() {
    let (_, next_midnight) = day_bounds(clock_ms(), UtcOffset::UTC);
    pass(Duration::from_millis(
        (next_midnight + 60_000 - clock_ms()) as u64,
    ))
    .await;
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
    h.agents.set_state(CHIEF_PANE, EffectiveAgentState::Working);
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
    h.agents.set_state(CHIEF_PANE, EffectiveAgentState::Working);
    h.start("w", 10, 10).await;

    pass(11 * MINUTE).await;
    h.agents.set_state(CHIEF_PANE, EffectiveAgentState::Idle);
    pass(Duration::from_secs(6)).await;

    assert_eq!(h.typing.typed(), ["WATCH w ended: 1 run, 1 report, $0.01"]);
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
async fn three_turn_cap_runs_in_a_row_stop_the_watch() {
    let cap = Step::Exit {
        stdout: r#"{"subtype":"error_max_turns","is_error":true,"num_turns":9}"#.into(),
        exit: Exit {
            code: Some(1),
            signal: None,
        },
    };
    let h = harness(settings(), vec![cap]).await;
    h.start("w", 10, 120).await;

    pass(25 * MINUTE).await;

    assert_eq!(h.runner.calls(), 3);
    assert_eq!(
        h.typing.typed(),
        ["WATCH w stopped: 3 runs in a row failed (max_turns_reached)"]
    );
    let row = h.row("w").await;
    assert_eq!(row.last_outcome.as_deref(), Some("max_turns_reached"));
    assert_eq!(row.stop_reason.as_deref(), Some("runs_failing"));
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
    start_just_after_midnight().await;
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
async fn an_edited_add_dirs_reaches_the_next_runs_job() {
    let h = harness(settings(), vec![answer("QUIET", 0.01)]).await;
    h.start("w", 10, 120).await;
    pass(Duration::from_secs(5)).await;
    h.folder
        .write_skill("pod-watch", "---\nadd-dirs: /a, /b\n---\nNEW\n");

    pass(10 * MINUTE).await;

    let jobs = h.runner.jobs.lock().unwrap().clone();
    assert!(jobs[0].add_dirs.is_empty());
    assert_eq!(jobs[1].add_dirs, ["/a", "/b"]);
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
    assert_eq!(jobs[0].timeout, Duration::from_secs(120));
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
async fn the_job_timeout_is_the_run_limit_of_the_watch() {
    let h = harness(settings(), vec![answer("QUIET", 0.01)]).await;
    h.start("default", 10, 120).await;
    let mut asked = h.checked("long", 10, 120);
    asked.run_limit_secs = 300;
    h.scheduler.start(asked).await.unwrap();
    pass(Duration::from_secs(5)).await;

    let jobs = h.runner.jobs.lock().unwrap().clone();
    assert_eq!(jobs.len(), 2);
    let timeouts: Vec<_> = jobs.iter().map(|job| job.timeout).collect();
    assert!(timeouts.contains(&Duration::from_secs(120)), "{timeouts:?}");
    assert!(timeouts.contains(&Duration::from_secs(300)), "{timeouts:?}");
}

#[tokio::test(start_paused = true)]
async fn a_run_of_150_seconds_completes_under_a_300_second_limit_and_is_killed_under_the_default() {
    let slow = || Step::Answer {
        result: "QUIET".into(),
        cost: 0.01,
        after: Duration::from_secs(150),
    };
    let h = harness(settings(), vec![slow()]).await;
    let mut asked = h.checked("long", 10, 120);
    asked.run_limit_secs = 300;
    h.scheduler.start(asked).await.unwrap();
    pass(Duration::from_secs(160)).await;
    assert_eq!(h.row("long").await.last_outcome.as_deref(), Some("quiet"));

    let h = harness(settings(), vec![slow()]).await;
    h.start("default", 10, 120).await;
    pass(Duration::from_secs(160)).await;
    assert_eq!(
        h.row("default").await.last_outcome.as_deref(),
        Some("timeout")
    );
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
    start_just_after_midnight().await;
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
async fn a_run_that_crosses_midnight_counts_toward_the_day_it_started() {
    let h = harness(
        settings(),
        vec![Step::Answer {
            result: "QUIET".into(),
            cost: 0.02,
            after: Duration::from_secs(40),
        }],
    )
    .await;
    // Move to 23:59:30 local (UTC here), so the run ends at 00:00:10.
    let (_, next_midnight) = day_bounds(clock_ms(), UtcOffset::UTC);
    let to_go = (next_midnight - clock_ms() - 30_000).rem_euclid(86_400_000);
    pass(Duration::from_millis(to_go as u64)).await;
    let (day_start, day_end) = day_bounds(clock_ms(), UtcOffset::UTC);
    h.start("w", 10, 120).await;

    pass(Duration::from_secs(60)).await;

    assert_eq!(h.runner.calls(), 1);
    assert!(clock_ms() >= day_end, "the run ended after midnight");
    assert_eq!(
        h.store.spent_micros(day_start, day_end).await.unwrap(),
        20_000
    );
    assert_eq!(
        h.store
            .spent_micros(day_end, day_end + 86_400_000)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test(start_paused = true)]
async fn a_run_stopped_after_midnight_counts_toward_the_day_it_started() {
    let h = harness(
        settings(),
        vec![Step::Answer {
            result: "QUIET".into(),
            cost: 0.02,
            after: Duration::from_secs(600),
        }],
    )
    .await;
    let (_, next_midnight) = day_bounds(clock_ms(), UtcOffset::UTC);
    let to_go = (next_midnight - clock_ms() - 30_000).rem_euclid(86_400_000);
    pass(Duration::from_millis(to_go as u64)).await;
    let (day_start, day_end) = day_bounds(clock_ms(), UtcOffset::UTC);
    let mut checked = h.checked("w", 10, 120);
    checked.run_budget_usd_micros = Some(30_000);
    h.scheduler.start(checked).await.unwrap();

    pass(Duration::from_secs(40)).await;
    h.scheduler.stop("w").await.unwrap();
    pass(Duration::from_secs(1)).await;

    assert!(clock_ms() >= day_end, "the stop came after midnight");
    assert_eq!(
        h.store.spent_micros(day_start, day_end).await.unwrap(),
        30_000
    );
}

#[tokio::test]
async fn stopping_a_watch_kills_the_child_its_run_started() {
    let folder = TempFolder::with_skill("pod-watch", "---\nname: pod-watch\n---\nCheck.\n");
    let data = TempFolder::new();
    let cli = data.0.join("fake-claude");
    crate::test_support::fake_program::write_fake_program(
        &cli,
        crate::test_support::fake_program::Language::Shell,
        "sleep 300 &\necho $! > child.pid\ncat > /dev/null\nsleep 300",
    );
    let store = Arc::new(WatchStore::open(&data.0).await.unwrap());
    let (_stop, shutdown) = signal::channel(());
    let scheduler = Scheduler::new(Deps {
        store: Arc::clone(&store),
        runner: Arc::new(crate::tools::watch::runner::ClaudeRunner::new(
            cli.to_str().unwrap().to_owned(),
            vec![("PATH".into(), std::env::var_os("PATH").unwrap())],
        )),
        typing: FakeTyping::new(),
        agents: FakeAgents::with(Vec::new()),
        pods: Arc::new(FakePods::default()),
        chief_pane: FakeChief::at(Some(CHIEF_PANE)),
        settings: settings(),
        local: ScriptedOffset::at(UtcOffset::UTC),
        shutdown,
        counts: Arc::new(signal::channel(WatchCounts::default()).0),
    });
    let checked = Checked {
        name: "w".into(),
        folder: folder.0.clone(),
        skill: "pod-watch".into(),
        report_to: ReportTo::Chief,
        every_minutes: 10,
        cron: None,
        for_minutes: Some(120),
        every_run: false,
        run_budget_usd_micros: None,
        run_limit_secs: 120,
    };
    scheduler.start(checked).await.unwrap();

    let pid_file = folder.0.join("child.pid");
    let mut pid = None;
    for _ in 0..100 {
        if let Ok(text) = std::fs::read_to_string(&pid_file)
            && !text.trim().is_empty()
        {
            pid = Some(text.trim().to_owned());
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let pid = pid.expect("the fake Claude Code started its child");
    let alive = |pid: &str| {
        std::process::Command::new("kill")
            .args(["-0", pid])
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success()
    };
    assert!(alive(&pid), "the child runs before the stop");

    scheduler.stop("w").await.unwrap();

    let mut gone = false;
    for _ in 0..60 {
        if !alive(&pid) {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(gone, "the child {pid} outlived the stop");
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
    start_just_after_midnight().await;
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
    start_just_after_midnight().await;
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
    start_just_after_midnight().await;
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
    assert_eq!(
        h.typing.typed(),
        ["WATCH w ended: 1 run, 0 reports, $0.01"],
        "the closing line goes out even though the watch ran out while Harold was down"
    );
}

#[tokio::test(start_paused = true)]
async fn concurrent_starts_keep_to_five_watches() {
    let h = harness(settings(), vec![answer("QUIET", 0.01)]).await;
    for index in 0..4 {
        h.start(&format!("w{index}"), 10, 120).await;
    }
    let (a, b) = tokio::join!(
        h.scheduler.start(h.checked("x", 10, 120)),
        h.scheduler.start(h.checked("y", 10, 120)),
    );
    assert_eq!(a.is_ok() as u8 + b.is_ok() as u8, 1, "{a:?} {b:?}");
    assert_eq!(h.store.running().await.unwrap().len(), 5);
}

#[tokio::test(start_paused = true)]
async fn a_linked_skill_runs_from_a_listed_folder_and_fails_when_its_link_leaves_it() {
    let kept = TempFolder::new();
    let listed = kept.0.join("watches");
    let (inside, outside) = (listed.join("pod-watch"), kept.0.join("other/pod-watch"));
    for dir in [&inside, &outside] {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), "Linked rules.\n").unwrap();
    }
    let h = harness(
        WatchSettings {
            skill_folders: vec![listed.to_string_lossy().into_owned()],
            ..settings()
        },
        vec![answer("QUIET", 0.01)],
    )
    .await;
    let link = h.folder.0.join(".claude/skills/pod-watch");
    std::fs::remove_dir_all(&link).unwrap();
    std::os::unix::fs::symlink(&inside, &link).unwrap();
    h.start("w", 10, 120).await;
    pass(Duration::from_secs(5)).await;
    assert_eq!(h.runner.calls(), 1);
    assert!(
        h.runner.jobs.lock().unwrap()[0]
            .system_prompt
            .ends_with("Linked rules.\n")
    );

    // The link now points outside every listed folder.
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    pass(35 * MINUTE).await;

    assert_eq!(h.runner.calls(), 1, "no model run without an allowed skill");
    assert_eq!(
        h.typing.typed(),
        ["WATCH w stopped: 3 runs in a row failed (skill_missing)"]
    );
}

#[tokio::test(start_paused = true)]
async fn the_closing_line_leaves_skipped_runs_out_of_the_run_count() {
    let h = harness(settings(), vec![answer("QUIET", 0.01)]).await;
    h.start("w", 10, 120).await;
    let began = h.row("w").await.started_at_ms;
    for run in 1..=12 {
        let outcome = if run <= 2 { "skipped" } else { "quiet" };
        h.store
            .record_run(
                "w",
                began,
                &RunRecord {
                    started_at_ms: began + run,
                    outcome: outcome.into(),
                    cost_usd_micros: Some(10_000),
                    streak: Streak::Unchanged,
                    next_run_at_ms: began + 1_000 * MINUTE_MS,
                },
            )
            .await
            .unwrap();
    }
    pass(125 * MINUTE).await;
    let typed = h.typing.typed();
    assert_eq!(typed.len(), 1);
    assert_eq!(
        typed[0],
        "WATCH w ended: 10 runs, 2 skipped, 0 reports, $0.12"
    );
}

#[tokio::test(start_paused = true)]
async fn a_budget_stop_survives_a_restart_for_that_day_only() {
    let h = harness(
        WatchSettings {
            daily_budget_usd: Some(0.05),
            ..settings()
        },
        vec![answer("QUIET", 0.03)],
    )
    .await;
    start_just_after_midnight().await;
    let mut first = h.checked("a", 10, 120);
    first.run_budget_usd_micros = Some(60_000);
    h.scheduler.start(first).await.unwrap();
    pass(Duration::from_secs(30)).await;
    assert_eq!(
        h.row("a").await.stop_reason.as_deref(),
        Some("daily_budget_spent")
    );

    let runner = FakeRunner::new(vec![answer("QUIET", 0.01)]);
    let (_stop, shutdown) = signal::channel(());
    let again = build(
        &h.store,
        &runner,
        &h.typing,
        &h.agents,
        &h.chief,
        &h.pods,
        WatchSettings {
            daily_budget_usd: Some(0.05),
            ..settings()
        },
        shutdown,
    );
    assert_eq!(
        again.start(h.checked("c", 10, 120)).await,
        Err(StartRefusal::DailyBudgetSpent)
    );
    pass(Duration::from_secs(25 * 3600)).await;
    assert!(again.start(h.checked("c", 10, 120)).await.is_ok());
}

#[tokio::test(start_paused = true)]
async fn day_bounds_and_end_time_follow_a_changed_offset() {
    let plus_eleven = UtcOffset::from_hms(11, 0, 0).unwrap();
    let plus_ten = UtcOffset::from_hms(10, 0, 0).unwrap();
    let local = ScriptedOffset::at(plus_eleven);
    let h = harness_with_offset(&local).await;
    h.start("w", 60, 120).await;

    h.scheduler.list().await.unwrap();
    let (old_start, old_end) = h.scheduler.today().await;
    assert_eq!((old_start, old_end), day_bounds(clock_ms(), plus_eleven));
    let ends_at = h.row("w").await.ends_at_ms.unwrap();
    let old_end_text = local_text(ends_at, plus_eleven);

    // 23:30 at +10:00 on the new day is 00:30 at +11:00 on the next day.
    local.set(plus_ten);
    h.scheduler.list().await.unwrap();
    let (new_start, new_end) = h.scheduler.today().await;
    assert_eq!((new_start, new_end), day_bounds(clock_ms(), plus_ten));
    assert_eq!(new_start - old_start, 3_600_000);
    assert_ne!(local_text(ends_at, plus_ten), old_end_text);

    let late = new_end - 30 * 60_000;
    let record = RunRecord {
        started_at_ms: late,
        outcome: "quiet".into(),
        cost_usd_micros: Some(30_000),
        streak: Streak::Worked,
        next_run_at_ms: late + 3_600_000,
    };
    let started = h.row("w").await.started_at_ms;
    h.store.record_run("w", started, &record).await.unwrap();
    assert_eq!(h.scheduler.list().await.unwrap().spent_usd_micros, 30_000);

    local.set(plus_eleven);
    assert_eq!(h.scheduler.list().await.unwrap().spent_usd_micros, 0);
}

// ---------------------------------------------------------------------------
// Open-ended watches, pause and resume, and the capped watch
// ---------------------------------------------------------------------------

/// Settings with a daily budget of `daily` dollars.
fn with_daily(daily: f64) -> WatchSettings {
    WatchSettings {
        daily_budget_usd: Some(daily),
        ..settings()
    }
}

fn pause_of(name: &str, for_minutes: Option<u32>) -> CheckedPause {
    CheckedPause {
        target: Aim::Name(name.into()),
        for_minutes,
        caller: Caller::Cli,
    }
}

fn pause_all() -> CheckedPause {
    CheckedPause {
        target: Aim::All,
        for_minutes: None,
        caller: Caller::Cli,
    }
}

fn resume_of(name: &str) -> CheckedResume {
    CheckedResume {
        target: Aim::Name(name.into()),
        caller: Caller::Cli,
    }
}

fn resume_all() -> CheckedResume {
    CheckedResume {
        target: Aim::All,
        caller: Caller::Cli,
    }
}

impl Harness {
    /// A watch with no end.
    fn open(&self, name: &str, every: u32) -> Checked {
        Checked {
            for_minutes: None,
            ..self.checked(name, every, 1)
        }
    }

    async fn start_open(&self, name: &str, every: u32) {
        self.scheduler.start(self.open(name, every)).await.unwrap();
    }

    async fn pause(&self, name: &str, for_minutes: Option<u32>) {
        self.scheduler
            .pause(pause_of(name, for_minutes))
            .await
            .unwrap();
    }

    async fn resume(&self, name: &str) {
        self.scheduler
            .resume_watches(resume_of(name))
            .await
            .unwrap();
    }

    /// Starts an open-ended watch whose run budget is over the daily budget: the budget
    /// check pauses it before its first run.
    async fn start_capped(&self, name: &str) {
        let mut checked = self.open(name, 10);
        checked.run_budget_usd_micros = Some(600_000);
        self.scheduler.start(checked).await.unwrap();
        pass(Duration::from_secs(5)).await;
        assert_eq!(
            self.row(name).await.pause_how.as_deref(),
            Some("budget_day")
        );
    }
}

/// The start of the next local day, at the harness's offset (UTC).
fn next_day_start() -> i64 {
    day_bounds(clock_ms(), UtcOffset::UTC).1
}

#[tokio::test(start_paused = true)]
async fn an_open_ended_watch_runs_past_1440_minutes() {
    let h = harness(with_daily(100.0), vec![answer("QUIET", 0.01)]).await;
    let mut checked = h.open("pods", 60);
    checked.every_run = true;
    h.scheduler.start(checked).await.unwrap();
    assert_eq!(h.row("pods").await.ends_at_ms, None);

    pass(1501 * MINUTE).await;

    assert_eq!(h.runner.calls(), 26, "runs at 0, 60 ... 1500 minutes");
    let row = h.row("pods").await;
    assert!(row.running);
    assert_eq!((row.runs, row.ends_at_ms), (26, None));
    let typed = h.typing.typed();
    assert_eq!(typed.len(), 26);
    assert_eq!(typed[0], "WATCH pods 1: quiet");
    assert_eq!(typed[25], "WATCH pods 26: quiet");
    let facts: serde_json::Value =
        serde_json::from_slice(&h.runner.jobs.lock().unwrap()[0].facts_json).unwrap();
    assert_eq!(facts["watch"]["run"], 1);
    assert!(facts["watch"].get("runs_left").is_none(), "{facts}");
}

#[tokio::test(start_paused = true)]
async fn an_open_ended_watch_survives_a_restart() {
    let h = harness(with_daily(100.0), vec![answer("QUIET", 0.01)]).await;
    h.start_open("pods", 10).await;
    pass(25 * MINUTE).await;
    assert_eq!(h.runner.calls(), 3);
    let _ = h.stop.send(());
    pass(Duration::from_secs(1)).await;

    let (scheduler, runner, _stop) = restarted(&h);
    scheduler.resume().await;
    pass(1500 * MINUTE).await;

    assert_eq!(runner.calls(), 150, "runs at 30, 40 ... 1520 minutes");
    let row = h.row("pods").await;
    assert!(row.running);
    assert_eq!((row.runs, row.ends_at_ms), (153, None));
    assert!(h.typing.typed().is_empty());
}

#[tokio::test(start_paused = true)]
async fn start_without_for_and_without_a_daily_budget_is_refused() {
    let h = harness(settings(), vec![answer("QUIET", 0.01)]).await;

    assert_eq!(
        h.scheduler.start(h.open("pods", 10)).await,
        Err(StartRefusal::DailyBudgetUnset)
    );
    assert_eq!(h.store.get("pods").await.unwrap(), None, "nothing stored");
    pass(30 * MINUTE).await;
    assert_eq!(h.runner.calls(), 0);
    assert!(h.scheduler.start(h.checked("pods", 10, 60)).await.is_ok());
}

#[tokio::test(start_paused = true)]
async fn a_spent_budget_stops_a_watch_with_an_end_and_pauses_an_open_ended_one_until_the_next_day_then_it_resumes()
 {
    let h = harness(with_daily(1.0), vec![answer("QUIET", 0.6)]).await;
    start_just_after_midnight().await;
    let mut ending = h.checked("a", 10, 120);
    ending.run_budget_usd_micros = Some(600_000);
    h.scheduler.start(ending).await.unwrap();
    pass(Duration::from_secs(5)).await;
    assert_eq!(h.runner.calls(), 1, "a spent $0.60");
    let mut open = h.open("pods", 10);
    open.run_budget_usd_micros = Some(600_000);
    h.scheduler.start(open).await.unwrap();
    pass(Duration::from_secs(5)).await;

    // pods' run could pass $1.00: a stops, pods pauses until the next day.
    let mut typed = h.typing.typed();
    typed.sort();
    assert_eq!(
        typed,
        [
            "WATCH a stopped: daily budget of $1.00 spent",
            "WATCH pods paused: daily budget of $1.00 spent, resumes at 00:00",
        ]
    );
    assert_eq!(h.runner.calls(), 1);
    let next_day = next_day_start();
    let pods = h.row("pods").await;
    assert_eq!(pods.state, WatchState::Paused);
    assert_eq!(
        (pods.pause_how.as_deref(), pods.paused_until_ms),
        (Some("budget_day"), Some(next_day))
    );
    let listing = h.scheduler.list().await.unwrap();
    let listed = listing.watches.iter().find(|w| w.name == "pods").unwrap();
    assert_eq!(listed.state, WatchState::Paused);
    assert_eq!(listed.paused_until_ms, Some(next_day));
    assert_eq!(
        h.scheduler.start(h.checked("c", 10, 60)).await,
        Err(StartRefusal::DailyBudgetSpent)
    );

    // Nothing runs until the next day; then pods resumes by itself and runs once at once.
    pass(Duration::from_millis((next_day - clock_ms()) as u64) - Duration::from_secs(1)).await;
    assert_eq!(h.runner.calls(), 1);
    pass(Duration::from_secs(5)).await;
    assert_eq!(h.runner.calls(), 2, "one run after the day change");
    let pods = h.row("pods").await;
    assert_eq!((pods.state, pods.runs), (WatchState::Running, 1));

    // Its next run could pass the new day's budget: it pauses until the day after.
    pass(10 * MINUTE).await;
    assert_eq!(h.runner.calls(), 2);
    let pods = h.row("pods").await;
    assert_eq!(pods.state, WatchState::Paused);
    assert_eq!(pods.paused_until_ms, Some(next_day + 86_400_000));
    assert_eq!(h.typing.typed().len(), 3);
}

#[tokio::test(start_paused = true)]
async fn a_capped_watch_whose_run_could_pass_the_daily_budget_pauses_again_each_day() {
    let h = harness(with_daily(0.5), vec![answer("QUIET", 0.01)]).await;
    start_just_after_midnight().await;
    h.start_capped("pods").await;
    let first_day_end = next_day_start();

    pass(Duration::from_millis((first_day_end - clock_ms()) as u64) + MINUTE).await;

    assert_eq!(h.runner.calls(), 0, "no run passes the daily budget");
    let pods = h.row("pods").await;
    assert_eq!(pods.state, WatchState::Paused);
    assert_eq!(pods.paused_until_ms, Some(first_day_end + 86_400_000));
    assert_eq!(
        h.typing.typed(),
        [
            "WATCH pods paused: daily budget of $0.50 spent, resumes at 00:00",
            "WATCH pods paused: daily budget of $0.50 spent, resumes at 00:00",
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn resume_and_pause_refuse_budget_day() {
    let h = harness(with_daily(0.5), vec![answer("QUIET", 0.01)]).await;
    start_just_after_midnight().await;
    h.start_capped("pods").await;

    assert_eq!(
        h.scheduler.resume_watches(resume_of("pods")).await,
        Err(ResumeRefusal::BudgetDay)
    );
    assert_eq!(
        h.scheduler.pause(pause_of("pods", Some(30))).await,
        Err(PauseRefusal::BudgetDay)
    );
    let pods = h.row("pods").await;
    assert_eq!(pods.pause_how.as_deref(), Some("budget_day"));
    assert_eq!(pods.paused_until_ms, Some(next_day_start()));
    // A capped watch keeps today's rule for a new watch, as a budget stop does.
    assert_eq!(
        h.scheduler.start(h.checked("other", 10, 60)).await,
        Err(StartRefusal::DailyBudgetSpent)
    );
}

#[tokio::test(start_paused = true)]
async fn resume_all_skips_and_names_a_capped_watch() {
    let h = harness(with_daily(0.5), vec![answer("QUIET", 0.01)]).await;
    start_just_after_midnight().await;
    h.start_open("w", 10).await;
    pass(Duration::from_secs(5)).await;
    h.pause("w", None).await;
    h.start_capped("pods").await;

    // pause --all skips the capped watch and names it; w is already paused.
    assert_eq!(
        h.scheduler.pause(pause_all()).await,
        Ok(Changed {
            done: vec![],
            skipped: vec!["pods".into()],
        })
    );
    let changed = h.scheduler.resume_watches(resume_all()).await.unwrap();

    assert_eq!(changed.done, ["w"]);
    assert_eq!(changed.skipped, ["pods"]);
    assert_eq!(h.row("w").await.state, WatchState::Running);
    assert_eq!(h.row("pods").await.state, WatchState::Paused);
}

#[tokio::test(start_paused = true)]
async fn pause_stops_runs_and_spend_until_resume() {
    let h = harness(with_daily(100.0), vec![answer("QUIET", 0.01)]).await;
    start_just_after_midnight().await;
    h.start_open("pods", 10).await;
    pass(5 * MINUTE).await;
    h.pause("pods", None).await;
    let spent = h.scheduler.list().await.unwrap().spent_usd_micros;

    pass(120 * MINUTE).await;

    assert_eq!(h.runner.calls(), 1, "a paused watch runs nothing");
    assert_eq!(h.scheduler.list().await.unwrap().spent_usd_micros, spent);
    let row = h.row("pods").await;
    assert_eq!(row.state, WatchState::Paused);
    assert_eq!(
        (row.paused_until_ms, row.pause_how.as_deref()),
        (None, Some("cli"))
    );

    h.resume("pods").await;
    pass(Duration::from_secs(5)).await;
    assert_eq!(h.runner.calls(), 2);
    let row = h.row("pods").await;
    assert_eq!((row.state, row.runs), (WatchState::Running, 2));
}

#[tokio::test(start_paused = true)]
async fn a_long_pause_then_resume_runs_exactly_once() {
    let h = harness(settings(), vec![answer("QUIET", 0.01)]).await;
    h.start("w", 10, 600).await;
    pass(5 * MINUTE).await;
    let started = h.row("w").await.started_at_ms;
    h.pause("w", None).await;
    pass(180 * MINUTE).await;

    h.resume("w").await;
    pass(4 * MINUTE).await;

    assert_eq!(
        h.runner.calls(),
        2,
        "the resume runs once; missed runs do not catch up"
    );
    let row = h.row("w").await;
    assert_eq!((row.runs, row.started_at_ms), (2, started));
    assert_eq!(
        row.next_run_at_ms,
        started + 190 * MINUTE_MS,
        "the grid stays"
    );
    pass(2 * MINUTE).await;
    assert_eq!(h.runner.calls(), 3);
}

#[tokio::test(start_paused = true)]
async fn pause_for_resumes_at_its_end_with_a_run_at_once() {
    let h = harness(with_daily(100.0), vec![answer("QUIET", 0.01)]).await;
    h.start_open("pods", 10).await;
    pass(5 * MINUTE).await;
    h.pause("pods", Some(30)).await;
    assert_eq!(
        h.row("pods").await.paused_until_ms,
        Some(clock_ms() + 30 * MINUTE_MS)
    );

    pass(29 * MINUTE).await;
    assert_eq!(h.runner.calls(), 1);
    pass(MINUTE + Duration::from_secs(5)).await;

    assert_eq!(h.runner.calls(), 2, "a run at once at the end of the pause");
    let row = h.row("pods").await;
    assert_eq!(row.state, WatchState::Running);
    assert_eq!((row.paused_until_ms, row.pause_how), (None, None));
    assert_eq!(row.runs, 2);
}

#[tokio::test(start_paused = true)]
async fn pause_all_pauses_every_running_watch() {
    let h = harness(settings(), vec![answer("QUIET", 0.01)]).await;
    h.start("a", 10, 120).await;
    h.start("b", 10, 120).await;
    pass(Duration::from_secs(5)).await;
    assert_eq!(h.runner.calls(), 2);

    let changed = h.scheduler.pause(pause_all()).await.unwrap();

    assert_eq!(changed.done, ["a", "b"]);
    assert!(changed.skipped.is_empty());
    pass(60 * MINUTE).await;
    assert_eq!(h.runner.calls(), 2);
    for name in ["a", "b"] {
        assert_eq!(h.row(name).await.state, WatchState::Paused);
    }
    // A watch already paused is skipped and not named.
    assert_eq!(h.scheduler.pause(pause_all()).await, Ok(Changed::default()));
}

#[tokio::test(start_paused = true)]
async fn a_watch_with_an_end_ends_while_paused() {
    let h = harness(settings(), vec![answer("QUIET", 0.01)]).await;
    h.start("w", 10, 60).await;
    pass(15 * MINUTE).await;
    h.pause("w", Some(120)).await;

    pass(50 * MINUTE).await;

    assert_eq!(h.runner.calls(), 2);
    assert_eq!(
        h.typing.typed(),
        ["WATCH w ended: 2 runs, 0 reports, $0.02"]
    );
    let row = h.row("w").await;
    assert_eq!(
        (row.state, row.stop_reason.as_deref()),
        (WatchState::Stopped, Some("ended"))
    );
    pass(120 * MINUTE).await;
    assert_eq!(h.runner.calls(), 2);
}

#[tokio::test(start_paused = true)]
async fn the_paused_state_survives_a_restart() {
    let h = harness(with_daily(100.0), vec![answer("QUIET", 0.01)]).await;
    start_just_after_midnight().await;
    for name in ["held", "timed", "short"] {
        h.start_open(name, 10).await;
    }
    pass(5 * MINUTE).await;
    h.pause("held", None).await;
    h.pause("timed", Some(60)).await;
    h.pause("short", Some(10)).await;
    let until = h.row("timed").await.paused_until_ms;
    let _ = h.stop.send(());
    pass(Duration::from_secs(1)).await;
    // Down for 20 minutes: the pause of "short" ends meanwhile.
    pass(20 * MINUTE).await;

    let (scheduler, runner, _stop) = restarted(&h);
    scheduler.resume().await;
    pass(Duration::from_secs(5)).await;
    assert_eq!(runner.calls(), 1, "short resumes at once");
    assert_eq!(h.row("short").await.state, WatchState::Running);
    let held = h.row("held").await;
    assert_eq!(
        (held.state, held.paused_until_ms),
        (WatchState::Paused, None)
    );
    let timed = h.row("timed").await;
    assert_eq!(
        (timed.state, timed.paused_until_ms),
        (WatchState::Paused, until)
    );

    // "timed" resumes at its end with a run at once; "held" waits for a resume.
    pass(Duration::from_millis((until.unwrap() - clock_ms()) as u64) + Duration::from_secs(5))
        .await;
    assert_eq!(h.row("timed").await.state, WatchState::Running);
    assert_eq!(h.row("timed").await.runs, 2);
    assert_eq!(h.row("held").await.state, WatchState::Paused);
    assert_eq!(h.row("held").await.runs, 1);
}

#[tokio::test(start_paused = true)]
async fn pause_drops_a_waiting_report_and_stops_a_run_in_progress() {
    let h = harness(with_daily(100.0), vec![answer("REPORT x", 0.01)]).await;
    h.agents.set_state(CHIEF_PANE, EffectiveAgentState::Working);
    h.start_open("w", 10).await;
    pass(MINUTE).await;
    h.pause("w", None).await;
    h.agents.set_state(CHIEF_PANE, EffectiveAgentState::Idle);
    pass(MINUTE).await;
    assert!(
        h.typing.typed().is_empty(),
        "the waiting report was dropped"
    );

    let h = harness(
        with_daily(100.0),
        vec![Step::Answer {
            result: "REPORT late".into(),
            cost: 0.01,
            after: Duration::from_secs(50),
        }],
    )
    .await;
    h.start_open("w", 10).await;
    pass(Duration::from_secs(10)).await;
    assert_eq!(h.runner.calls(), 1);
    h.pause("w", None).await;
    pass(5 * MINUTE).await;

    assert!(h.typing.typed().is_empty(), "the stopped run typed nothing");
    let row = h.row("w").await;
    assert_eq!((row.state, row.runs), (WatchState::Paused, 0));
    assert_eq!(h.runner.calls(), 1);
}

#[tokio::test(start_paused = true)]
async fn pause_and_resume_type_no_line() {
    let h = harness(with_daily(100.0), vec![answer("QUIET", 0.01)]).await;
    h.start_open("w", 10).await;
    h.start_open("t", 10).await;
    pass(Duration::from_secs(5)).await;
    h.pause("w", None).await;
    h.pause("t", Some(5)).await;
    pass(MINUTE).await;
    h.resume("w").await;
    pass(10 * MINUTE).await;

    assert_eq!(h.row("t").await.state, WatchState::Running);
    assert_eq!(h.row("w").await.state, WatchState::Running);
    assert!(h.typing.typed().is_empty(), "{:?}", h.typing.typed());
}

#[tokio::test(start_paused = true)]
async fn a_paused_watch_counts_toward_five_and_start_replaces_it() {
    let h = harness(settings(), vec![answer("QUIET", 0.01)]).await;
    for index in 0..5 {
        h.start(&format!("w{index}"), 10, 120).await;
    }
    pass(Duration::from_secs(5)).await;
    h.pause("w0", None).await;

    assert_eq!(
        h.scheduler.start(h.checked("w5", 10, 120)).await,
        Err(StartRefusal::TooManyWatches)
    );
    h.start("w0", 10, 120).await;
    pass(Duration::from_secs(5)).await;
    let row = h.row("w0").await;
    assert_eq!((row.state, row.runs), (WatchState::Running, 1));
}

#[tokio::test(start_paused = true)]
async fn unknown_watch() {
    let h = harness(settings(), vec![answer("QUIET", 0.01)]).await;
    assert_eq!(
        h.scheduler.pause(pause_of("nope", None)).await,
        Err(PauseRefusal::UnknownWatch)
    );
    assert_eq!(
        h.scheduler.resume_watches(resume_of("nope")).await,
        Err(ResumeRefusal::UnknownWatch)
    );
}

#[tokio::test(start_paused = true)]
async fn not_running() {
    let h = harness(settings(), vec![answer("QUIET", 0.01)]).await;
    h.start("paused", 10, 120).await;
    h.start("stopped", 10, 120).await;
    pass(Duration::from_secs(5)).await;
    h.pause("paused", None).await;
    h.scheduler.stop("stopped").await.unwrap();

    for name in ["paused", "stopped"] {
        assert_eq!(
            h.scheduler.pause(pause_of(name, None)).await,
            Err(PauseRefusal::NotRunning),
            "{name}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn not_paused() {
    let h = harness(settings(), vec![answer("QUIET", 0.01)]).await;
    h.start("running", 10, 120).await;
    h.start("stopped", 10, 120).await;
    pass(Duration::from_secs(5)).await;
    h.scheduler.stop("stopped").await.unwrap();

    for name in ["running", "stopped"] {
        assert_eq!(
            h.scheduler.resume_watches(resume_of(name)).await,
            Err(ResumeRefusal::NotPaused),
            "{name}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn stop_ends_a_paused_watch() {
    let h = harness(settings(), vec![answer("QUIET", 0.01)]).await;
    h.start("w", 10, 120).await;
    pass(Duration::from_secs(5)).await;
    h.pause("w", Some(30)).await;

    h.scheduler.stop("w").await.unwrap();
    pass(60 * MINUTE).await;

    let row = h.row("w").await;
    assert_eq!(
        (row.state, row.stop_reason.as_deref()),
        (WatchState::Stopped, Some("stopped"))
    );
    assert_eq!(h.runner.calls(), 1);
    assert!(h.typing.typed().is_empty());
}

/// The lines of `message` in `text` for the watch `name`.
fn log_lines<'a>(text: &'a str, message: &str, name: &str) -> Vec<&'a str> {
    text.lines()
        .filter(|line| line.contains(message) && line.contains(&format!("watch=\"{name}\"")))
        .collect()
}

#[tokio::test(start_paused = true)]
async fn a_capped_watch_logs_budget_day_when_it_pauses_and_when_it_resumes_by_itself() {
    let h = harness(with_daily(0.5), vec![answer("QUIET", 0.01)]).await;
    start_just_after_midnight().await;
    let logs = crate::outbound::fake::CapturedLogs::start();
    h.start_capped("pods").await;
    let first_day_end = next_day_start();

    pass(Duration::from_millis((first_day_end - clock_ms()) as u64) + MINUTE).await;

    let text = logs.text();
    let paused = log_lines(&text, "watch paused", "pods");
    assert_eq!(
        paused.len(),
        2,
        "paused at the start and again the next day: {text}"
    );
    for line in &paused {
        assert!(line.contains("how=\"budget_day\""), "{line}");
        assert!(line.contains("paused_until=\""), "{line}");
    }
    let resumed = log_lines(&text, "watch resumed", "pods");
    assert_eq!(resumed.len(), 1, "{text}");
    assert!(resumed[0].contains("how=\"budget_day\""), "{}", resumed[0]);
    assert!(!resumed[0].contains("paused_until"), "{}", resumed[0]);
}

#[tokio::test(start_paused = true)]
async fn a_capped_watch_stays_paused_over_a_restart_and_resumes_at_the_next_day_start() {
    let h = harness(with_daily(0.5), vec![answer("QUIET", 0.01)]).await;
    start_just_after_midnight().await;
    h.start_capped("pods").await;
    let day_end = next_day_start();
    let _ = h.stop.send(());
    pass(Duration::from_secs(1)).await;

    let logs = crate::outbound::fake::CapturedLogs::start();
    let (scheduler, runner, _stop) = restarted(&h);
    scheduler.resume().await;
    pass(Duration::from_secs(5)).await;
    let pods = h.row("pods").await;
    assert_eq!(pods.state, WatchState::Paused);
    assert_eq!(pods.pause_how.as_deref(), Some("budget_day"));
    assert_eq!(pods.paused_until_ms, Some(day_end));

    pass(Duration::from_millis((day_end - clock_ms()) as u64) - Duration::from_secs(1)).await;
    assert_eq!(h.row("pods").await.state, WatchState::Paused);
    assert_eq!(runner.calls(), 0);
    assert!(log_lines(&logs.text(), "watch resumed", "pods").is_empty());

    pass(Duration::from_secs(5)).await;
    let text = logs.text();
    let resumed = log_lines(&text, "watch resumed", "pods");
    assert_eq!(resumed.len(), 1, "{text}");
    assert!(resumed[0].contains("how=\"budget_day\""), "{}", resumed[0]);
    assert_eq!(h.row("pods").await.pause_how.as_deref(), None);
}

// ---------------------------------------------------------------------------
// A computer sleep: the wall clock jumps and tokio's clock does not
// ---------------------------------------------------------------------------

/// One task wake-up and a few seconds more, so a task that wakes every minute sees a jump.
const AFTER_A_WAKE: Duration = Duration::from_secs(65);

#[tokio::test(start_paused = true)]
async fn pause_for_resumes_at_its_wall_clock_end_after_a_computer_sleep() {
    let h = harness(with_daily(100.0), vec![answer("QUIET", 0.01)]).await;
    h.start_open("pods", 10).await;
    pass(5 * MINUTE).await;
    h.pause("pods", Some(30)).await;
    pass(MINUTE).await;

    clock::jump_wall_clock(40 * MINUTE_MS);
    pass(AFTER_A_WAKE).await;

    let row = h.row("pods").await;
    assert_eq!(row.state, WatchState::Running);
    assert_eq!(h.runner.calls(), 2, "a run at once at the end of the pause");
}

#[tokio::test(start_paused = true)]
async fn a_capped_watch_resumes_at_the_next_local_day_after_a_computer_sleep() {
    let h = harness(with_daily(0.5), vec![answer("QUIET", 0.01)]).await;
    start_just_after_midnight().await;
    let logs = crate::outbound::fake::CapturedLogs::start();
    h.start_capped("pods").await;
    let day_end = next_day_start();

    clock::jump_wall_clock(day_end - clock_ms() + MINUTE_MS);
    pass(AFTER_A_WAKE).await;

    let resumed = log_lines(&logs.text(), "watch resumed", "pods").len();
    assert_eq!(resumed, 1, "it resumed at the new day");
    // Its run could still pass the new day's budget: it caps again until the day after.
    let pods = h.row("pods").await;
    assert_eq!(pods.paused_until_ms, Some(day_end + 86_400_000));
}

#[tokio::test(start_paused = true)]
async fn a_watch_with_an_end_ends_while_paused_after_a_computer_sleep() {
    let h = harness(settings(), vec![answer("QUIET", 0.01)]).await;
    h.start("w", 10, 60).await;
    pass(15 * MINUTE).await;
    h.pause("w", None).await;
    pass(MINUTE).await;

    clock::jump_wall_clock(60 * MINUTE_MS);
    pass(AFTER_A_WAKE).await;

    let row = h.row("w").await;
    assert_eq!(
        (row.state, row.stop_reason.as_deref()),
        (WatchState::Stopped, Some("ended"))
    );
    assert_eq!(
        h.typing.typed(),
        ["WATCH w ended: 2 runs, 0 reports, $0.02"]
    );
}

#[tokio::test(start_paused = true)]
async fn a_running_watch_runs_at_its_wall_clock_time_after_a_computer_sleep() {
    let h = harness(with_daily(100.0), vec![answer("QUIET", 0.01)]).await;
    h.start_open("pods", 240).await;
    pass(Duration::from_secs(5)).await;
    assert_eq!(h.runner.calls(), 1);

    clock::jump_wall_clock(240 * MINUTE_MS);
    pass(AFTER_A_WAKE).await;

    assert_eq!(h.runner.calls(), 2, "the run at 240 minutes is not late");
}

// ---------------------------------------------------------------------------
// A pause or a resume that loses a race
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_named_pause_that_loses_a_race_is_not_running_and_all_skips_it() {
    let h = harness(settings(), vec![answer("QUIET", 0.01)]).await;
    h.start("w", 10, 120).await;
    pass(Duration::from_secs(5)).await;
    // The row as pause read it; then the watch stops before the pause is stored.
    let read = h.row("w").await;
    h.scheduler.stop("w").await.unwrap();

    assert_eq!(
        h.scheduler
            .pause_rows(vec![read.clone()], true, None, How::Cli)
            .await,
        Err(PauseRefusal::NotRunning)
    );
    assert_eq!(
        h.scheduler
            .pause_rows(vec![read], false, None, How::Cli)
            .await,
        Ok(vec![])
    );
    assert_eq!(h.row("w").await.state, WatchState::Stopped);
}

#[tokio::test(start_paused = true)]
async fn a_named_resume_that_loses_a_race_is_not_paused_and_all_skips_it() {
    let h = harness(settings(), vec![answer("QUIET", 0.01)]).await;
    h.start("w", 10, 120).await;
    pass(Duration::from_secs(5)).await;
    h.pause("w", None).await;
    // The row as resume read it; then another resume comes first.
    let read = h.row("w").await;
    h.resume("w").await;

    assert_eq!(
        h.scheduler
            .resume_rows(vec![read.clone()], true, How::Cli)
            .await,
        Err(ResumeRefusal::NotPaused)
    );
    assert_eq!(
        h.scheduler.resume_rows(vec![read], false, How::Cli).await,
        Ok(vec![])
    );
    assert_eq!(h.row("w").await.state, WatchState::Running);
}

#[tokio::test(start_paused = true)]
async fn local_now_is_the_local_date_and_seconds_of_day_at_the_offset_now() {
    for (hours, minutes) in [(5, 30), (-3, 0), (14, 0)] {
        let offset = UtcOffset::from_hms(hours, minutes, 0).unwrap();
        let local = ScriptedOffset::at(offset);
        let h = harness_with_offset(&local).await;

        let now = h.scheduler.local_now().await;

        let local_secs = clock_ms().div_euclid(1000) + i64::from(offset.whole_seconds());
        let seconds_of_day = local_secs.rem_euclid(86_400) as u32;
        assert_eq!(now.seconds_of_day, seconds_of_day, "{offset}");
        let day = local_text(clock_ms(), offset)[..10].to_owned();
        assert_eq!(now.day, day, "{offset}");
        assert_eq!(now.day.len(), 10, "YYYY-MM-DD");
    }
}

/// Sets one column of the watch row `name` through a second connection to `watch.db`.
async fn set_column(h: &Harness, name: &str, assignment: &str) {
    let path = h._data.0.join("watch.db");
    let database = turso::Builder::new_local(path.to_str().unwrap())
        .build()
        .await
        .unwrap();
    let conn = database.connect().unwrap();
    conn.execute(
        &format!("UPDATE watches SET {assignment} WHERE name = ?1"),
        [name],
    )
    .await
    .unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_failed_store_read_is_logged_and_tried_again_and_the_watch_goes_on() {
    let h = harness(settings(), vec![answer("QUIET", 0.01)]).await;
    h.start("w", 10, 120).await;
    pass(Duration::from_secs(5)).await;
    assert_eq!(h.runner.calls(), 1);
    let logs = crate::outbound::fake::CapturedLogs::start();

    // A text in an integer column: each read of the row fails.
    set_column(&h, "w", "runs = 'x'").await;
    pass(5 * MINUTE).await;
    assert!(
        !log_lines(&logs.text(), "watch store read failed", "w").is_empty(),
        "{}",
        logs.text()
    );
    set_column(&h, "w", "runs = 1").await;
    pass(6 * MINUTE).await;

    assert_eq!(
        h.runner.calls(),
        2,
        "the task lived on and ran at 10 minutes"
    );
    let line = log_lines(&logs.text(), "watch store read failed", "w")[0].to_owned();
    assert!(line.contains("reason=\"store_failed\""), "{line}");
}

// -----------------------------------------------------------------------
// The counts the watch tool pushes to the host
// -----------------------------------------------------------------------

fn counts_of(h: &Harness) -> crate::tools::WatchCounts {
    *h.scheduler.deps.counts.borrow()
}

fn counts(running: u32, paused: u32, capped: u32) -> crate::tools::WatchCounts {
    crate::tools::WatchCounts {
        running,
        paused,
        capped,
    }
}

#[tokio::test(start_paused = true)]
async fn the_counts_follow_start_pause_resume_cap_and_stop() {
    let h = harness(with_daily(0.5), vec![answer("QUIET", 0.01)]).await;
    start_just_after_midnight().await;
    assert_eq!(counts_of(&h), counts(0, 0, 0));

    h.start_open("w", 10).await;
    pass(Duration::from_secs(5)).await;
    assert_eq!(counts_of(&h), counts(1, 0, 0), "start");
    h.pause("w", None).await;
    assert_eq!(counts_of(&h), counts(0, 1, 0), "pause");
    h.resume("w").await;
    assert_eq!(counts_of(&h), counts(1, 0, 0), "resume");
    // The budget check caps every running watch with no end, so "w" is capped too.
    h.start_capped("pods").await;
    assert_eq!(counts_of(&h), counts(0, 0, 2), "cap");
    h.scheduler.stop("w").await.unwrap();
    assert_eq!(counts_of(&h), counts(0, 0, 1), "stop of a capped watch");
    h.scheduler.stop("pods").await.unwrap();
    assert_eq!(
        counts_of(&h),
        counts(0, 0, 0),
        "stop of the last capped watch"
    );
}

#[tokio::test(start_paused = true)]
async fn a_timed_pause_that_ends_changes_the_counts_without_a_call() {
    let h = harness(with_daily(0.5), vec![answer("QUIET", 0.01)]).await;
    start_just_after_midnight().await;
    h.start_open("w", 10).await;
    pass(Duration::from_secs(5)).await;
    h.pause("w", Some(30)).await;
    assert_eq!(counts_of(&h), counts(0, 1, 0));

    pass(31 * MINUTE).await;

    assert_eq!(counts_of(&h), counts(1, 0, 0), "the pause ended by itself");
}

#[tokio::test(start_paused = true)]
async fn a_restart_sends_the_counts_of_the_stored_watches() {
    let h = harness(with_daily(0.5), vec![answer("QUIET", 0.01)]).await;
    start_just_after_midnight().await;
    h.start_open("w", 10).await;
    pass(Duration::from_secs(5)).await;
    h.pause("w", None).await;
    h.start_capped("pods").await;
    let (_stop, shutdown) = signal::channel(());

    let again = build(
        &h.store,
        &h.runner,
        &h.typing,
        &h.agents,
        &h.chief,
        &h.pods,
        with_daily(0.5),
        shutdown,
    );
    assert_eq!(
        *again.deps.counts.borrow(),
        counts(0, 0, 0),
        "before resume"
    );
    again.resume().await;

    assert_eq!(*again.deps.counts.borrow(), counts(0, 1, 1));
}

/// A log buffer at debug level for this thread. The shared test subscriber keeps info and
/// above, and `watch counts sent` is a debug line.
#[derive(Clone, Default)]
struct DebugBuffer(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for DebugBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl DebugBuffer {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

#[tokio::test(start_paused = true)]
async fn each_changed_count_is_logged_once_and_an_equal_count_is_not() {
    let h = harness(with_daily(0.5), vec![answer("QUIET", 0.01)]).await;
    start_just_after_midnight().await;
    let buffer = DebugBuffer::default();
    let writer = buffer.clone();
    let _debug = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish(),
    );
    h.start_open("w", 10).await;
    pass(Duration::from_secs(5)).await;
    h.pause("w", None).await;
    h.resume("w").await;
    // A replacement keeps the counts, so it logs no new line.
    h.start_open("w", 10).await;
    pass(Duration::from_secs(5)).await;

    let text = buffer.text();
    let sent: Vec<&str> = text
        .lines()
        .filter(|line| line.contains("watch counts sent"))
        .collect();
    assert_eq!(sent.len(), 3, "start, pause, resume: {text}");
    for (line, (running, paused)) in sent.iter().zip([(1, 0), (0, 1), (1, 0)]) {
        assert!(line.starts_with("DEBUG"), "{line}");
        assert!(line.contains(&format!("running={running}")), "{line}");
        assert!(line.contains(&format!("paused={paused}")), "{line}");
        assert!(line.contains("capped=0"), "{line}");
        for word in ["watch=", "text=", "folder="] {
            assert!(!line.contains(word), "{word} in {line}");
        }
    }
// ---------------------------------------------------------------------------
// Cron watches
// ---------------------------------------------------------------------------

use crate::cron::{Cron, LocalMinute};
use crate::tools::watch::obs::RunDue;

fn hours(count: i8) -> UtcOffset {
    UtcOffset::from_hms(count, 0, 0).unwrap()
}

/// The instant the local clock at `offset` reads `text` (`YYYY-MM-DD HH:MM`).
fn instant(text: &str, offset: UtcOffset) -> i64 {
    clock::local_ms(text, offset).unwrap()
}

/// Moves the test wall clock to `at_ms`, forward or back.
fn set_wall_clock(at_ms: i64) {
    clock::jump_wall_clock(at_ms - clock_ms());
}

fn minute(text: &str) -> LocalMinute {
    LocalMinute::parse(text).unwrap()
}

fn now_at(text: &str) -> LocalNow {
    parse_local_now(text).unwrap()
}

impl Harness {
    fn cron_checked(&self, name: &str, cron: &str, minutes: u32) -> Checked {
        Checked {
            every_minutes: 0,
            cron: Some(Cron::parse(cron).unwrap()),
            ..self.checked(name, 10, minutes)
        }
    }

    async fn start_cron(&self, name: &str, cron: &str, minutes: u32) {
        self.scheduler
            .start(self.cron_checked(name, cron, minutes))
            .await
            .unwrap();
    }

    async fn checked_local(&self, name: &str) -> Option<String> {
        self.row(name).await.checked_local
    }
}

#[test]
fn a_cron_watch_has_no_grid_and_no_total() {
    assert_eq!(next_on_grid(1_000, 0, 5_000), NO_RUN_DUE);
    let mut row = WatchRow {
        name: "w".into(),
        folder: "/x".into(),
        skill: "s".into(),
        report_to: "chief".into(),
        every_minutes: 0,
        every_run: false,
        run_budget_usd_micros: None,
        run_limit_secs: 120,
        running: true,
        state: WatchState::Running,
        started_at_ms: 0,
        ends_at_ms: Some(60 * MINUTE_MS),
        next_run_at_ms: NO_RUN_DUE,
        runs: 0,
        reports: 0,
        failed_in_a_row: 0,
        cost_usd_micros: 0,
        last_outcome: None,
        stop_reason: None,
        ended_at_ms: None,
        paused_until_ms: None,
        pause_how: None,
        cron: Some("*/15 * * * *".into()),
        checked_local: None,
    };
    assert_eq!(total_runs(&row), None);
    assert_eq!(
        report_line("w", 3, total_runs(&row), "quiet"),
        "WATCH w 3: quiet"
    );
    (row.every_minutes, row.cron) = (10, None);
    assert_eq!(total_runs(&row), Some(6));
}

#[test]
fn cron_check_runs_the_newest_match_with_missed_and_late_s() {
    let cron = Cron::parse("*/10 * * * *").unwrap();
    let step = cron_check(
        &cron,
        Some(minute("2026-11-03 12:00")),
        &now_at("2026-11-03 12:36:20"),
        false,
    );
    assert_eq!(step.checked, minute("2026-11-03 12:36"));
    assert_eq!(
        step.run,
        Some(RunDue {
            matched: Some(minute("2026-11-03 12:30")),
            missed: 2,
            late_s: 6 * 60 + 20,
        })
    );
    assert_eq!(step.missed_all, 0);
}

#[test]
fn cron_check_past_the_window_runs_nothing_and_counts_the_missed_matches() {
    let cron = Cron::parse("5 12 * * *").unwrap();
    let step = cron_check(
        &cron,
        Some(minute("2026-11-03 12:00")),
        &now_at("2026-11-03 15:01:00"),
        false,
    );
    assert_eq!(step.checked, minute("2026-11-03 15:01"));
    assert_eq!(step.run, None);
    assert_eq!(step.missed_all, 1);
}

#[test]
fn cron_check_with_no_checked_minute_starts_from_now_and_runs_nothing() {
    let cron = Cron::parse("*/10 * * * *").unwrap();
    let step = cron_check(&cron, None, &now_at("2026-11-03 12:30:05"), false);
    assert_eq!(step.checked, minute("2026-11-03 12:30"));
    assert_eq!((step.run, step.missed_all), (None, 0));
}

#[test]
fn cron_check_runs_a_resume_at_once_and_never_moves_the_checked_minute_back() {
    let cron = Cron::parse("*/10 * * * *").unwrap();
    let step = cron_check(
        &cron,
        Some(minute("2026-11-03 12:34")),
        &now_at("2026-11-03 12:35:10"),
        true,
    );
    assert_eq!(step.checked, minute("2026-11-03 12:35"));
    assert_eq!(
        step.run,
        Some(RunDue {
            matched: None,
            missed: 0,
            late_s: 0,
        })
    );

    // Daylight saving ends: the clock reads 02:30 again after 02:59 was checked.
    let daily = Cron::parse("30 2 * * *").unwrap();
    let step = cron_check(
        &daily,
        Some(minute("2027-04-04 02:59")),
        &now_at("2027-04-04 02:30:00"),
        false,
    );
    assert_eq!(step.checked, minute("2027-04-04 02:59"));
    assert_eq!((step.run, step.missed_all), (None, 0));

    // Daylight saving starts: 01:59 then 03:00 holds 02:30, so it runs, 1800 s late.
    let step = cron_check(
        &daily,
        Some(minute("2026-10-04 01:59")),
        &now_at("2026-10-04 03:00:04"),
        false,
    );
    assert_eq!(
        step.run,
        Some(RunDue {
            matched: Some(minute("2026-10-04 02:30")),
            missed: 0,
            late_s: 1804,
        })
    );
}

#[test]
fn an_every_run_is_late_from_its_slot() {
    assert_eq!(
        RunDue::every(10 * MINUTE_MS, 10 * MINUTE_MS + 7_500),
        RunDue {
            matched: None,
            missed: 0,
            late_s: 7,
        }
    );
    assert_eq!(RunDue::every(10 * MINUTE_MS, 0).late_s, 0);
}

#[tokio::test(start_paused = true)]
async fn a_cron_watch_does_not_run_at_start_and_runs_at_each_match() {
    let h = harness(settings(), vec![answer("QUIET", 0.01)]).await;
    set_wall_clock(instant("2026-11-03 12:00", UtcOffset::UTC));
    h.start_cron("w", "*/10 * * * *", 600).await;
    let row = h.row("w").await;
    assert_eq!(
        (
            row.every_minutes,
            row.cron.as_deref(),
            row.checked_local.as_deref()
        ),
        (0, Some("*/10 * * * *"), Some("2026-11-03 12:00"))
    );
    pass(9 * MINUTE + Duration::from_secs(55)).await;
    assert_eq!(h.runner.calls(), 0, "no run at start");
    pass(Duration::from_secs(10)).await;
    assert_eq!(h.runner.calls(), 1, "the 12:10 match");
    pass(10 * MINUTE).await;
    assert_eq!(h.runner.calls(), 2, "the 12:20 match");
    assert_eq!(h.row("w").await.runs, 2);
}

#[tokio::test(start_paused = true)]
async fn cron_sydney_skip_runs_once() {
    let local = ScriptedOffset::at(hours(10));
    let h = harness_on(settings(), vec![answer("QUIET", 0.01)], &local).await;
    set_wall_clock(instant("2026-10-04 01:50", hours(10)));
    h.start_cron("digest", "30 2 * * *", 1440).await;

    pass(9 * MINUTE + Duration::from_millis(59_500)).await;
    assert_eq!(h.runner.calls(), 0);
    // 01:59:59.5, then the clock goes from 02:00 to 03:00.
    local.set(hours(11));
    pass(Duration::from_secs(5)).await;
    assert_eq!(h.runner.calls(), 1, "02:30 runs at 03:00");
    pass(120 * MINUTE).await;
    assert_eq!(h.runner.calls(), 1, "and only once");
}

#[tokio::test(start_paused = true)]
async fn cron_sydney_repeat_runs_once() {
    let local = ScriptedOffset::at(hours(11));
    let h = harness_on(settings(), vec![answer("QUIET", 0.01)], &local).await;
    set_wall_clock(instant("2027-04-04 02:20", hours(11)));
    h.start_cron("digest", "30 2 * * *", 1440).await;

    pass(10 * MINUTE + Duration::from_secs(5)).await;
    assert_eq!(h.runner.calls(), 1, "02:30");
    pass(29 * MINUTE + Duration::from_millis(54_500)).await;
    // 02:59:59.5, then the clock goes back from 03:00 to 02:00.
    local.set(hours(10));
    pass(61 * MINUTE).await;
    assert_eq!(h.runner.calls(), 1, "the second 02:30 does not run");
    assert_eq!(
        h.checked_local("digest").await.as_deref(),
        Some("2027-04-04 03:00")
    );
}

#[tokio::test(start_paused = true)]
async fn a_sleep_over_three_matches_runs_once_for_the_newest() {
    let h = harness(settings(), vec![answer("QUIET", 0.01)]).await;
    set_wall_clock(instant("2026-11-03 12:00", UtcOffset::UTC));
    h.start_cron("w", "*/10 * * * *", 600).await;
    pass(Duration::from_secs(5)).await;

    clock::jump_wall_clock(35 * MINUTE_MS);
    pass(AFTER_A_WAKE).await;
    assert_eq!(h.runner.calls(), 1, "12:10, 12:20 and 12:30 give one run");
    pass(3 * MINUTE).await;
    assert_eq!(h.runner.calls(), 1);
    pass(2 * MINUTE).await;
    assert_eq!(h.runner.calls(), 2, "12:40");
}

#[tokio::test(start_paused = true)]
async fn a_sleep_past_120_minutes_runs_nothing() {
    let h = harness(settings(), vec![answer("QUIET", 0.01)]).await;
    set_wall_clock(instant("2026-11-03 12:00", UtcOffset::UTC));
    h.start_cron("w", "5 12 * * *", 600).await;
    pass(Duration::from_secs(5)).await;

    clock::jump_wall_clock(180 * MINUTE_MS);
    pass(AFTER_A_WAKE).await;
    assert_eq!(h.runner.calls(), 0, "12:05 is 3 hours old");
    assert_eq!(
        h.checked_local("w").await.as_deref(),
        Some("2026-11-03 15:01")
    );
    pass(60 * MINUTE).await;
    assert_eq!(h.runner.calls(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_restart_keeps_the_checked_minute_of_a_cron_watch() {
    let h = harness(settings(), vec![answer("QUIET", 0.01)]).await;
    set_wall_clock(instant("2026-11-03 12:00", UtcOffset::UTC));
    h.start_cron("w", "*/10 * * * *", 600).await;
    pass(5 * MINUTE + Duration::from_secs(5)).await;
    assert_eq!(
        h.checked_local("w").await.as_deref(),
        Some("2026-11-03 12:05")
    );
    let _ = h.stop.send(());
    pass(Duration::from_secs(1)).await;
    // Down from 12:05 to 12:15: the 12:10 match is missed.
    pass(10 * MINUTE).await;
    assert_eq!(
        h.checked_local("w").await.as_deref(),
        Some("2026-11-03 12:05")
    );

    let (scheduler, runner, _stop) = restarted(&h);
    scheduler.resume().await;
    pass(Duration::from_secs(5)).await;
    assert_eq!(runner.calls(), 1, "the 12:10 match runs once, late");
    pass(4 * MINUTE).await;
    assert_eq!(runner.calls(), 1);
    pass(MINUTE).await;
    assert_eq!(runner.calls(), 2, "12:20");
}

#[tokio::test(start_paused = true)]
async fn a_cron_watch_paused_over_three_matches_then_resumed_runs_once() {
    let h = harness(settings(), vec![answer("QUIET", 0.01)]).await;
    set_wall_clock(instant("2026-11-03 12:00", UtcOffset::UTC));
    h.start_cron("w", "*/10 * * * *", 600).await;
    pass(5 * MINUTE).await;
    h.pause("w", None).await;
    // From 12:05 to 12:35: 12:10, 12:20 and 12:30 pass while paused.
    pass(30 * MINUTE + Duration::from_secs(5)).await;
    assert_eq!(h.runner.calls(), 0);
    assert_eq!(
        h.checked_local("w").await.as_deref(),
        Some("2026-11-03 12:35"),
        "the checked minute moves on while paused"
    );

    h.resume("w").await;
    pass(Duration::from_secs(5)).await;
    assert_eq!(h.runner.calls(), 1, "the resume runs once");
    pass(4 * MINUTE).await;
    assert_eq!(h.runner.calls(), 1, "no older match is added");
    pass(MINUTE).await;
    assert_eq!(h.runner.calls(), 2, "12:40");
}

#[tokio::test(start_paused = true)]
async fn a_cron_watch_with_an_end_ends_and_a_replacement_resets_the_checked_minute() {
    let h = harness(settings(), vec![answer("QUIET", 0.01)]).await;
    set_wall_clock(instant("2026-11-03 12:00", UtcOffset::UTC));
    h.start_cron("w", "*/10 * * * *", 25).await;
    pass(26 * MINUTE).await;
    let row = h.row("w").await;
    assert_eq!((row.runs, row.stop_reason.as_deref()), (2, Some("ended")));
    assert_eq!(
        h.typing.typed(),
        ["WATCH w ended: 2 runs, 0 reports, $0.02"]
    );

    h.start_cron("w", "*/10 * * * *", 25).await;
    assert_eq!(
        h.checked_local("w").await.as_deref(),
        Some("2026-11-03 12:26")
    );
}
