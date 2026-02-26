use crate::route_reply::{
    PaneInfo, is_claude_code_process, parse_tag, resolve_pane, set_last_notified_pane,
    strip_control,
};
use crate::settings::init_settings_for_test;

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
    let panes = vec![
        PaneInfo {
            pane_id: "%1".into(),
            label: "work:0.0".into(),
        },
        PaneInfo {
            pane_id: "%2".into(),
            label: "home:0.1".into(),
        },
    ];
    let result = resolve_pane(Some("work:0.0"), "hi", &panes);
    assert!(result.is_some());
    assert_eq!(result.unwrap().0.pane_id, "%1");
}

#[test]
fn resolve_pane_substring_match() {
    let panes = vec![
        PaneInfo {
            pane_id: "%1".into(),
            label: "work:0.0".into(),
        },
        PaneInfo {
            pane_id: "%2".into(),
            label: "home:0.1".into(),
        },
    ];
    let result = resolve_pane(Some("home"), "hi", &panes);
    assert!(result.is_some());
    assert_eq!(result.unwrap().0.pane_id, "%2");
}

#[test]
fn resolve_pane_no_tag_falls_back_to_my_agent() {
    let panes = vec![PaneInfo {
        pane_id: "%1".into(),
        label: "my-agent:0.0".into(),
    }];
    let result = resolve_pane(None, "hi", &panes);
    assert!(result.is_some());
    assert_eq!(result.unwrap().0.pane_id, "%1");
}

#[test]
fn resolve_pane_my_agent_beats_last_notified() {
    // my-agent should win over last_notified_pane when no tag or semantic match.
    init_settings_for_test();
    let panes = vec![
        PaneInfo {
            pane_id: "%1".into(),
            label: "harold:0.3".into(),
        },
        PaneInfo {
            pane_id: "%2".into(),
            label: "my-agent:0.0".into(),
        },
    ];
    set_last_notified_pane(PaneInfo {
        pane_id: "%1".into(),
        label: "harold:0.3".into(),
    });
    let result = resolve_pane(None, "hi", &panes);
    assert!(result.is_some());
    assert_eq!(result.unwrap().0.pane_id, "%2");
}

#[test]
fn resolve_pane_no_match_returns_none() {
    let panes = vec![PaneInfo {
        pane_id: "%1".into(),
        label: "work:0.0".into(),
    }];
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
