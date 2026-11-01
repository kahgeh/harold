use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use tokio::sync::{Notify, watch};

use super::domain::ObservedAgentState;
use super::rex::RexError;
use super::rex_host_runner::*;
use super::rex_hosts::{HostAgentState, HostClock, HostScan, RexHostAgent};
use crate::outbound::fake::CapturedLogs;

const INTERVAL: Duration = Duration::from_secs(1);
const WORKING: HostAgentState = HostAgentState::Observed(ObservedAgentState::Working);
const IDLE: HostAgentState = HostAgentState::Observed(ObservedAgentState::Idle);

fn agent(
    host: &str,
    block: &str,
    pid: u32,
    first_seen_ms: i64,
    state: HostAgentState,
) -> RexHostAgent {
    RexHostAgent {
        host: host.into(),
        host_pane_id: format!("rex:{host}/block:{block}"),
        pid,
        first_seen_ms,
        state,
        current_step: Some("Edit files".into()),
        provider_id: "alpha".into(),
        provider_display_name: "Alpha".into(),
        session_name: "work".into(),
        window_index: 1,
        pane_index: 2,
        working_directory: "/work/repo".into(),
    }
}

#[derive(Clone)]
enum Step {
    Good(Vec<RexHostAgent>),
    Fail(RexError),
    /// The scan waits until the test lets it go, then fails.
    Block,
    /// The scan runs until the stop signal of its thread is requested.
    UntilStopped,
}

/// A host that answers each scan from a script. The last step repeats.
struct FakeHost {
    label: String,
    host_id: Option<String>,
    listed: Option<bool>,
    /// The `host_id` of the Rex server on this Mac. `None` makes that read fail.
    local_id: Option<String>,
    local_calls: AtomicUsize,
    script: Mutex<VecDeque<Step>>,
    last: Mutex<Option<Step>>,
    calls: AtomicUsize,
    started: Notify,
    stopped: Notify,
    released: (Mutex<bool>, Condvar),
}

impl FakeHost {
    fn new(label: &str, steps: Vec<Step>) -> Arc<Self> {
        Arc::new(Self {
            label: label.into(),
            host_id: Some(format!("id-{label}")),
            listed: Some(true),
            local_id: None,
            local_calls: AtomicUsize::new(0),
            script: Mutex::new(steps.into()),
            last: Mutex::new(None),
            calls: AtomicUsize::new(0),
            started: Notify::new(),
            stopped: Notify::new(),
            released: (Mutex::new(false), Condvar::new()),
        })
    }

    fn with(label: &str, host_id: &str, listed: Option<bool>, steps: Vec<Step>) -> Arc<Self> {
        let mut host = Arc::into_inner(Self::new(label, steps)).unwrap();
        host.host_id = Some(host_id.into());
        host.listed = listed;
        Arc::new(host)
    }

