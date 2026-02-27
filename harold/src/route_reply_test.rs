use std::sync::Mutex;

use crate::route_reply::{
    AgentAddress, clear_routing_state, is_claude_code_process, parse_tag, resolve_pane,
    set_last_away_notification_source_agent, set_last_routed_agent, strip_control,
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
    assert_eq!(result.unwrap().0.tmux_pane_id(), "%1");
}

#[test]
fn resolve_pane_substring_match() {
    let panes = vec![tmux("%1", "work:0.0"), tmux("%2", "home:0.1")];
    let result = resolve_pane(Some("home"), "hi", &panes);
    assert!(result.is_some());
    assert_eq!(result.unwrap().0.tmux_pane_id(), "%2");
}

#[test]
fn resolve_pane_no_tag_falls_back_to_my_agent() {
    let _lock = ROUTING_TEST_LOCK.lock().unwrap();
    clear_routing_state();
    let panes = vec![tmux("%1", "my-agent:0.0")];
    let result = resolve_pane(None, "hi", &panes);
    assert!(result.is_some());
    assert_eq!(result.unwrap().0.tmux_pane_id(), "%1");
}

#[test]
fn resolve_pane_last_routed_agent_beats_my_agent() {
    let _lock = ROUTING_TEST_LOCK.lock().unwrap();
    // last_routed_agent should win over my-agent when no tag or semantic match.
    init_settings_for_test();
    clear_routing_state();
    let panes = vec![tmux("%1", "harold:0.3"), tmux("%2", "my-agent:0.0")];
    set_last_routed_agent(tmux("%1", "harold:0.3"));
    let result = resolve_pane(None, "hi", &panes);
    assert!(result.is_some());
    assert_eq!(result.unwrap().0.tmux_pane_id(), "%1");
}

#[test]
fn resolve_pane_last_away_notification_source_beats_my_agent() {
    let _lock = ROUTING_TEST_LOCK.lock().unwrap();
    // last_away_notification_source_agent should win over my-agent when no routed agent.
    init_settings_for_test();
    clear_routing_state();
    let panes = vec![tmux("%3", "alir-app:0.1"), tmux("%4", "my-agent:0.0")];
    set_last_away_notification_source_agent(tmux("%3", "alir-app:0.1"));
    let result = resolve_pane(None, "hi", &panes);
    assert!(result.is_some());
    assert_eq!(result.unwrap().0.tmux_pane_id(), "%3");
}

#[test]
fn resolve_pane_no_match_returns_none() {
    let panes = vec![tmux("%1", "work:0.0")];
    let result = resolve_pane(Some("nonexistent"), "hi", &panes);
    assert!(result.is_none());
}

#[test]
fn strip_control_removes_ansi_and_controls() {
    // ANSI escape sequences are stripped; \x01 (SOH) is a control char and stripped;
    // the text "hidden" after it is plain ASCII and passes through.
    let input = "\x1b[31mred\x1b[0m normal\x01hidden";
    let output = strip_control(input);
    assert_eq!(output, "red normalhidden");
}

#[test]
fn strip_control_removes_lone_control_chars() {
    // A lone \x01 with no trailing text is stripped entirely.
    let input = "clean\x01";
    let output = strip_control(input);
    assert_eq!(output, "clean");
}

#[test]
fn is_claude_code_process_matches_node_version() {
    assert!(is_claude_code_process("16.20.1"));
    assert!(is_claude_code_process("20.11.0"));
    assert!(!is_claude_code_process("python3.11"));
    assert!(!is_claude_code_process("bash"));
    assert!(!is_claude_code_process("node"));
}
