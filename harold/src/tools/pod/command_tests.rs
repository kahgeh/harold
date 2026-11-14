use super::*;

fn answer() -> pb::WhoResponse {
    pb::WhoResponse {
        session_name: "main-b8".into(),
        record: Some(pb::PodRoleRecord {
            pane_id: "%7".into(),
            session_id: "s-1".into(),
            agent_pid: 10,
            agent_proc_start: 1000,
            folder: "/pod".into(),
            transcript_path: "/t/s-1.jsonl".into(),
            start_file: String::new(),
            started_at_ms: 5,
        }),
    }
}

#[test]
fn who_takes_a_product_a_role_and_an_optional_json_switch() {
    assert_eq!(
        parse(&["who", "harold", "pnem"]),
        Ok(PodCommand::Who {
            product: "harold".into(),
            role: "pnem".into(),
            json: false
        })
    );
    assert_eq!(
        parse(&["who", "-", "chief", "--json"]),
        Ok(PodCommand::Who {
            product: "-".into(),
            role: "chief".into(),
            json: true
        })
    );
}

#[test]
fn a_wrong_command_gives_the_usage_line() {
    for args in [
        &[][..],
        &["who"],
        &["who", "harold"],
        &["who", "harold", "pnem", "extra"],
        &["who", "harold", "pnem", "--json", "--json"],
        &["who", "harold", "pnem", "--wide"],
        &["list"],
    ] {
        assert_eq!(parse(args), Err(USAGE_LINE.to_string()), "{args:?}");
    }
}

#[test]
fn the_text_is_the_session_name_alone() {
    assert_eq!(render(&answer(), false), "main-b8");
}

#[test]
fn the_json_holds_the_name_and_the_whole_record() {
    let value: serde_json::Value = serde_json::from_str(&render(&answer(), true)).unwrap();
    assert_eq!(value["session_name"], "main-b8");
    assert_eq!(value["record"]["pane_id"], "%7");
    assert_eq!(value["record"]["agent_pid"], 10);
    assert_eq!(value["record"]["started_at_ms"], 5);
    assert_eq!(value["record"]["folder"], "/pod");
}

#[test]
fn each_reason_has_one_plain_line() {
    use tonic::{Code, Status};
    let line = |status: Status| refusal_line(&status);
    assert_eq!(line(Status::not_found("no_record")), "no_record");
    assert_eq!(
        line(Status::failed_precondition("agent_gone")),
        "agent_gone"
    );
    assert_eq!(
        line(Status::unavailable("session_file_unreadable")),
        "session_file_unreadable"
    );
    assert_eq!(
        line(Status::invalid_argument("bad_role")),
        "bad_role: the role must be chief, pnem or pod_coordination"
    );
    assert_eq!(
        line(Status::new(Code::Unimplemented, "")),
        "the Harold daemon does not serve the pod tool"
    );
}

#[test]
fn check_takes_a_product_a_role_and_an_optional_spec_id() {
    assert_eq!(
        parse(&["check", "harold", "pnem"]),
        Ok(PodCommand::Check {
            product: "harold".into(),
            role: "pnem".into(),
            spec: None
        })
    );
    assert_eq!(
        parse(&[
            "check",
            "harold",
            "build_worker",
            "--spec",
            "decision-tool-a1"
        ]),
        Ok(PodCommand::Check {
            product: "harold".into(),
            role: "build_worker".into(),
            spec: Some("decision-tool-a1".into())
        })
    );
    for args in [
        &["check"][..],
        &["check", "harold"],
        &["check", "harold", "pnem", "--spec"],
        &["check", "harold", "pnem", "--wide", "x"],
    ] {
        assert_eq!(parse(args), Err(USAGE_LINE.to_string()), "{args:?}");
    }
}

#[test]
fn a_check_refusal_is_one_line_with_its_reason() {
    for (status, line) in [
        (tonic::Status::not_found("no_record"), "no_record"),
        (
            tonic::Status::unavailable("restart_check_off"),
            "restart_check_off",
        ),
        (tonic::Status::invalid_argument("bad_spec"), "bad_spec"),
    ] {
        assert_eq!(refusal_line(&status), line);
    }
}
