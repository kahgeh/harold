use super::*;

#[test]
fn quiet_and_report_are_read() {
    assert_eq!(parse_answer("QUIET"), Some(Answer::Quiet));
    assert_eq!(parse_answer("  QUIET\n"), Some(Answer::Quiet));
    assert_eq!(
        parse_answer("REPORT harold: watch-jobs merged"),
        Some(Answer::Report("harold: watch-jobs merged".into()))
    );
}

#[test]
fn anything_else_is_unreadable() {
    for text in [
        "",
        "REPORT",
        "REPORT   ",
        "REPORT\n",
        "REPORTED x",
        "All good!",
        "quiet",
        "QUIET please",
        "report x",
    ] {
        assert_eq!(parse_answer(text), None, "{text:?}");
    }
}

#[test]
fn escapes_and_newlines_are_removed_and_only_the_first_line_is_kept() {
    assert_eq!(
        parse_answer("REPORT a \u{1b}[31mred\u{1b}[0m thing\nsecond line\nthird"),
        Some(Answer::Report("a red thing".into()))
    );
    assert_eq!(
        parse_answer("REPORT x\r\ny"),
        Some(Answer::Report("x".into()))
    );
    assert_eq!(
        parse_answer("REPORT bell\u{7} and\u{0} nul"),
        Some(Answer::Report("bell and nul".into()))
    );
    assert_eq!(parse_answer("\u{1b}[1mQUIET\u{1b}[0m"), Some(Answer::Quiet));
}

#[test]
fn a_long_line_is_cut_to_300_characters_on_a_character_boundary() {
    let long = format!("REPORT {}", "é".repeat(400));
    let Some(Answer::Report(line)) = parse_answer(&long) else {
        panic!("not a report")
    };
    assert_eq!(line.chars().count(), 300);
    assert!(line.chars().all(|c| c == 'é'));
}

#[test]
fn the_system_prompt_states_the_format_and_not_what_is_worth_reporting() {
    let prompt = system_prompt("SKILL BODY");
    assert!(prompt.contains("QUIET") && prompt.contains("REPORT"));
    assert!(prompt.ends_with("SKILL BODY"));
    let fixed = system_prompt("").to_lowercase();
    for word in [
        "problem", "worth", "merged", "idle", "slot", "stuck", "error",
    ] {
        assert!(!fixed.contains(word), "the fixed prompt says {word:?}");
    }
}

fn output(extra: &str) -> Vec<u8> {
    format!("{{{extra}}}").into_bytes()
}

#[test]
fn a_good_output_gives_the_answer_and_the_cost() {
    let read = read_output(
        &output(
            r#""type":"result","subtype":"success","is_error":false,"result":"REPORT x","total_cost_usd":0.012"#,
        ),
        true,
    );
    assert_eq!(
        read,
        Read {
            answer: Ok(Answer::Report("x".into())),
            cost_usd: Some(0.012),
            cause: Some(Cause {
                subtype: Some("success".into()),
                is_error: Some(false),
                num_turns: None,
                terminal_reason: None,
                api_error_status: None
            })
        }
    );
}

