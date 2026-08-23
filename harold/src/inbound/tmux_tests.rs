use super::*;
use crate::agent::domain::{AgentIncarnation, AgentPaneObservation};

#[test]
fn strip_control_removes_ansi_and_controls() {
    let input = "\x1b[31mred\x1b[0m normal\x01hidden";
    let output = strip_control(input);
    assert_eq!(output, "red normalhidden");
}

#[test]
fn strip_control_removes_lone_control_chars() {
    let input = "clean\x01";
    let output = strip_control(input);
    assert_eq!(output, "clean");
}

#[test]
fn inventory_observation_preserves_inbound_pane_address() {
    let address = observation_to_address(AgentPaneObservation {
        incarnation: AgentIncarnation {
            pane_id: "%7".to_string(),
            pane_pid: 10,
            agent_pid: 20,
            agent_started_at_ms: 1_000,
            provider_id: "codex".to_string(),
        },
        tmux_target: "harold:2.1".to_string(),
        session_name: "harold".to_string(),
        window_index: 2,
        pane_index: 1,
        working_directory: "/work/harold".to_string(),
        provider_display_name: "Codex".to_string(),
        observed_at_ms: 99,
    });

    assert_eq!(address.pane_id(), "%7");
    assert_eq!(address.label(), "harold:2.1");
}

#[test]
fn inventory_observation_sanitizes_and_bounds_inbound_pane_label() {
    let sentinel = "HOSTILE_TMUX_TARGET";
    let address = observation_to_address(AgentPaneObservation {
        incarnation: AgentIncarnation {
            pane_id: "%7".to_string(),
            pane_pid: 10,
            agent_pid: 20,
            agent_started_at_ms: 1_000,
            provider_id: "codex".to_string(),
        },
        tmux_target: format!("harold\u{1b}]0;{sentinel}\u{7}:{}", "pane".repeat(100)),
        session_name: "harold".to_string(),
        window_index: 2,
        pane_index: 1,
        working_directory: "/work/harold".to_string(),
        provider_display_name: "Codex".to_string(),
        observed_at_ms: 99,
    });

    assert_eq!(address.pane_id(), "%7");
    assert!(address.label().starts_with("harold:"));
    assert_eq!(address.label().chars().count(), 256);
    assert!(!address.label().contains(sentinel));
    assert!(!address.label().chars().any(char::is_control));
    assert!(!format!("{address:?}").contains(sentinel));
}
