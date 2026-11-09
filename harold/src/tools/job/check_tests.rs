use std::sync::atomic::{AtomicI64, Ordering};

use super::*;
use crate::outbound::fake::CapturedLogs;
use crate::tools::Typing;
use crate::tools::job::store::{JobState, NewJob};
use crate::tools::job::test_support::{FakePane, FakeProcesses, Temp};

const START_S: i64 = 1_700_000_000;
const MIN: i64 = 60_000;

struct Rig {
    dir: Temp,
    store: Arc<JobStore>,
    processes: Arc<FakeProcesses>,
    pane: Arc<FakePane>,
    clock: Arc<AtomicI64>,
    checker: Checker,
}

fn checker(
    store: &Arc<JobStore>,
    processes: &Arc<FakeProcesses>,
    pane: &Arc<FakePane>,
    clock: &Arc<AtomicI64>,
) -> Checker {
    let clock = Arc::clone(clock);
    Checker::new(
        Arc::clone(store),
        processes.clone(),
        pane.clone(),
        Arc::new(move || clock.load(Ordering::SeqCst)),
        Arc::new(crate::tools::job::feed::JobsFeed::new()),
    )
}

async fn rig() -> Rig {
    let dir = Temp::new();
    let store = dir.store().await;
    let processes = Arc::new(FakeProcesses::default());
    processes.0.lock().unwrap().insert(4121, START_S);
    let pane = Arc::new(FakePane::default());
    let clock = Arc::new(AtomicI64::new(1_000 * MIN));
    let checker = checker(&store, &processes, &pane, &clock);
    Rig {
        dir,
        store,
        processes,
        pane,
        clock,
        checker,
    }
}

fn job(name: &str, started_ms: i64) -> NewJob {
    NewJob {
        name: name.into(),
        what: String::new(),
        pane_id: "%4".into(),
        pid: 4121,
        process_start_s: START_S,
        started_at_ms: started_ms,
        due_at_ms: started_ms + 20 * MIN,
    }
}

impl Rig {
    fn now(&self) -> i64 {
        self.clock.load(Ordering::SeqCst)
    }

    fn advance(&self, ms: i64) {
        self.clock.fetch_add(ms, Ordering::SeqCst);
    }

    async fn end_process(&self) {
        self.processes.0.lock().unwrap().remove(&4121);
    }

    async fn only_job(&self) -> JobRow {
        let all = self.store.shown(self.now()).await.unwrap();
        assert_eq!(all.len(), 1);
        all[0].clone()
    }
}

#[tokio::test]
async fn a_running_job_stays_running_and_types_nothing() {
    let r = rig().await;
    r.store.add(&job("calibration", r.now())).await.unwrap();
    let report = r.checker.pass().await;
    assert_eq!(r.only_job().await.state, JobState::Running);
    assert!(r.pane.lines().is_empty());
    assert!(!report.changed);
}

#[tokio::test]
async fn a_job_whose_process_ends_is_ended_at_the_next_check_and_the_line_is_typed_once() {
    let r = rig().await;
    r.store.add(&job("calibration", r.now())).await.unwrap();
    r.advance(21 * MIN);
    r.end_process().await;
    let report = r.checker.pass().await;
    assert!(report.changed);
    let ended = r.only_job().await;
    assert_eq!(ended.state, JobState::Ended);
    assert_eq!(ended.end_line.as_deref(), Some("typed"));
    assert_eq!(
        r.pane.lines(),
        vec![(
            "%4".to_string(),
            "JOB calibration ended after 21 m (pid 4121). Check its output.".to_string()
        )]
    );
    r.advance(15_000);
    r.checker.pass().await;
    assert_eq!(r.pane.lines().len(), 1, "typed once");
}

#[tokio::test]
async fn a_new_process_with_the_same_pid_and_a_new_start_time_does_not_keep_the_job_running() {
    let r = rig().await;
    r.store.add(&job("a", r.now())).await.unwrap();
    r.processes.0.lock().unwrap().insert(4121, START_S + 900);
    r.checker.pass().await;
    assert_eq!(r.only_job().await.state, JobState::Ended);
}

#[tokio::test]
async fn a_busy_pane_gets_the_line_at_a_later_check_and_it_is_marked_typed_at_turn_end() {
    let r = rig().await;
    r.store.add(&job("a", r.now())).await.unwrap();
    r.end_process().await;
    r.pane
        .answers
        .lock()
        .unwrap()
        .extend([Typing::PaneBusy("pane_busy"), Typing::PaneBusy("pane_busy")]);
    r.checker.pass().await;
    assert_eq!(r.only_job().await.end_line, None);
    r.advance(15_000);
    r.checker.pass().await;
    assert_eq!(r.only_job().await.end_line, None);
    r.advance(15_000);
    r.checker.pass().await;
    assert_eq!(r.only_job().await.end_line.as_deref(), Some("typed_at_turn_end"));
    r.advance(15_000);
    r.checker.pass().await;
    assert_eq!(r.pane.lines().len(), 3, "two busy tries, one typed line, then nothing");
}

#[tokio::test]
async fn a_gone_pane_gives_no_pane_and_nothing_is_typed_again() {
    let r = rig().await;
    r.store.add(&job("a", r.now())).await.unwrap();
    r.end_process().await;
    r.pane.answers.lock().unwrap().push_back(Typing::NoPane);
    r.checker.pass().await;
    assert_eq!(r.only_job().await.end_line.as_deref(), Some("no_pane"));
    r.advance(15_000);
    r.checker.pass().await;
    assert_eq!(r.pane.lines().len(), 1);
}

