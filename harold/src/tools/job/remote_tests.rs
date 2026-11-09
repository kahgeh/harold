use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use super::*;
use crate::outbound::fake::CapturedLogs;
use crate::tools::Typing;
use crate::tools::job::check::Checker;
use crate::tools::job::feed::JobsFeed;
use crate::tools::job::store::{JobState, JobStore, NewJob};
use crate::tools::job::test_support::{FakePane, FakeProcesses, FakeUnits, Temp};

const MIN: i64 = 60_000;

struct Rig {
    _dir: Temp,
    store: Arc<JobStore>,
    feed: Arc<JobsFeed>,
    pane: Arc<FakePane>,
    clock: Arc<AtomicI64>,
    checker: Checker,
}

fn checker(
    store: &Arc<JobStore>,
    feed: &Arc<JobsFeed>,
    pane: &Arc<FakePane>,
    clock: &Arc<AtomicI64>,
) -> Checker {
    let clock = Arc::clone(clock);
    Checker::new(
        Arc::clone(store),
        Arc::new(FakeProcesses::default()),
        Arc::new(FakeUnits::default()),
        pane.clone(),
        Arc::new(move || clock.load(Ordering::SeqCst)),
        Arc::clone(feed),
    )
}

async fn rig() -> Rig {
    let dir = Temp::new();
    let store = dir.store().await;
    let feed = Arc::new(JobsFeed::new());
    let pane = Arc::new(FakePane::default());
    let clock = Arc::new(AtomicI64::new(1_000 * MIN));
    let checker = checker(&store, &feed, &pane, &clock);
    Rig {
        _dir: dir,
        store,
        feed,
        pane,
        clock,
        checker,
    }
}

impl Rig {
    fn now(&self) -> i64 {
        self.clock.load(Ordering::SeqCst)
    }

    fn advance(&self, ms: i64) {
        self.clock.fetch_add(ms, Ordering::SeqCst);
    }

    /// The master's feed gets a snapshot of the slave with one job.
    fn slave_job(&self, state: &str, pane_id: &str, unit: bool) {
        let ended = if state == "ended" {
            1_000 * MIN + 21 * MIN
        } else {
            0
        };
        self.feed.set_slave_jobs(
            "omarchy",
            vec![AgentJob {
                name: "calibration".into(),
                pane_id: pane_id.into(),
                unit: if unit { "cal-1".into() } else { String::new() },
                pid: if unit { 0 } else { 4121 },
                started_at_ms: 1_000 * MIN,
                due_at_ms: 1_000 * MIN + 20 * MIN,
                ended_at_ms: ended,
                state: state.into(),
                exit_known: unit,
                exit_code: 0,
                ..Default::default()
            }],
            self.now(),
        );
    }
}

use crate::harold::AgentJob;

#[tokio::test]
async fn a_slave_job_that_ends_gives_one_line_in_a_mac_pane_also_for_the_same_snapshot_twice() {
    let r = rig().await;
    r.slave_job("running", "block:7", true);
    r.advance(21 * MIN);
    r.checker.pass().await;
    assert!(r.pane.lines().is_empty());
    r.slave_job("ended", "block:7", true);
    r.checker.pass().await;
    assert_eq!(
        r.pane.lines(),
        vec![(
            "block:7".to_string(),
            "JOB calibration on omarchy ended after 21 m: exit 0.".to_string()
        )]
    );
    // The same snapshot comes again, and again later.
    r.slave_job("ended", "block:7", true);
    r.advance(15_000);
    r.checker.pass().await;
    r.checker.pass().await;
    assert_eq!(r.pane.lines().len(), 1, "typed once");
}

#[tokio::test]
async fn the_line_is_typed_once_also_after_a_master_restart() {
    let r = rig().await;
    r.advance(21 * MIN);
    r.slave_job("ended", "block:7", true);
    r.checker.pass().await;
    let restarted = checker(&r.store, &r.feed, &r.pane, &r.clock);
    r.advance(15_000);
    restarted.pass().await;
    assert_eq!(r.pane.lines().len(), 1);
}

#[tokio::test]
async fn a_pid_job_of_a_slave_reads_with_its_host_and_pid() {
    let r = rig().await;
    r.advance(21 * MIN);
    r.slave_job("ended", "block:7", false);
    r.checker.pass().await;
    assert_eq!(
        r.pane.lines()[0].1,
        "JOB calibration on omarchy ended after 21 m (pid 4121). Check its output."
    );
}

