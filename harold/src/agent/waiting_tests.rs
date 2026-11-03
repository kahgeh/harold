use crate::store::EventVersion;

use super::domain::{
    AgentIncarnation, AgentPaneObservation, AgentPaneProjection, AgentSnapshot, EffectiveAgentState,
};
use super::waiting::{Focus, WaitingEvent, WaitingTracker};

use EffectiveAgentState::{Blocked, Done, Error, Idle, Unknown, Working};

fn pane(pane_id: &str, agent_pid: u32, state: EffectiveAgentState) -> AgentPaneProjection {
    AgentPaneProjection {
        pane: AgentPaneObservation {
            incarnation: AgentIncarnation {
                pane_id: pane_id.into(),
                pane_pid: 1,
                agent_pid,
                agent_started_at_ms: 1_000,
                provider_id: "p".into(),
            },
            tmux_target: "s:1.1".into(),
            session_name: "s".into(),
            window_index: 1,
            pane_index: 1,
            working_directory: "/w".into(),
            provider_display_name: "P".into(),
            observed_at_ms: 1,
        },
        hook_state: None,
        hook_observed_at_ms: None,
        lifecycle_adapter_id: None,
        screen_state: None,
        screen_classifier_id: None,
        screen_observed_at_ms: None,
        effective_state: state,
        explicit_work_summary: None,
        explicit_work_summary_updated_at_ms: None,
        screen_work_summary: None,
        screen_work_summary_updated_at_ms: None,
        summary_basis_version: EventVersion::start(),
        generated_work_summary: None,
        generated_summary_basis_version: None,
        work_summary: None,
        last_transition_at_ms: 1,
        last_event_version: EventVersion::start(),
    }
}

fn snap(panes: Vec<AgentPaneProjection>) -> AgentSnapshot {
    AgentSnapshot {
        tts_muted: false,
        messaging_paused: false,
        through_event_version: EventVersion::start(),
        server_time_ms: 1,
        monitor_health: Vec::new(),
        panes,
    }
}

fn role(pane_id: &str) -> String {
    format!("role{pane_id}")
}

fn observe(
    t: &mut WaitingTracker,
    panes: Vec<AgentPaneProjection>,
    focus: &Focus,
    now: i64,
) -> Vec<WaitingEvent> {
    t.observe(&snap(panes), focus, now, &role)
}

fn on(p: &str) -> Focus {
    Focus::Pane(Some(p.into()))
}

fn began(p: &str) -> WaitingEvent {
    WaitingEvent::Began {
        pane_id: p.into(),
        role: role(p),
    }
}

#[test]
fn working_to_done_off_focus_waits() {
    let mut t = WaitingTracker::new();
    observe(&mut t, vec![pane("%1", 5, Working)], &on("%2"), 100);
    let ev = observe(&mut t, vec![pane("%1", 5, Done)], &on("%2"), 200);
    assert_eq!(ev, vec![began("%1")]);
    assert_eq!(t.view().since_ms.get("%1"), Some(&200));
}

#[test]
fn working_to_done_in_focus_does_not_wait() {
    let mut t = WaitingTracker::new();
    observe(&mut t, vec![pane("%1", 5, Working)], &on("%1"), 100);
    let ev = observe(&mut t, vec![pane("%1", 5, Done)], &on("%1"), 200);
    assert!(ev.is_empty());
    assert!(t.view().since_ms.is_empty());
}

#[test]
fn blocked_to_idle_off_focus_waits() {
    let mut t = WaitingTracker::new();
    observe(
        &mut t,
        vec![pane("%1", 5, Blocked)],
        &Focus::Pane(None),
        100,
    );
    let ev = observe(&mut t, vec![pane("%1", 5, Idle)], &Focus::Pane(None), 200);
    assert_eq!(ev, vec![began("%1")]);
    let ev = observe(
        &mut t,
        vec![pane("%1", 5, Working)],
        &Focus::Pane(None),
        250,
    );
    assert_eq!(ev.len(), 1);
    let ev = observe(&mut t, vec![pane("%1", 5, Error)], &Focus::Pane(None), 300);
    assert_eq!(ev, vec![began("%1")]);
}

#[test]
fn focus_on_a_waiting_pane_clears_it_and_records_waited_s() {
    let mut t = WaitingTracker::new();
    observe(&mut t, vec![pane("%1", 5, Working)], &on("%2"), 100);
    observe(&mut t, vec![pane("%1", 5, Done)], &on("%2"), 1_000);
    let ev = observe(&mut t, vec![pane("%1", 5, Done)], &on("%1"), 3_500);
    assert_eq!(
        ev,
        vec![WaitingEvent::Seen {
            pane_id: "%1".into(),
            role: role("%1"),
            waited_s: 2.5
        }]
    );
    assert!(t.view().since_ms.is_empty());
}

