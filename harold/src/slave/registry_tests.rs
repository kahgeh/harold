//! Tests of the registry. They call the `MasterSink` methods directly: no port, no socket.

use std::sync::atomic::{AtomicBool, Ordering};

use harold_api::harold::{AgentPaneState, AgentState};

use super::*;
use crate::agent::rex_host_runner::HostAgents;

const LABEL: &str = "omarchy";

/// The turns that the turn-end path took.
#[derive(Default)]
struct FakeTurns {
    taken: Mutex<Vec<TurnCompleteRequest>>,
    fail: AtomicBool,
}

impl TurnSink for FakeTurns {
    fn record(&self, request: TurnCompleteRequest) -> TurnFuture<'_> {
        Box::pin(async move {
            if self.fail.load(Ordering::SeqCst) {
                return Err("store".to_owned());
            }
            self.taken.lock().unwrap().push(request);
            Ok(())
        })
    }
}

fn registry() -> (SlaveRegistry, HostAgents, Arc<FakeTurns>) {
    let hosts = HostAgents::new(vec![LABEL.to_owned()]);
    let turns = Arc::new(FakeTurns::default());
    let registry = SlaveRegistry::new(hosts.clone(), turns.clone());
    (registry, hosts, turns)
}

fn pane(id: &str, pid: u32, state: AgentState) -> AgentPaneState {
    AgentPaneState {
        pane_id: id.into(),
        session_name: "work".into(),
        agent_pid: pid,
        pane_pid: pid,
        agent_started_at_ms: 5,
        provider_id: "alpha".into(),
        provider_display_name: "Alpha".into(),
        state: state as i32,
        host_icon: "linux".into(),
        recap: Some("a recap".into()),
        recap_at_ms: 9,
        ..AgentPaneState::default()
    }
}

fn snapshot(panes: Vec<AgentPaneState>) -> AgentStateSnapshot {
    AgentStateSnapshot {
        panes,
        ..AgentStateSnapshot::default()
    }
}

fn turn(pane_id: &str) -> Turn {
    Turn {
        report_no: 7,
        pane_id: pane_id.into(),
        pane_label: "label".into(),
        session_id: "s-1".into(),
        turn_started_at: "2026-11-05T01:00:00Z".into(),
        last_user_prompt: "prompt".into(),
        assistant_message: "answer".into(),
        main_context: "ctx".into(),
        input_tokens: 11,
        output_tokens: 22,
        cache_read_tokens: 33,
        cache_write_tokens: 44,
        model: "model-x".into(),
        transcript_path: "/home/someone/t.jsonl".into(),
    }
}

fn ids(hosts: &HostAgents) -> Vec<String> {
    hosts
        .snapshot()
        .into_iter()
        .map(|row| row.agent.host_pane_id)
        .collect()
}

#[test]
fn a_link_up_makes_the_label_linked_until_it_drops() {
    let (registry, hosts, _) = registry();
    assert!(!registry.is_linked(LABEL));
    registry.link_changed(LABEL, true);
    assert!(registry.is_linked(LABEL));
    assert!(hosts.is_linked(LABEL));
    registry.link_changed(LABEL, false);
    assert!(!registry.is_linked(LABEL));
}

#[test]
fn snapshot_agents_keep_host_pane_ids() {
    let (registry, hosts, _) = registry();
    registry.link_changed(LABEL, true);
    registry.apply_snapshot(
        LABEL,
        snapshot(vec![
            pane("block:a1", 100, AgentState::Working),
            pane("block:b2", 101, AgentState::Idle),
            // A pane that is not a Rex block has no host pane id.
            pane("%3", 102, AgentState::Idle),
        ]),
    );
    assert_eq!(
        ids(&hosts),
        ["rex:omarchy/block:a1", "rex:omarchy/block:b2"]
    );
    let rows = hosts.snapshot();
    assert_eq!(rows[0].agent.host, LABEL);
    assert_eq!(rows[0].agent.pid, 100);
    assert_eq!(rows[0].agent.provider_id, "alpha");
    assert_eq!(rows[0].icon, "linux");
    assert_eq!(rows[0].agent.state.effective().as_str(), "working");
    assert_eq!(rows[1].agent.state.effective().as_str(), "idle");
}

#[test]
fn empty_snapshot_clears_the_host_agents() {
    let (registry, hosts, _) = registry();
    registry.link_changed(LABEL, true);
    registry.apply_snapshot(
        LABEL,
        snapshot(vec![pane("block:a1", 100, AgentState::Idle)]),
    );
    assert_eq!(ids(&hosts).len(), 1);
    registry.apply_snapshot(LABEL, snapshot(Vec::new()));
    assert!(ids(&hosts).is_empty());
}

