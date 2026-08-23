use super::*;

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
fn command_matches_default_agent_names() {
    let settings = AgentSettings::default();

    assert!(command_matches_agent("claude", &settings));
    assert!(command_matches_agent("codex", &settings));
    assert!(command_matches_agent("/Users/me/bin/codex", &settings));
    assert!(command_matches_agent("Claude", &settings));
    assert!(!command_matches_agent("python3.11", &settings));
    assert!(!command_matches_agent("bash", &settings));
    assert!(!command_matches_agent("node", &settings));
}

#[test]
fn command_matches_configured_agent_name_contains() {
    let settings = AgentSettings {
        command_contains: vec!["future-agent".to_string()],
    };

    assert!(command_matches_agent("future-agent-preview", &settings));
    assert!(!command_matches_agent("codex", &settings));
    assert!(!command_matches_agent("claude", &settings));
}

#[test]
fn process_tree_contains_agent_descendant() {
    let settings = AgentSettings::default();
    let processes = vec![
        ProcessInfo {
            pid: 10,
            ppid: 1,
            command: "/bin/zsh".to_string(),
        },
        ProcessInfo {
            pid: 11,
            ppid: 10,
            command: "/bin/zsh".to_string(),
        },
        ProcessInfo {
            pid: 12,
            ppid: 11,
            command: "codex".to_string(),
        },
    ];

    assert!(process_tree_contains_agent(10, &processes, &settings));
    assert!(process_tree_contains_agent(11, &processes, &settings));
    assert!(!process_tree_contains_agent(99, &processes, &settings));
}

#[test]
fn process_tree_contains_agent_pane_process() {
    let settings = AgentSettings::default();
    let processes = vec![ProcessInfo {
        pid: 10,
        ppid: 1,
        command: "claude".to_string(),
    }];

    assert!(process_tree_contains_agent(10, &processes, &settings));
}
