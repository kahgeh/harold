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
fn node_semver_process_matches_node_version() {
    assert!(node_semver_process("16.20.1"));
    assert!(node_semver_process("20.11.0"));
    assert!(!node_semver_process("python3.11"));
    assert!(!node_semver_process("bash"));
    assert!(!node_semver_process("node"));
}