#[test]
fn duplicate_pane_ids_in_a_snapshot_keep_the_last() {
    let (registry, hosts, _) = registry();
    registry.link_changed(LABEL, true);
    registry.apply_snapshot(
        LABEL,
        snapshot(vec![
            pane("block:a1", 100, AgentState::Working),
            pane("block:b2", 101, AgentState::Idle),
            pane("block:a1", 200, AgentState::Done),
        ]),
    );
    let rows = hosts.snapshot();
    assert_eq!(rows.len(), 2);
    let first = rows
        .iter()
        .find(|row| row.agent.host_pane_id == "rex:omarchy/block:a1")
        .unwrap();
    assert_eq!(first.agent.pid, 200);
    assert_eq!(first.agent.state.effective().as_str(), "done");
}

#[tokio::test(flavor = "multi_thread")]
async fn slave_turn_reaches_the_turn_end_path_with_host_pane_id() {
    let (registry, _, turns) = registry();
    let registry = Arc::new(registry);
    let outcome = {
        let registry = Arc::clone(&registry);
        tokio::spawn(async move { registry.apply_turn(LABEL, turn("block:a1")) })
            .await
            .unwrap()
    };
    assert_eq!(outcome, Applied::Applied);
    let taken = turns.taken.lock().unwrap();
    assert_eq!(taken.len(), 1);
    let request = &taken[0];
    assert_eq!(request.pane_id, "rex:omarchy/block:a1");
    assert_eq!(request.pane_label, "label");
    assert_eq!(request.session_id, "s-1");
    assert_eq!(request.turn_started_at, "2026-11-05T01:00:00Z");
    assert_eq!(request.last_user_prompt, "prompt");
    assert_eq!(request.assistant_message, "answer");
    assert_eq!(request.main_context, "ctx");
    assert_eq!(
        (
            request.input_tokens,
            request.output_tokens,
            request.cache_read_tokens,
            request.cache_write_tokens
        ),
        (11, 22, 33, 44)
    );
    assert_eq!(request.model, "model-x");
    // The slave's transcript path means nothing on the master.
    assert_eq!(request.transcript_path, "");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_fails_only_when_the_turn_path_fails_and_a_bad_pane_id_is_dropped() {
    let (registry, _, turns) = registry();
    let registry = Arc::new(registry);
    let run = |turn: Turn| {
        let registry = Arc::clone(&registry);
        async move {
            tokio::spawn(async move { registry.apply_turn(LABEL, turn) })
                .await
                .unwrap()
        }
    };
    turns.fail.store(true, Ordering::SeqCst);
    assert_eq!(run(turn("block:a1")).await, Applied::Failed);
    turns.fail.store(false, Ordering::SeqCst);
    // A pane id that has no host form cannot be applied ever: drop it, do not loop on it.
    assert_eq!(run(turn("%3")).await, Applied::Applied);
    assert_eq!(run(turn("block:a/../b")).await, Applied::Applied);
    assert!(turns.taken.lock().unwrap().is_empty());
}

#[test]
fn slave_lock_decides_away_for_slave_agents() {
    let (registry, _, _) = registry();
    let pane_id = "rex:omarchy/block:a1";
    // No lock yet: the slave counts unlocked, and the master does not claim locked.
    assert_eq!(registry.lock_of(LABEL), None);
    assert_eq!(registry.pane_lock(pane_id), None);
    registry.link_changed(LABEL, true);
    assert_eq!(registry.pane_lock(pane_id), Some(false));
    assert_eq!(registry.apply_lock(LABEL, true), Applied::Applied);
    assert_eq!(registry.pane_lock(pane_id), Some(true));
    assert_eq!(registry.apply_lock(LABEL, false), Applied::Applied);
    assert_eq!(registry.pane_lock(pane_id), Some(false));
    // The lock of one slave never decides for the Mac's own panes or another host.
    assert_eq!(registry.pane_lock("block:abc"), None);
    assert_eq!(registry.pane_lock("%1"), None);
    assert_eq!(registry.pane_lock("rex:other/block:a1"), None);
    // The last lock stays after the link drops.
    registry.apply_lock(LABEL, true);
    registry.link_changed(LABEL, false);
    assert_eq!(registry.pane_lock(pane_id), Some(true));
}