#[test]
fn bad_outputs_map_to_their_failures() {
    let case = |json: &str, ok: bool| read_output(json.as_bytes(), ok);
    assert_eq!(case("not json", true).answer, Err(Failure::ProcessFailed));
    assert_eq!(case("", false).answer, Err(Failure::ProcessFailed));
    assert_eq!(
        case(
            r#"{"subtype":"success","is_error":false,"result":"All good!","total_cost_usd":0.01}"#,
            true
        ),
        Read {
            answer: Err(Failure::UnreadableAnswer),
            cost_usd: Some(0.01),
            cause: Some(Cause {
                subtype: Some("success".into()),
                is_error: Some(false),
                num_turns: None,
                terminal_reason: None,
                api_error_status: None
            })
        }
    );
    assert_eq!(
        case(
            r#"{"subtype":"error_max_budget_usd","is_error":true,"total_cost_usd":0.05}"#,
            false
        ),
        Read {
            answer: Err(Failure::RunBudgetReached),
            cost_usd: Some(0.05),
            cause: Some(Cause {
                subtype: Some("error_max_budget_usd".into()),
                is_error: Some(true),
                num_turns: None,
                terminal_reason: None,
                api_error_status: None
            })
        }
    );
    assert_eq!(
        case(
            r#"{"subtype":"success","is_error":true,"result":"Not logged in"}"#,
            true
        )
        .answer,
        Err(Failure::ProcessFailed)
    );
    assert_eq!(
        case(r#"{"subtype":"error_max_turns","is_error":true}"#, true).answer,
        Err(Failure::MaxTurnsReached)
    );
    assert_eq!(
        case(
            r#"{"subtype":"error_during_execution","is_error":true}"#,
            false
        )
        .answer,
        Err(Failure::ProcessFailed)
    );
    assert_eq!(
        case(r#"{"subtype":"success","result":"QUIET"}"#, false).answer,
        Err(Failure::ProcessFailed)
    );
    // A cost that is negative or missing is unknown.
    assert_eq!(
        case(
            r#"{"subtype":"success","result":"QUIET","total_cost_usd":-1}"#,
            true
        )
        .cost_usd,
        None
    );
}

#[test]
fn failure_names_are_the_reasons_the_log_carries() {
    let names: Vec<&str> = Failure::ALL.into_iter().map(Failure::as_str).collect();
    assert_eq!(
        names,
        [
            "timeout",
            "process_failed",
            "run_budget_reached",
            "unreadable_answer",
            "skill_missing",
            "max_turns_reached"
        ]
    );
}

#[test]
fn a_turn_cap_run_exiting_non_zero_keeps_its_cause() {
    let read = read_output(
        br#"{"subtype":"error_max_turns","is_error":true,"num_turns":9}"#,
        false,
    );
    assert_eq!(read.answer, Err(Failure::MaxTurnsReached));
    assert_eq!(read.cause.unwrap().num_turns, Some(9));
}

#[test]
fn num_turns_reads_whole_numbers_and_digit_strings_only() {
    let turns = |value: &str| cause_of(&format!(r#"{{"num_turns":{value}}}"#)).num_turns;
    assert_eq!(turns("8"), Some(8));
    assert_eq!(turns("8.0"), Some(8));
    assert_eq!(turns(r#""8""#), Some(8));
    for odd in [
        "8.5",
        r#""x""#,
        "-1",
        "true",
        "null",
        r#""""#,
        "1e20",
        "99999999999",
    ] {
        assert_eq!(turns(odd), None, "{odd}");
    }
}

#[test]
fn an_odd_num_turns_does_not_spoil_a_good_run() {
    let read = read_output(
        br#"{"subtype":"success","is_error":false,"result":"QUIET","num_turns":"x"}"#,
        true,
    );
    assert_eq!(read.answer, Ok(Answer::Quiet));
    assert_eq!(read.cause.unwrap().num_turns, None);
}

fn cause_of(json: &str) -> Cause {
    read_output(json.as_bytes(), false).cause.unwrap()
}

#[test]
fn the_cause_is_read_from_the_json() {
    let read = read_output(
        br#"{"subtype":"error_max_turns","is_error":true,"num_turns":8}"#,
        true,
    );
    assert_eq!(read.answer, Err(Failure::MaxTurnsReached));
    assert_eq!(
        read.cause,
        Some(Cause {
            subtype: Some("error_max_turns".into()),
            is_error: Some(true),
            num_turns: Some(8),
            terminal_reason: None,
            api_error_status: None
        })
    );
    let none = cause_of("{}");
    assert_eq!(
        (none.subtype, none.is_error, none.num_turns),
        (None, None, None)
    );
}

#[test]
fn an_out_of_shape_subtype_becomes_other() {
    let subtype = |value: &str| cause_of(&format!(r#"{{"subtype":"{value}"}}"#)).subtype;
    assert_eq!(subtype("Bad subtype!").as_deref(), Some("other"));
    assert_eq!(subtype("Upper").as_deref(), Some("other"));
    assert_eq!(subtype("").as_deref(), Some("other"));
    assert_eq!(subtype(&"a".repeat(41)).as_deref(), Some("other"));
    assert_eq!(subtype(&"a".repeat(40)), Some("a".repeat(40)));
    assert_eq!(
        subtype("error_during_execution").as_deref(),
        Some("error_during_execution")
    );
}

#[test]
fn output_that_is_not_json_is_unreadable_json() {
    for text in ["not json", ""] {
        let read = read_output(text.as_bytes(), true);
        assert_eq!(
            read.cause,
            Some(Cause {
                subtype: Some("unreadable_json".into()),
                is_error: None,
                num_turns: None,
                terminal_reason: None,
                api_error_status: None
            })
        );
    }
}

#[test]
fn an_api_error_names_its_terminal_reason_and_status() {
    let cause = cause_of(
        r#"{"type":"result","subtype":"success","is_error":true,"num_turns":1,"total_cost_usd":0,"terminal_reason":"api_error","api_error_status":404,"result":"CAUSE-CANARY"}"#,
    );
    assert_eq!(cause.terminal_reason.as_deref(), Some("api_error"));
    assert_eq!(cause.api_error_status, Some(404));
}

#[test]
fn an_odd_terminal_reason_becomes_other() {
    let reason =
        |value: &str| cause_of(&format!(r#"{{"terminal_reason":"{value}"}}"#)).terminal_reason;
    assert_eq!(reason(&"a".repeat(41)).as_deref(), Some("other"));
    assert_eq!(reason("Bad reason!").as_deref(), Some("other"));
    assert_eq!(reason("").as_deref(), Some("other"));
    assert_eq!(reason(&"a".repeat(40)), Some("a".repeat(40)));
    assert_eq!(cause_of(r#"{"terminal_reason":7}"#).terminal_reason, None);
}

#[test]
fn an_api_error_status_is_kept_only_as_a_whole_number_from_100_to_599() {
    let status =
        |value: &str| cause_of(&format!(r#"{{"api_error_status":{value}}}"#)).api_error_status;
    assert_eq!(status("100"), Some(100));
    assert_eq!(status("599"), Some(599));
    for odd in [r#""404""#, "99", "600", "4.5", "-404", "null", "true"] {
        assert_eq!(status(odd), None, "{odd}");
    }
}

#[test]
fn output_without_either_field_gives_none_for_both() {
    let cause = cause_of(r#"{"subtype":"success","is_error":true}"#);
    assert_eq!(
        (cause.terminal_reason, cause.api_error_status),
        (None, None)
    );
}
