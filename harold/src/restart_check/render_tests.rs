use super::*;

fn link(label: &[&str], outgoing: Outcome, incoming: Outcome) -> LinkRow {
    LinkRow {
        label: label.iter().map(|part| (*part).to_string()).collect(),
        outgoing,
        incoming,
    }
}

fn report(
    agent: &Agent,
    pane: &str,
    session: &str,
    time: &str,
    identity: IdentityRow,
    communication: LinkTable,
    fails: Vec<FailLine>,
) -> Report {
    Report {
        agent: agent.clone(),
        session: session.into(),
        pane: pane.into(),
        time: time.into(),
        identity,
        communication,
        fails,
        not_checked: vec![],
    }
}

#[test]
fn the_chief_report_shows_two_groups_and_names_the_chief_as_the_one_who_fixes_it() {
    let chief = report(
        &Agent::Chief,
        "7404f1",
        "chief",
        "2026-11-14 16:02",
        IdentityRow {
            outcome: Outcome::Ok,
            subject: "chief".into(),
            detail: "pane 7404f1 in session chief".into(),
        },
        LinkTable {
            header: ["pod", "agent", "chief -> agent", "agent -> chief"]
                .map(String::from)
                .to_vec(),
            rows: vec![
                link(&["harold", "pnem"], Outcome::Ok, Outcome::Ok),
                link(&["keymap", "pod-coordination"], Outcome::Ok, Outcome::Fail),
            ],
        },
        vec![FailLine {
            group: Group::Communication,
            text:
                "keymap pod-coordination -> chief: it still sends to the old name chief-of-pods-98"
                    .into(),
        }],
    );
    let expected = "\
restart check: chief  (session chief, pane 7404f1)  2026-11-14 16:02
identity
  ok    chief   pane 7404f1 in session chief

communication
  pod      agent              chief -> agent   agent -> chief
  harold   pnem               ok               ok
  keymap   pod-coordination   ok               FAIL

  FAIL keymap pod-coordination -> chief: it still sends to the old name chief-of-pods-98

result: 1 FAIL -> the chief fixes it; nothing for the user
";
    assert_eq!(render(&chief), expected);
}

#[test]
fn a_pnem_report_with_no_fail_gives_result_ok_and_no_stage_2_group() {
    let pnem = report(
        &Agent::Pnem("harold".into()),
        "790ae0",
        "harold",
        "2026-11-14 16:05",
        IdentityRow {
            outcome: Outcome::Ok,
            subject: "pnem harold".into(),
            detail: "pane 790ae0 in session harold".into(),
        },
        LinkTable {
            header: ["", "pnem -> agent", "agent -> pnem"]
                .map(String::from)
                .to_vec(),
            rows: vec![
                link(&["chief"], Outcome::Ok, Outcome::Ok),
                link(&["pod-coordination harold"], Outcome::Ok, Outcome::Ok),
            ],
        },
        vec![],
    );
    let text = render(&pnem);
    assert!(text.starts_with(
        "restart check: pnem harold  (session harold, pane 790ae0)  2026-11-14 16:05\n"
    ));
    assert!(text.contains("  chief                     ok              ok\n"));
    assert!(text.ends_with("result: ok\n"));
    for word in ["setup", "memory", "stage"] {
        assert!(!text.contains(word), "the text holds {word}");
    }
}

#[test]
fn a_build_worker_identity_fail_names_the_fix_and_goes_to_pod_coordination() {
    let worker = report(
        &Agent::BuildWorker {
            product: "harold".into(),
            spec: "decision-tool-a1".into(),
        },
        "41d7a0",
        "decision-tool",
        "2026-11-14 16:10",
        IdentityRow {
            outcome: Outcome::Fail,
            subject: "worker decision-tool-a1".into(),
            detail: "Harold has pane 3c19e2; the agent now runs in pane 41d7a0".into(),
        },
        LinkTable {
            header: ["", "worker -> agent", "agent -> worker"]
                .map(String::from)
                .to_vec(),
            rows: vec![link(
                &["pod-coordination harold"],
                Outcome::NotChecked,
                Outcome::Ok,
            )],
        },
        vec![
            FailLine {
                group: Group::Identity,
                text: "worker: Harold updates its record to pane 41d7a0, then checks again".into(),
            },
            FailLine {
                group: Group::Communication,
                text: "pod-coordination harold -> worker: socket_unknown".into(),
            },
        ],
    );
    let mut worker = worker;
    worker.not_checked =
        vec!["worker decision-tool-a1 -> pod-coordination harold: socket_unknown".into()];
    let text = render(&worker);
    assert!(text.contains(
        "\n  not checked worker decision-tool-a1 -> pod-coordination harold: socket_unknown\n"
    ));
    assert!(text.contains("restart check: worker decision-tool-a1  (session decision-tool, pane 41d7a0)  2026-11-14 16:10\n"));
    assert!(text.contains("  FAIL  worker decision-tool-a1   Harold has pane 3c19e2; the agent now runs in pane 41d7a0\n"));
    assert!(text.contains("not checked"));
    assert!(text.ends_with("result: 2 FAIL -> sent to pod-coordination\n"));
}

#[test]
fn the_recipient_of_each_agent_and_the_count_words() {
    assert_eq!(Agent::Chief.recipient(), Recipient::Chief);
    assert_eq!(Agent::Pnem("h".into()).recipient(), Recipient::Chief);
    assert_eq!(
        Agent::PodCoordination("h".into()).recipient(),
        Recipient::Chief
    );
    let worker = Agent::BuildWorker {
        product: "h".into(),
        spec: "s".into(),
    };
    assert_eq!(worker.recipient(), Recipient::PodCoordination);
    assert_eq!(Agent::Chief.title(), "chief");
    assert_eq!(worker.title(), "worker s");
}

#[test]
fn a_name_with_a_newline_or_a_control_character_gives_one_clean_line_of_at_most_80_characters() {
    let dirty = format!("a\nb\u{7}c{}", "x".repeat(120));
    let clean = clean_name(&dirty);
    assert_eq!(clean.chars().count(), 80);
    assert!(!clean.contains('\n') && !clean.chars().any(char::is_control));
    assert!(clean.starts_with("a b c"));
}