#[tokio::test]
async fn an_agent_pane_on_a_rex_host_gets_remote_pane_and_no_typing() {
    let r = rig().await;
    r.advance(21 * MIN);
    r.slave_job("ended", "rex:omarchy/block:7", true);
    r.checker.pass().await;
    assert!(r.pane.lines().is_empty());
    let told = r
        .store
        .told(&ToldKey {
            host: "omarchy".into(),
            name: "calibration".into(),
            started_at_ms: 1_000 * MIN,
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(told.end_line.as_deref(), Some("remote_pane"));
}

#[tokio::test]
async fn a_busy_pane_gets_the_slave_line_at_a_later_check() {
    let r = rig().await;
    r.advance(21 * MIN);
    r.pane
        .answers
        .lock()
        .unwrap()
        .push_back(Typing::PaneBusy("pane_busy"));
    r.slave_job("ended", "block:7", true);
    r.checker.pass().await;
    r.advance(15_000);
    r.checker.pass().await;
    r.advance(15_000);
    r.checker.pass().await;
    assert_eq!(
        r.pane.lines().len(),
        2,
        "one busy try, one typed line, then nothing"
    );
    let told = r
        .store
        .told(&ToldKey {
            host: "omarchy".into(),
            name: "calibration".into(),
            started_at_ms: 1_000 * MIN,
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(told.end_line.as_deref(), Some("typed_at_turn_end"));
}

#[tokio::test]
async fn the_master_logs_the_end_once_with_the_host_and_the_exit_code() {
    let logs = CapturedLogs::start();
    let r = rig().await;
    r.advance(21 * MIN);
    r.slave_job("ended", "block:7", true);
    r.checker.pass().await;
    r.checker.pass().await;
    let text = logs.text();
    let ended: Vec<&str> = text.lines().filter(|l| l.contains("job ended")).collect();
    assert_eq!(ended.len(), 1, "{text}");
    for field in [
        "host=omarchy",
        "job.name=calibration",
        "exit_code=0",
        "run_s=1260",
    ] {
        assert!(ended[0].contains(field), "{field} in {}", ended[0]);
    }
}

#[tokio::test]
async fn a_closed_link_logs_job_host_not_reached_once_at_30_minutes_also_after_a_restart() {
    let logs = CapturedLogs::start();
    let r = rig().await;
    r.slave_job("running", "block:7", true);
    r.feed.slave_link("omarchy", false, r.now());
    r.advance(29 * MIN);
    r.checker.pass().await;
    assert!(!logs.text().contains("job host not reached"));
    r.advance(MIN);
    r.checker.pass().await;
    r.checker.pass().await;
    let text = logs.text();
    assert_eq!(text.matches("job host not reached").count(), 1, "{text}");
    let line = text
        .lines()
        .find(|l| l.contains("job host not reached"))
        .unwrap();
    for field in ["host=omarchy", "job.name=calibration", "pane_id=block:7"] {
        assert!(line.contains(field), "{field} in {line}");
    }
    // A master restart: a new checker, the same record.
    let restarted = checker(&r.store, &r.feed, &r.pane, &r.clock);
    restarted.pass().await;
    assert_eq!(logs.text().matches("job host not reached").count(), 1);
    // The link opens, then closes again: a new outage logs again.
    r.feed.slave_link("omarchy", true, r.now());
    r.advance(MIN);
    r.feed.slave_link("omarchy", false, r.now());
    r.advance(30 * MIN);
    restarted.pass().await;
    assert_eq!(logs.text().matches("job host not reached").count(), 2);
}

#[tokio::test]
async fn a_slave_line_that_reached_no_pane_logs_end_not_told_once_at_10_minutes() {
    let logs = CapturedLogs::start();
    let r = rig().await;
    r.advance(21 * MIN);
    r.pane.answers.lock().unwrap().push_back(Typing::NoPane);
    r.slave_job("ended", "block:7", true);
    r.checker.pass().await;
    r.advance(10 * MIN);
    r.checker.pass().await;
    r.checker.pass().await;
    let text = logs.text();
    assert_eq!(text.matches("job end not told").count(), 1, "{text}");
    assert!(
        text.contains("host=omarchy") && text.contains("reason=no_pane"),
        "{text}"
    );
}

#[tokio::test]
async fn on_a_slave_an_ended_job_records_end_line_master_and_types_and_logs_nothing() {
    let logs = CapturedLogs::start();
    let dir = Temp::new();
    let store = dir.store().await;
    let processes = Arc::new(FakeProcesses::default());
    processes.0.lock().unwrap().insert(4121, 1_700_000_000);
    let pane = Arc::new(FakePane::default());
    let clock = Arc::new(AtomicI64::new(1_000 * MIN));
    let clock2 = Arc::clone(&clock);
    let slave = Checker::new(
        Arc::clone(&store),
        processes.clone(),
        Arc::new(FakeUnits::default()),
        pane.clone(),
        Arc::new(move || clock2.load(Ordering::SeqCst)),
        Arc::new(JobsFeed::new()),
    )
    .with_role(true);
    store
        .add(&NewJob {
            name: "calibration".into(),
            pane_id: "block:7".into(),
            pid: 4121,
            process_start_s: 1_700_000_000,
            started_at_ms: 1_000 * MIN,
            due_at_ms: 1_020 * MIN,
            ..Default::default()
        })
        .await
        .unwrap();
    processes.0.lock().unwrap().clear();
    slave.pass().await;
    let job = store
        .shown(clock.load(Ordering::SeqCst))
        .await
        .unwrap()
        .remove(0);
    assert_eq!(job.state, JobState::Ended);
    assert_eq!(job.end_line.as_deref(), Some("master"));
    assert!(pane.lines().is_empty());
    assert!(!logs.text().contains("job ended"), "{}", logs.text());
    drop(dir);
}