#[tokio::test]
async fn a_failed_enter_gives_failed_and_the_line_is_not_typed_again() {
    let r = rig().await;
    r.store.add(&job("a", r.now())).await.unwrap();
    r.end_process().await;
    r.pane.answers.lock().unwrap().push_back(Typing::EnterFailed);
    r.checker.pass().await;
    assert_eq!(r.only_job().await.end_line.as_deref(), Some("failed"));
    r.advance(15_000);
    r.checker.pass().await;
    assert_eq!(r.pane.lines().len(), 1);
}

#[tokio::test]
async fn the_line_is_typed_once_also_after_a_restart() {
    let r = rig().await;
    r.store.add(&job("a", r.now())).await.unwrap();
    r.store.add(&job("b", r.now())).await.unwrap();
    r.end_process().await;
    // Job "a" is told. Job "b" meets a busy pane, then Harold restarts.
    r.pane
        .answers
        .lock()
        .unwrap()
        .extend([Typing::Typed { insert_pressed: false }, Typing::PaneBusy("pane_busy")]);
    r.checker.pass().await;
    assert_eq!(r.pane.lines().len(), 2);
    let restarted = checker(&r.store, &r.processes, &r.pane, &r.clock);
    r.advance(15_000);
    restarted.pass().await;
    let lines = r.pane.lines();
    assert_eq!(lines.len(), 3, "only job b is typed after the restart");
    assert!(lines[0].1.starts_with("JOB a ended"));
    assert!(lines[1].1.starts_with("JOB b ended"));
    assert!(lines[2].1.starts_with("JOB b ended"));
    restarted.pass().await;
    assert_eq!(r.pane.lines().len(), 3);
}

#[tokio::test]
async fn remove_types_no_line_even_when_the_process_ends_after() {
    let r = rig().await;
    r.store.add(&job("wrong", r.now())).await.unwrap();
    r.store.remove("wrong", r.now()).await.unwrap();
    r.end_process().await;
    r.checker.pass().await;
    assert!(r.pane.lines().is_empty());
}

#[tokio::test]
async fn the_name_in_the_line_is_cut_to_48_characters() {
    let r = rig().await;
    let long = "n".repeat(60);
    r.store.add(&job(&long, r.now())).await.unwrap();
    r.end_process().await;
    r.checker.pass().await;
    let (_, text) = r.pane.lines().remove(0);
    assert_eq!(text, format!("JOB {} ended after <1 m (pid 4121). Check its output.", "n".repeat(48)));
}

#[tokio::test]
async fn the_report_counts_the_jobs_past_their_expected_end() {
    let r = rig().await;
    r.store.add(&job("a", r.now())).await.unwrap();
    r.store.add(&job("b", r.now() + 30 * MIN)).await.unwrap();
    r.advance(21 * MIN);
    assert_eq!(r.checker.pass().await.past_due, 1);
    r.advance(30 * MIN);
    assert_eq!(r.checker.pass().await.past_due, 2);
}

#[tokio::test]
async fn job_end_not_told_logs_once_at_10_minutes_and_only_for_no_pane_or_failed() {
    let logs = CapturedLogs::start();
    let r = rig().await;
    r.store.add(&job("gone", r.now())).await.unwrap();
    r.store.add(&job("busy", r.now())).await.unwrap();
    r.store.add(&job("fine", r.now())).await.unwrap();
    r.end_process().await;
    // gone: no pane. busy: busy for a while. fine: typed.
    r.pane.answers.lock().unwrap().extend([
        Typing::NoPane,
        Typing::PaneBusy("pane_busy"),
        Typing::Typed { insert_pressed: false },
    ]);
    r.checker.pass().await;
    r.advance(9 * MIN);
    r.pane.answers.lock().unwrap().push_back(Typing::PaneBusy("pane_busy"));
    r.checker.pass().await;
    assert!(!logs.text().contains("job end not told"), "not before 10 minutes");
    r.advance(2 * MIN);
    r.pane.answers.lock().unwrap().push_back(Typing::PaneBusy("pane_busy"));
    r.checker.pass().await;
    r.advance(15_000);
    r.pane.answers.lock().unwrap().push_back(Typing::PaneBusy("pane_busy"));
    r.checker.pass().await;
    let text = logs.text();
    let told: Vec<&str> = text.lines().filter(|l| l.contains("job end not told")).collect();
    assert_eq!(told.len(), 1, "{text}");
    assert!(told[0].contains("gone") && told[0].contains("reason=no_pane"));
    // The busy pane gets the line later: it logs nothing.
    r.advance(15_000);
    r.checker.pass().await;
    assert_eq!(r.only_busy_end_line().await, Some("typed_at_turn_end".to_string()));
    assert_eq!(logs.text().matches("job end not told").count(), 1);
}

impl Rig {
    async fn only_busy_end_line(&self) -> Option<String> {
        self.store
            .shown(self.now() - 60 * MIN)
            .await
            .unwrap()
            .into_iter()
            .find(|job| job.name == "busy")
            .and_then(|job| job.end_line)
    }
}

#[tokio::test]
async fn job_ended_logs_the_run_and_the_seconds_past_the_expected_end() {
    let logs = CapturedLogs::start();
    let r = rig().await;
    r.store.add(&job("a", r.now())).await.unwrap();
    r.advance(25 * MIN);
    r.end_process().await;
    r.checker.pass().await;
    let text = logs.text();
    let line = text.lines().find(|l| l.contains("job ended")).expect("logged");
    assert!(line.contains("run_s=1500") && line.contains("past_due_s=300"), "{line}");
    let _ = &r.dir;
}
