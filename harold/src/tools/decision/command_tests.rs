use super::*;

fn parse_ok(args: &[&str]) -> DecisionCommand {
    parse(args).unwrap_or_else(|line| panic!("refused: {line}"))
}

fn full_add() -> Vec<&'static str> {
    vec![
        "add",
        "--role",
        "pnem",
        "--kind",
        "slicing",
        "--details-owner",
        "pnem:harold",
        "--accountable",
        "pnem",
        "--title",
        "Fold the naming fix into agent-restart",
        "--link",
        "spec:harold/pod-role-record",
        "--check",
        "2026-12-01",
        "--details",
        "pod:harold/specs/pod-role-record.decisions.md#d-20261114-02",
        "--asked-at",
        "2026-11-14T03:10Z",
    ]
}

#[test]
fn add_reads_every_flag_and_the_optional_ones_may_be_left_out() {
    assert_eq!(
        parse_ok(&full_add()),
        DecisionCommand::Add {
            role: "pnem".into(),
            kind: "slicing".into(),
            details_owner: "pnem:harold".into(),
            accountable: "pnem".into(),
            title: "Fold the naming fix into agent-restart".into(),
            link: "spec:harold/pod-role-record".into(),
            check: "2026-12-01".into(),
            details: "pod:harold/specs/pod-role-record.decisions.md#d-20261114-02".into(),
            asked_at: "2026-11-14T03:10Z".into(),
        }
    );
    let short = [
        "add",
        "--role",
        "chief",
        "--kind",
        "model",
        "--details-owner",
        "chief",
        "--accountable",
        "user",
        "--title",
        "T",
        "--check",
        "2026-12-01",
        "--details",
        "vault:decisions.md#d-20261114-03",
    ];
    let DecisionCommand::Add { link, asked_at, .. } = parse_ok(&short) else {
        panic!("not an add")
    };
    assert_eq!((link.as_str(), asked_at.as_str()), ("", ""));
}

#[test]
fn add_refuses_a_missing_required_flag_a_repeat_and_an_unknown_flag() {
    for skip in [
        "--role",
        "--kind",
        "--details-owner",
        "--accountable",
        "--title",
        "--check",
        "--details",
    ] {
        let mut args = full_add();
        let at = args.iter().position(|word| *word == skip).unwrap();
        args.drain(at..=at + 1);
        assert_eq!(parse(&args), Err(USAGE_LINE.into()), "{skip}");
    }
    let mut repeat = full_add();
    repeat.extend(["--role", "chief"]);
    assert_eq!(parse(&repeat), Err(USAGE_LINE.into()));
    let mut unknown = full_add();
    unknown.push("--wat");
    assert_eq!(parse(&unknown), Err(USAGE_LINE.into()));
    assert_eq!(parse(&["add", "--role"]), Err(USAGE_LINE.into()));
}

#[test]
fn list_show_evaluate_recheck_and_link_parse() {
    assert_eq!(
        parse_ok(&["list"]),
        DecisionCommand::List(ListFlags::default())
    );
    assert_eq!(
        parse_ok(&[
            "list",
            "--due",
            "--open",
            "--all",
            "--role",
            "chief",
            "--kind",
            "model",
            "--details-owner",
            "chief",
            "--accountable",
            "user",
            "--json"
        ]),
        DecisionCommand::List(ListFlags {
            due: true,
            open: true,
            all: true,
            role: "chief".into(),
            kind: "model".into(),
            details_owner: "chief".into(),
            accountable: "user".into(),
            json: true,
        })
    );
    assert_eq!(
        parse_ok(&["show", "d-20261114-02"]),
        DecisionCommand::Show {
            id: "d-20261114-02".into()
        }
    );
    assert_eq!(
        parse_ok(&["evaluate", "d-1", "--process", "good", "--outcome", "poor"]),
        DecisionCommand::Evaluate {
            id: "d-1".into(),
            process: "good".into(),
            outcome: "poor".into()
        }
    );
    assert_eq!(
        parse_ok(&["recheck", "d-1", "--check", "2026-12-25"]),
        DecisionCommand::Recheck {
            id: "d-1".into(),
            check: "2026-12-25".into()
        }
    );
    assert_eq!(
        parse_ok(&["link", "d-1", "work:thing"]),
        DecisionCommand::Link {
            id: "d-1".into(),
            link: "work:thing".into()
        }
    );
    for bad in [
        vec![],
        vec!["nope"],
        vec!["show"],
        vec!["evaluate", "d-1", "--process", "good"],
        vec!["recheck", "d-1"],
        vec!["link", "d-1"],
        vec!["list", "extra"],
        vec!["list", "--role"],
    ] {
        assert_eq!(parse(&bad), Err(USAGE_LINE.into()), "{bad:?}");
    }
}

/// The fields of one row, in order: id, date, role, kind, accountable, title, check date, status.
type RowFields<'a> = [&'a str; 8];