    fn on_this_mac(label: &str, host_id: &str, local_id: Option<&str>) -> Arc<Self> {
        let mut host = Arc::into_inner(Self::with(
            label,
            host_id,
            Some(true),
            vec![Step::Good(vec![agent(label, "b1", 200, 5, WORKING)])],
        ))
        .unwrap();
        host.local_id = local_id.map(str::to_string);
        Arc::new(host)
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl HostSource for FakeHost {
    fn label(&self) -> &str {
        &self.label
    }

    fn scan(&self) -> HostScan {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let step = match self.script.lock().unwrap().pop_front() {
            Some(step) => {
                *self.last.lock().unwrap() = Some(step.clone());
                step
            }
            None => self.last.lock().unwrap().clone().expect("a scripted step"),
        };
        let failed = |error| HostScan {
            agents: Vec::new(),
            host_id: self.host_id.clone(),
            outcome: Err(error),
        };
        match step {
            Step::Good(agents) => HostScan {
                agents,
                host_id: self.host_id.clone(),
                outcome: Ok(()),
            },
            Step::Fail(error) => failed(error),
            Step::UntilStopped => {
                self.started.notify_one();
                while !super::screen::stop_requested() {
                    std::thread::yield_now();
                }
                self.stopped.notify_one();
                failed(RexError::Failed)
            }
            Step::Block => {
                self.started.notify_one();
                let (lock, ready) = &self.released;
                let mut released = lock.lock().unwrap();
                while !*released {
                    released = ready.wait(released).unwrap();
                }
                failed(RexError::Failed)
            }
        }
    }

    fn icon(&self) -> String {
        "server.rack".into()
    }

    fn listed(&self) -> Option<bool> {
        self.listed
    }

    fn local_host_id(&self) -> Option<String> {
        self.local_calls.fetch_add(1, Ordering::SeqCst);
        self.local_id.clone()
    }
}

/// Lets a blocked scan go when the test ends, so the runtime can stop.
struct Release(Arc<FakeHost>);

impl Drop for Release {
    fn drop(&mut self) {
        *self.0.released.0.lock().unwrap() = true;
        self.0.released.1.notify_all();
    }
}

/// Epoch ms from the paused tokio clock, from 1 000 000 at the test start.
fn virtual_clock() -> HostClock {
    let start = tokio::time::Instant::now();
    Arc::new(move || 1_000_000 + start.elapsed().as_millis() as i64)
}

fn spawn(hosts: &[Arc<FakeHost>]) -> (HostAgents, watch::Sender<()>) {
    let (shutdown, shutdown_rx) = watch::channel(());
    let sources = hosts
        .iter()
        .map(|host| Arc::clone(host) as Arc<dyn HostSource>)
        .collect();
    let (agents, _tasks) = spawn_hosts(sources, INTERVAL, virtual_clock(), shutdown_rx);
    (agents, shutdown)
}

fn host_lines(logs: &CapturedLogs) -> Vec<String> {
    logs.text()
        .lines()
        .filter(|line| line.contains("agent monitor host"))
        .map(str::to_string)
        .collect()
}

#[tokio::test(start_paused = true)]
async fn after_three_failures_the_host_is_tried_every_30_seconds_and_a_good_scan_resets() {
    let logs = CapturedLogs::start();
    let mut steps = vec![Step::Fail(RexError::Failed); 4];
    steps.push(Step::Good(vec![agent("omarchy", "b1", 200, 5, WORKING)]));
    let host = FakeHost::new("omarchy", steps);
    let (_agents, _shutdown) = spawn(&[Arc::clone(&host)]);
    // Scans at 0, 1 and 2 s fail. The fourth try waits 30 s: at 32 s.
    tokio::time::sleep(Duration::from_millis(31_500)).await;
    assert_eq!(host.calls(), 3);
    tokio::time::sleep(Duration::from_millis(30_000)).await;
    assert_eq!(host.calls(), 4);
    // The fifth try, at 62 s, works. Then the host is read every second again.
    tokio::time::sleep(Duration::from_millis(4_000)).await;
    assert_eq!(host.calls(), 8);
    let lines = host_lines(&logs);
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(lines[0].contains("host=omarchy"), "{lines:?}");
    assert!(lines[0].contains(r#"state="unreachable""#), "{lines:?}");
    assert!(lines[0].contains(r#"reason="failed""#), "{lines:?}");
    assert!(lines[1].contains(r#"state="reachable""#), "{lines:?}");
    assert!(
        lines
            .iter()
            .all(|line| line.trim_start().starts_with("INFO")),
        "{lines:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_failed_scan_keeps_the_last_agents_as_unknown_and_a_host_that_never_answered_has_none() {
    let host = FakeHost::new(
        "omarchy",
        vec![
            Step::Good(vec![agent("omarchy", "b1", 200, 5, WORKING)]),
            Step::Fail(RexError::Failed),
        ],
    );
    let never = FakeHost::new("nas", vec![Step::Fail(RexError::Timeout)]);
    let (agents, _shutdown) = spawn(&[Arc::clone(&host), Arc::clone(&never)]);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let rows = agents.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].agent.state, WORKING);
    assert_eq!(rows[0].last_transition_at_ms, 5);
    assert_eq!(rows[0].icon, "server.rack");
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    let rows = agents.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].agent.host, "omarchy");
    assert_eq!(rows[0].agent.state, HostAgentState::Unknown);
    assert_eq!(rows[0].last_transition_at_ms, 1_001_000);
}

#[tokio::test(start_paused = true)]
async fn an_unknown_label_logs_unknown_host_once() {
    let logs = CapturedLogs::start();
    let host = FakeHost::with(
        "nosuch",
        "id-x",
        Some(false),
        vec![Step::Fail(RexError::Failed)],
    );
    let (agents, _shutdown) = spawn(&[Arc::clone(&host)]);
    tokio::time::sleep(Duration::from_secs(100)).await;
    assert!(host.calls() > 3);
    let lines = host_lines(&logs);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains(r#"reason="unknown_host""#), "{lines:?}");
    assert!(lines[0].contains(r#"state="unreachable""#), "{lines:?}");
    assert!(agents.snapshot().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_timeout_logs_timeout() {
    let logs = CapturedLogs::start();
    let host = FakeHost::with("omarchy", "id-o", None, vec![Step::Fail(RexError::Timeout)]);
    let (_agents, _shutdown) = spawn(&[Arc::clone(&host)]);
    tokio::time::sleep(Duration::from_secs(10)).await;
    let lines = host_lines(&logs);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains(r#"reason="timeout""#), "{lines:?}");
}

#[tokio::test(start_paused = true)]
async fn two_labels_for_one_server_list_only_the_first() {
    let logs = CapturedLogs::start();
    let first = FakeHost::with(
        "omarchy",
        "same",
        Some(true),
        vec![Step::Good(vec![agent("omarchy", "b1", 200, 5, WORKING)])],
    );
    let second = FakeHost::with(
        "omarchy-ts",
        "same",
        Some(true),
        vec![Step::Good(vec![agent("omarchy-ts", "b1", 200, 5, WORKING)])],
    );
    let (agents, _shutdown) = spawn(&[Arc::clone(&first), Arc::clone(&second)]);
    tokio::time::sleep(Duration::from_millis(5_500)).await;
    let rows = agents.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].agent.host, "omarchy");
    // The left-out label is not read again.
    assert_eq!(second.calls(), 1);
    assert!(first.calls() >= 5);
    let lines: Vec<_> = host_lines(&logs)
        .into_iter()
        .filter(|line| line.contains("duplicate_host"))
        .collect();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("host=omarchy-ts"), "{lines:?}");
    assert!(lines[0].contains(r#"state="left_out""#), "{lines:?}");
}

#[tokio::test(start_paused = true)]
async fn an_overall_scan_timeout_counts_as_a_failed_scan() {
    let logs = CapturedLogs::start();
    let host = FakeHost::new(
        "omarchy",
        vec![
            Step::Good(vec![agent("omarchy", "b1", 200, 5, WORKING)]),
            Step::Block,
        ],
    );
    let _release = Release(Arc::clone(&host));
    let (agents, _shutdown) = spawn(&[Arc::clone(&host)]);
    let mut versions = agents.subscribe();
    versions.changed().await.unwrap();
    assert_eq!(agents.snapshot()[0].agent.state, WORKING);
    // The blocked scan stops the paused clock, so the test moves it on by hand.
    host.started.notified().await;
    tokio::time::advance(SCAN_BUDGET).await;
    versions.changed().await.unwrap();
    assert_eq!(agents.snapshot()[0].agent.state, HostAgentState::Unknown);
    let lines = host_lines(&logs);
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(lines[1].contains(r#"reason="timeout""#), "{lines:?}");
}

#[test]
fn the_last_transition_starts_at_first_seen_and_moves_when_the_state_changes() {
    let agents = HostAgents::new(vec!["omarchy".into()]);
    let versions = agents.subscribe();
    let reached = |state| HostReport::Reached {
        agents: vec![agent("omarchy", "b1", 200, 100, state)],
        host_id: Some("id-o".into()),
        icon: "server.rack".into(),
    };
    agents.record("omarchy", reached(WORKING), 500);
    assert_eq!(agents.snapshot()[0].last_transition_at_ms, 100);
    let version = *versions.borrow();
    agents.record("omarchy", reached(WORKING), 600);
    assert_eq!(agents.snapshot()[0].last_transition_at_ms, 100);
    // The same content does not raise the version.
    assert_eq!(*versions.borrow(), version);
    agents.record("omarchy", reached(IDLE), 700);
    assert_eq!(agents.snapshot()[0].last_transition_at_ms, 700);
    assert!(*versions.borrow() > version);
}

#[test]
fn rows_follow_the_order_of_rex_hosts() {
    let agents = HostAgents::new(vec!["b-host".into(), "a-host".into()]);
    for label in ["a-host", "b-host"] {
        agents.record(
            label,
            HostReport::Reached {
                agents: vec![agent(label, "b1", 200, 1, WORKING)],
                host_id: Some(format!("id-{label}")),
                icon: String::new(),
            },
            10,
        );
    }
    let hosts: Vec<_> = agents
        .snapshot()
        .into_iter()
        .map(|row| row.agent.host)
        .collect();
    assert_eq!(hosts, ["b-host", "a-host"]);
}

fn gauge(host: &str, provider: &str, state: &str) -> Option<u64> {
    crate::telemetry::test_support::gauge_value(
        "harold.agents",
        &[
            ("state", state),
            ("provider_id", provider),
            ("multiplexer", "rex"),
            ("host", host),
        ],
    )
}

#[test]
fn host_agents_are_counted_by_state_and_an_unreached_host_counts_unknown() {
    crate::telemetry::test_support::install();
    let agents = HostAgents::new(vec!["count_host".into()]);
    let mut first = agent("count_host", "b1", 200, 1, WORKING);
    let mut second = agent("count_host", "b2", 201, 1, IDLE);
    first.provider_id = "count_provider".into();
    second.provider_id = "count_provider".into();
    agents.record(
        "count_host",
        HostReport::Reached {
            agents: vec![first, second],
            host_id: Some("id".into()),
            icon: String::new(),
        },
        10,
    );
    assert_eq!(gauge("count_host", "count_provider", "working"), Some(1));
    assert_eq!(gauge("count_host", "count_provider", "idle"), Some(1));
    assert_eq!(gauge("count_host", "count_provider", "unknown"), Some(0));
    agents.record(
        "count_host",
        HostReport::Unreached {
            reason: Unreached::Failed,
            host_id: None,
        },
        20,
    );
    assert_eq!(gauge("count_host", "count_provider", "working"), Some(0));
    assert_eq!(gauge("count_host", "count_provider", "unknown"), Some(2));
}

#[tokio::test(start_paused = true)]
async fn shutdown_stops_the_host_tasks() {
    let host = FakeHost::new("omarchy", vec![Step::Good(vec![])]);
    let (shutdown, shutdown_rx) = watch::channel(());
    let (_agents, tasks) = spawn_hosts(
        vec![Arc::clone(&host) as Arc<dyn HostSource>],
        INTERVAL,
        virtual_clock(),
        shutdown_rx,
    );
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    shutdown.send(()).unwrap();
    for task in tasks {
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("the task stops")
            .expect("the task does not panic");
    }
}

#[tokio::test(start_paused = true)]
async fn a_host_that_fails_and_recovers_moves_the_transition_time_and_logs_the_pair() {
    let logs = CapturedLogs::start();
    let good = || Step::Good(vec![agent("omarchy", "b1", 200, 5, WORKING)]);
    let host = FakeHost::new(
        "omarchy",
        vec![good(), Step::Fail(RexError::Failed), good()],
    );
    let (agents, _shutdown) = spawn(&[Arc::clone(&host)]);
    // Scans at 0, 1 and 2 s.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(agents.snapshot()[0].last_transition_at_ms, 5);
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    let rows = agents.snapshot();
    assert_eq!(rows[0].agent.state, HostAgentState::Unknown);
    assert_eq!(rows[0].last_transition_at_ms, 1_001_000);
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    let rows = agents.snapshot();
    assert_eq!(rows[0].agent.state, WORKING);
    assert_eq!(rows[0].last_transition_at_ms, 1_002_000);
    let lines = host_lines(&logs);
    let states: Vec<_> = lines
        .iter()
        .map(|line| {
            ["reachable", "unreachable"]
                .into_iter()
                .find(|state| line.contains(&format!(r#"state="{state}""#)))
                .unwrap()
        })
        .collect();
    assert_eq!(
        states,
        ["reachable", "unreachable", "reachable"],
        "{lines:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn shutdown_stops_a_host_read_in_progress() {
    let host = FakeHost::new("omarchy", vec![Step::UntilStopped]);
    let (shutdown, shutdown_rx) = watch::channel(());
    let (_agents, tasks) = spawn_hosts(
        vec![Arc::clone(&host) as Arc<dyn HostSource>],
        INTERVAL,
        virtual_clock(),
        shutdown_rx,
    );
    host.started.notified().await;
    shutdown.send(()).unwrap();
    for task in tasks {
        task.await.expect("the task does not panic");
    }
    // The paused clock moves on only when every task waits. A read that does not stop
    // would let the budget pass first.
    tokio::time::timeout(SCAN_BUDGET, host.stopped.notified())
        .await
        .expect("shutdown stops the read before its call limit");
}

#[tokio::test(start_paused = true)]
async fn a_label_for_this_mac_is_left_out_with_one_warning() {
    let logs = CapturedLogs::start();
    let mine = FakeHost::on_this_mac("loopback", "mac-id", Some("mac-id"));
    let other = FakeHost::on_this_mac("omarchy", "other-id", Some("mac-id"));
    let (agents, _shutdown) = spawn(&[Arc::clone(&mine), Arc::clone(&other)]);
    tokio::time::sleep(Duration::from_millis(5_500)).await;
    let rows = agents.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].agent.host, "omarchy");
    // The left-out label is not read again, and the local id is read once for both.
    assert_eq!(mine.calls(), 1);
    assert_eq!(
        mine.local_calls.load(Ordering::SeqCst) + other.local_calls.load(Ordering::SeqCst),
        1
    );
    let lines: Vec<_> = host_lines(&logs)
        .into_iter()
        .filter(|line| line.contains("local_host"))
        .collect();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].trim_start().starts_with("WARN"), "{lines:?}");
    assert!(lines[0].contains("host=loopback"), "{lines:?}");
    assert!(lines[0].contains(r#"state="left_out""#), "{lines:?}");
}

#[tokio::test(start_paused = true)]
async fn a_failed_local_read_leaves_out_nothing_and_logs_nothing() {
    let logs = CapturedLogs::start();
    let first = FakeHost::on_this_mac("omarchy", "id-a", None);
    let second = FakeHost::on_this_mac("nas", "id-b", None);
    let (agents, _shutdown) = spawn(&[Arc::clone(&first), Arc::clone(&second)]);
    tokio::time::sleep(Duration::from_millis(5_500)).await;
    assert_eq!(agents.snapshot().len(), 2);
    assert!(first.calls() >= 5);
    assert_eq!(
        first.local_calls.load(Ordering::SeqCst) + second.local_calls.load(Ordering::SeqCst),
        1
    );
    assert!(
        host_lines(&logs)
            .iter()
            .all(|line| !line.contains("left_out")),
        "{:?}",
        host_lines(&logs)
    );
}
