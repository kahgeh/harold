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

#[test]
fn typing_sends_the_text_as_literal_keys_and_enter_as_a_key_name() {
    assert_eq!(
        literal_text_args("%7", "digest"),
        ["send-keys", "-t", "%7", "-l", "digest"]
    );
    assert_eq!(enter_args("%7"), ["send-keys", "-t", "%7", "Enter"]);
}

#[test]
fn each_typing_step_succeeds_when_its_command_does() {
    assert_eq!(type_text_with("/usr/bin/true", "%7", "digest"), Ok(()));
    assert_eq!(press_enter_with("/usr/bin/true", "%7"), Ok(()));
}

#[test]
fn each_typing_step_reports_its_own_failure() {
    let text = type_text_with("/usr/bin/false", "%7", "digest").unwrap_err();
    assert!(
        text.starts_with("tmux send-keys text failed for %7: "),
        "{text}"
    );
    let enter = press_enter_with("/usr/bin/false", "%7").unwrap_err();
    assert!(
        enter.starts_with("tmux send-keys Enter failed for %7: "),
        "{enter}"
    );
}

#[test]
fn each_typing_step_reports_a_command_that_cannot_start() {
    let missing = "/nonexistent/harold-test-tmux";
    let text = type_text_with(missing, "%7", "digest").unwrap_err();
    assert!(
        text.starts_with("failed to start tmux send-keys for %7: "),
        "{text}"
    );
    let enter = press_enter_with(missing, "%7").unwrap_err();
    assert!(
        enter.starts_with("failed to start tmux Enter for %7: "),
        "{enter}"
    );
}

/// How many calls with this operation and outcome this thread recorded during `call`.
fn relay_calls(operation: &str, outcome: &str, call: impl FnOnce()) -> usize {
    crate::telemetry::test_support::noted_calls(operation, outcome, call).1
}

#[test]
fn typed_text_and_enter_are_counted_as_type_and_key_calls() {
    use crate::telemetry::test_support::fake_program;
    let good = fake_program("relay-ok", "exit 0");
    let bad = fake_program("relay-bad", "exit 1");
    let missing = "/nonexistent/harold-test-tmux";
    assert_eq!(
        relay_calls("type", "ok", || drop(type_text_with(&good, "%1", "hi"))),
        1
    );
    assert_eq!(
        relay_calls("type", "failed", || drop(type_text_with(&bad, "%1", "hi"))),
        1
    );
    assert_eq!(
        relay_calls("type", "absent", || drop(type_text_with(
            missing, "%1", "hi"
        ))),
        1
    );
    assert_eq!(
        relay_calls("key", "ok", || drop(press_enter_with(&good, "%1"))),
        1
    );
    assert_eq!(
        relay_calls("key", "failed", || drop(press_enter_with(&bad, "%1"))),
        1
    );
    assert_eq!(
        relay_calls("key", "absent", || drop(press_enter_with(missing, "%1"))),
        1
    );
}

// -- a pane id on another Rex host reaches no writer ---------------------------------------

const NO_TMUX: &str = "/nonexistent/harold-test-tmux";
const HOST_PANE: &str = "rex:omarchy/block:abc";

fn never_rex(_: &str, _: &str) -> Result<(), String> {
    panic!("the Rex writer must not run");
}

#[test]
fn typing_into_a_host_pane_is_refused_with_no_tmux_or_rex_call() {
    let error = type_text_routed(NO_TMUX, HOST_PANE, "text", never_rex).unwrap_err();
    assert!(error.contains("on_rex_host"), "{error}");
}

#[test]
fn typing_into_an_unknown_pane_id_is_invalid_pane_id() {
    let error = type_text_routed(NO_TMUX, "main:0.1", "text", never_rex).unwrap_err();
    assert!(error.contains("invalid_pane_id"), "{error}");
}

#[test]
fn enter_in_a_host_pane_is_refused_with_no_tmux_or_rex_call() {
    let error = press_enter_routed(NO_TMUX, HOST_PANE, never_rex).unwrap_err();
    assert!(error.contains("on_rex_host"), "{error}");
    let error = press_enter_routed(NO_TMUX, "main:0.1", never_rex).unwrap_err();
    assert!(error.contains("invalid_pane_id"), "{error}");
}

#[test]
fn a_tmux_pane_id_still_reaches_tmux_and_a_block_id_reaches_rex() {
    // The program does not exist: the error shows the call reached tmux.
    let error = type_text_routed(NO_TMUX, "%7", "text", never_rex).unwrap_err();
    assert!(error.contains("failed to start tmux"), "{error}");
    let error = press_enter_routed(NO_TMUX, "%7", never_rex).unwrap_err();
    assert!(error.contains("failed to start tmux"), "{error}");

    let mut pasted = None;
    type_text_routed(NO_TMUX, "block:abc", "a\x01b", |pane, text| {
        pasted = Some((pane.to_string(), text.to_string()));
        Ok(())
    })
    .unwrap();
    assert_eq!(pasted, Some(("block:abc".into(), "ab".into())));
    let mut key = None;
    press_enter_routed(NO_TMUX, "block:abc", |pane, k| {
        key = Some((pane.to_string(), k.to_string()));
        Ok(())
    })
    .unwrap();
    assert_eq!(key, Some(("block:abc".into(), "Enter".into())));
}

#[test]
fn relaying_to_a_host_pane_takes_no_lock_and_logs_no_text() {
    let logs = crate::outbound::fake::CapturedLogs::start();
    let pane = "rex:relay-host/block:relay1";

    let error = relay_to_tmux_pane(pane, "RELAY-CANARY-5521").unwrap_err();

    assert!(error.contains("on_rex_host"), "{error}");
    assert!(!crate::timed_prompts::has_pane_lock(pane));
    assert!(
        !logs.text().contains("RELAY-CANARY-5521"),
        "{}",
        logs.text()
    );
    assert!(
        !logs.text().contains("relay_to_tmux_pane"),
        "{}",
        logs.text()
    );

    let error = relay_to_tmux_pane("main:0.1", "RELAY-CANARY-5521").unwrap_err();
    assert!(error.contains("invalid_pane_id"), "{error}");
    assert!(!crate::timed_prompts::has_pane_lock("main:0.1"));
}