fn row(fields: RowFields) -> pb::Decision {
    let [id, on, role, kind, who, title, check, status] = fields;
    pb::Decision {
        id: id.into(),
        decided_on: on.into(),
        role: role.into(),
        kind: kind.into(),
        accountable: who.into(),
        title: title.into(),
        check_on: check.into(),
        status: status.into(),
        details_owner: "chief".into(),
        organisation: "personal-chief".into(),
        details: "vault:decisions.md#x".into(),
        ..Default::default()
    }
}

#[test]
fn the_list_text_has_the_columns_of_the_spec_and_a_count_line() {
    let rows = [
        row([
            "d-20261101-04",
            "2026-11-01",
            "chief",
            "model",
            "user",
            "Run two builds on Sonnet",
            "2026-11-14",
            "due",
        ]),
        row([
            "d-20261103-01",
            "2026-11-03",
            "pnem",
            "slicing",
            "pnem",
            "Split agent-jobs into hosts",
            "2026-11-13",
            "due",
        ]),
    ];
    let flags = ListFlags {
        due: true,
        ..ListFlags::default()
    };
    assert_eq!(
        list_text(&rows, &flags),
        "d-20261101-04  2026-11-01  chief  model    user  Run two builds on Sonnet     check 2026-11-14  due\n\
         d-20261103-01  2026-11-03  pnem   slicing  pnem  Split agent-jobs into hosts  check 2026-11-13  due\n\
         2 due"
    );
    assert_eq!(list_text(&[], &flags), "0 due");
    assert_eq!(list_text(&[], &ListFlags::default()), "0 open or due");
    assert_eq!(
        list_text(
            &[],
            &ListFlags {
                all: true,
                ..ListFlags::default()
            }
        ),
        "0 records"
    );
    assert_eq!(
        list_text(
            &[],
            &ListFlags {
                open: true,
                ..ListFlags::default()
            }
        ),
        "0 open"
    );
}

#[test]
fn json_holds_every_field() {
    let mut one = row([
        "d-1",
        "2026-11-01",
        "chief",
        "model",
        "user",
        "T",
        "2026-11-14",
        "due",
    ]);
    one.process_grade = "good".into();
    one.asked_at_ms = 5;
    let text = list_text(
        &[one],
        &ListFlags {
            json: true,
            ..ListFlags::default()
        },
    );
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    let first = &value[0];
    for key in [
        "id",
        "decided_on",
        "organisation",
        "role",
        "kind",
        "details_owner",
        "accountable",
        "title",
        "link",
        "check_on",
        "status",
        "process_grade",
        "outcome_grade",
        "details",
        "asked_at_ms",
        "added_at_ms",
        "evaluated_at_ms",
    ] {
        assert!(first.get(key).is_some(), "{key}");
    }
    assert_eq!(first["process_grade"], "good");
}

#[test]
fn the_one_line_answers_and_the_show_text() {
    let one = row([
        "d-20261114-02",
        "2026-11-14",
        "pnem",
        "slicing",
        "pnem",
        "Fold",
        "2026-12-01",
        "open",
    ]);
    assert_eq!(added_line(&one), "added d-20261114-02 (check 2026-12-01)");
    let mut graded = one.clone();
    graded.process_grade = "good".into();
    graded.outcome_grade = "poor".into();
    assert_eq!(
        evaluated_line(&graded),
        "evaluated d-20261114-02 (process good, outcome poor)"
    );
    assert_eq!(
        rechecked_line(&one),
        "rechecked d-20261114-02 (check 2026-12-01, open)"
    );
    let mut linked = one.clone();
    linked.link = "work:x".into();
    assert_eq!(linked_line(&linked), "linked d-20261114-02 to work:x");
    let shown = show_text(&one, "/pods/harold/entry.md");
    for expected in [
        "id: d-20261114-02",
        "role: pnem",
        "status: open",
        "details path: /pods/harold/entry.md",
    ] {
        assert!(shown.contains(expected), "{expected}\n{shown}");
    }
}

#[test]
fn a_refusal_gets_one_readable_line() {
    let line = |code, message: &str| refusal_line(&tonic::Status::new(code, message));
    assert_eq!(
        line(tonic::Code::PermissionDenied, "not_chief"),
        "only the chief's pane can run this command"
    );
    assert_eq!(
        line(tonic::Code::NotFound, "unknown_decision"),
        "unknown decision record"
    );
    assert_eq!(
        line(tonic::Code::FailedPrecondition, "already_evaluated"),
        "this decision record is evaluated already"
    );
    assert_eq!(
        line(tonic::Code::InvalidArgument, "details_missing"),
        "details_missing"
    );
    assert_eq!(
        line(tonic::Code::Unimplemented, ""),
        "the Harold daemon does not serve decision records"
    );
    assert_eq!(
        line(tonic::Code::Unavailable, ""),
        "the Harold daemon refused the call"
    );
}
