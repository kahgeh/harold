//! `harold get` with a name that is not a thing: nothing on standard output, exit 2.

use std::process::Command;

fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_harold"))
        .args(args)
        .env_clear()
        .output()
        .unwrap()
}

#[test]
fn get_an_unknown_thing_exits_2_with_nothing_on_standard_output() {
    let output = run(&["get", "bogus"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "unknown thing \"bogus\": voice, messaging, version, machine, multiplexer, hosts, chief, call\n"
    );
}

#[test]
fn get_without_a_name_or_with_extra_words_exits_2_with_nothing_on_standard_output() {
    for args in [&["get"][..], &["get", "voice", "x"][..]] {
        let output = run(args);
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).starts_with("unknown thing "));
    }
}