#[test]
fn working_again_clears_without_a_duration() {
    let mut t = WaitingTracker::new();
    observe(&mut t, vec![pane("%1", 5, Working)], &on("%2"), 100);
    observe(&mut t, vec![pane("%1", 5, Done)], &on("%2"), 200);
    let ev = observe(&mut t, vec![pane("%1", 5, Working)], &on("%2"), 300);
    assert_eq!(
        ev,
        vec![WaitingEvent::Ended {
            pane_id: "%1".into()
        }]
    );
    assert!(t.view().since_ms.is_empty());
}

#[test]
fn unknown_focus_changes_nothing() {
    let mut t = WaitingTracker::new();
    observe(
        &mut t,
        vec![pane("%1", 5, Working), pane("%2", 6, Working)],
        &on("%3"),
        100,
    );
    observe(
        &mut t,
        vec![pane("%1", 5, Done), pane("%2", 6, Working)],
        &on("%3"),
        200,
    );
    let ev = observe(
        &mut t,
        vec![pane("%1", 5, Done), pane("%2", 6, Done)],
        &Focus::Unknown,
        300,
    );
    assert!(ev.is_empty());
    assert_eq!(t.view().since_ms.len(), 1);
    assert!(t.view().since_ms.contains_key("%1"));
    let ev = observe(
        &mut t,
        vec![pane("%1", 5, Done), pane("%2", 6, Done)],
        &Focus::Unknown,
        400,
    );
    assert!(ev.is_empty());
}

#[test]
fn rest_state_when_focus_turns_known_does_not_wait() {
    let mut t = WaitingTracker::new();
    observe(&mut t, vec![pane("%1", 5, Done)], &Focus::Unknown, 100);
    let ev = observe(&mut t, vec![pane("%1", 5, Done)], &on("%2"), 200);
    assert!(ev.is_empty());
    assert!(t.view().since_ms.is_empty());
}

#[test]
fn remote_host_pane_waits_until_it_works_again() {
    let mut t = WaitingTracker::new();
    observe(&mut t, vec![pane("rex:h:%1", 5, Working)], &on("%2"), 100);
    let ev = observe(&mut t, vec![pane("rex:h:%1", 5, Done)], &on("%2"), 200);
    assert_eq!(ev, vec![began("rex:h:%1")]);
    let ev = observe(
        &mut t,
        vec![pane("rex:h:%1", 5, Done)],
        &on("rex:h:%1"),
        300,
    );
    assert!(ev.is_empty());
    assert_eq!(t.view().since_ms.len(), 1);
    let ev = observe(&mut t, vec![pane("rex:h:%1", 5, Working)], &on("%2"), 400);
    assert_eq!(
        ev,
        vec![WaitingEvent::Ended {
            pane_id: "rex:h:%1".into()
        }]
    );
}

#[test]
fn gone_pane_leaves() {
    let mut t = WaitingTracker::new();
    observe(&mut t, vec![pane("%1", 5, Working)], &on("%2"), 100);
    observe(&mut t, vec![pane("%1", 5, Done)], &on("%2"), 200);
    let ev = observe(&mut t, vec![], &on("%2"), 300);
    assert_eq!(
        ev,
        vec![WaitingEvent::Ended {
            pane_id: "%1".into()
        }]
    );
    assert!(t.view().since_ms.is_empty());
}

#[test]
fn new_agent_pid_in_same_pane_ends_the_old_wait() {
    let mut t = WaitingTracker::new();
    observe(&mut t, vec![pane("%1", 5, Working)], &on("%2"), 100);
    observe(&mut t, vec![pane("%1", 5, Done)], &on("%2"), 200);
    let ev = observe(&mut t, vec![pane("%1", 9, Done)], &on("%2"), 300);
    assert_eq!(
        ev,
        vec![WaitingEvent::Ended {
            pane_id: "%1".into()
        }]
    );
    assert!(t.view().since_ms.is_empty());
}

#[test]
fn view_reports_focus_known() {
    let mut t = WaitingTracker::new();
    assert!(!t.view().focus_known);
    observe(&mut t, vec![], &on("%2"), 100);
    assert!(t.view().focus_known);
    observe(&mut t, vec![], &Focus::Pane(None), 200);
    assert!(t.view().focus_known);
    observe(&mut t, vec![], &Focus::Unknown, 300);
    assert!(!t.view().focus_known);
}

#[test]
fn unknown_state_keeps_the_wait() {
    let mut t = WaitingTracker::new();
    observe(&mut t, vec![pane("%1", 5, Working)], &on("%2"), 100);
    observe(&mut t, vec![pane("%1", 5, Done)], &on("%2"), 200);
    let ev = observe(&mut t, vec![pane("%1", 5, Unknown)], &on("%2"), 300);
    assert!(ev.is_empty());
    assert_eq!(t.view().since_ms.len(), 1);
}
