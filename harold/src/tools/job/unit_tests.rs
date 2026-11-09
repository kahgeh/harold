use super::*;

fn state(active: &str, sub: &str, load: &str, status: Option<i32>) -> UnitState {
    UnitState {
        active: active.into(),
        sub: sub.into(),
        result: "success".into(),
        exit_status: status,
        load: load.into(),
    }
}

#[test]
fn active_and_activating_run_and_every_other_value_ends() {
    assert!(state("active", "running", "loaded", Some(0)).is_running());
    assert!(state("activating", "start", "loaded", None).is_running());
    assert!(!state("inactive", "dead", "loaded", Some(0)).is_running());
    assert!(!state("failed", "failed", "loaded", Some(2)).is_running());
    assert!(!state("active", "exited", "loaded", Some(0)).is_running());
    assert!(!state("inactive", "dead", "not-found", None).is_running());
}

#[test]
fn the_exit_code_is_exec_main_status_while_the_unit_exists_and_unknown_when_not_found() {
    assert_eq!(
        state("failed", "failed", "loaded", Some(2)).exit_code(),
        Some(2)
    );
    assert_eq!(
        state("active", "exited", "loaded", Some(0)).exit_code(),
        Some(0)
    );
    assert_eq!(
        state("inactive", "dead", "not-found", Some(0)).exit_code(),
        None
    );
}

#[test]
fn show_output_is_read_by_key_and_a_bad_number_gives_no_exit_status() {
    let text = "ActiveState=failed\nSubState=failed\nResult=exit-code\nExecMainStatus=2\nLoadState=loaded\n";
    let parsed = parse_show(text);
    assert_eq!(
        parsed,
        UnitState {
            active: "failed".into(),
            sub: "failed".into(),
            result: "exit-code".into(),
            exit_status: Some(2),
            load: "loaded".into(),
        }
    );
    assert_eq!(
        parse_show("LoadState=loaded\nExecMainStatus=x\n").exit_status,
        None
    );
    assert_eq!(parse_show("LoadState=not-found\n").load, "not-found");
}
