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
            cost_usd: Some(0.012)
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
            cost_usd: Some(0.01)
        }
    );
    assert_eq!(
        case(
            r#"{"subtype":"error_max_budget_usd","is_error":true,"total_cost_usd":0.05}"#,
            false
        ),
        Read {
            answer: Err(Failure::RunBudgetReached),
            cost_usd: Some(0.05)
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
fn failure_names_round_trip() {
    for failure in Failure::ALL {
        assert_eq!(Failure::parse(failure.as_str()), Some(failure));
    }
    assert_eq!(Failure::parse("quiet"), None);
}
