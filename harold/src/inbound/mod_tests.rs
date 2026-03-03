use std::sync::Mutex;

use crate::inbound::{
    AgentAddress, clear_routing_state, parse_tag, resolve_pane,
    set_last_away_notification_source_agent,
};
use crate::settings::init_settings_for_test;

/// Serialises tests that mutate global routing state.
static ROUTING_TEST_LOCK: Mutex<()> = Mutex::new(());

fn tmux(pane_id: &str, label: &str) -> AgentAddress {
    AgentAddress::TmuxPane {
        pane_id: pane_id.into(),
        label: label.into(),
    }
}

#[test]
fn parse_tag_with_tag() {
    let (tag, body) = parse_tag("[main] hello world");
    assert_eq!(tag, Some("main"));
    assert_eq!(body, "hello world");
}

#[test]
fn parse_tag_without_tag() {
    let (tag, body) = parse_tag("just a message");
    assert_eq!(tag, None);
    assert_eq!(body, "just a message");
}

#[test]
fn parse_tag_unclosed_bracket() {
    let (tag, body) = parse_tag("[unclosed message");
    assert_eq!(tag, None);
    assert_eq!(body, "[unclosed message");
}

#[test]
fn resolve_pane_exact_match() {
    let panes = vec![tmux("%1", "work:0.0"), tmux("%2", "home:0.1")];
    let result = resolve_pane(Some("work:0.0"), "hi", &panes);
    assert!(result.is_some());
    assert_eq!(result.unwrap().0.pane_id(), "%1");
}

#[test]
fn resolve_pane_substring_match() {
    let panes = vec![tmux("%1", "work:0.0"), tmux("%2", "home:0.1")];
    let result = resolve_pane(Some("home"), "hi", &panes);
    assert!(result.is_some());
    assert_eq!(result.unwrap().0.pane_id(), "%2");
}

#[test]
fn resolve_pane_no_tag_falls_back_to_my_agent() {
    let _lock = ROUTING_TEST_LOCK.lock().unwrap();
    clear_routing_state();
    let panes = vec![tmux("%1", "my-agent:0.0")];
    let result = resolve_pane(None, "hi", &panes);
    assert!(result.is_some());
    assert_eq!(result.unwrap().0.pane_id(), "%1");
}

#[test]
fn resolve_pane_last_away_notification_source_beats_my_agent() {
    let _lock = ROUTING_TEST_LOCK.lock().unwrap();
    init_settings_for_test();
    clear_routing_state();
    let panes = vec![tmux("%3", "alir-app:0.1"), tmux("%4", "my-agent:0.0")];
    set_last_away_notification_source_agent(tmux("%3", "alir-app:0.1"));
    let result = resolve_pane(None, "hi", &panes);
    assert!(result.is_some());
    assert_eq!(result.unwrap().0.pane_id(), "%3");
}

#[test]
fn resolve_pane_no_match_returns_none() {
    let panes = vec![tmux("%1", "work:0.0")];
    let result = resolve_pane(Some("nonexistent"), "hi", &panes);
    assert!(result.is_none());
}
