use super::*;

fn option(key: &str) -> pb::DecisionOption {
    pb::DecisionOption {
        key: key.into(),
        label: format!("label {key}"),
        impact: "impact".into(),
        suggested: false,
        free: false,
    }
}

fn free_option(key: &str) -> pb::DecisionOption {
    pb::DecisionOption {
        free: true,
        ..option(key)
    }
}

fn decision(ask: &str, options: Vec<pb::DecisionOption>) -> pb::Decision {
    pb::Decision {
        ask: ask.into(),
        options,
    }
}

fn proto() -> pb::Question {
    pb::Question {
        id: "Q25".into(),
        product: "turso-durable".into(),
        spec: "durable-server".into(),
        sha: "d84bf386a2a5".into(),
        link: "https://claude.ai/artifact/x".into(),
        intro: "intro".into(),
        decisions: vec![
            decision("First?", vec![option("a"), option("b"), free_option("z")]),
            decision("Second?", vec![option("a"), option("b")]),
        ],
    }
}

fn pick(decision: u32, key: &str, text: &str) -> pb::Pick {
    pb::Pick {
        decision,
        key: key.into(),
        text: text.into(),
    }
}

#[test]
fn a_valid_question_converts() {
    let q = from_proto(proto()).unwrap();
    assert_eq!(q.id, "Q25");
    assert_eq!(q.decisions.len(), 2);
    assert!(q.decisions[0].options[2].free);
    assert_eq!(to_proto(&q), proto());
}

#[test]
fn bad_link_id_duplicate_key_and_second_suggested_name_their_field() {
    let mut p = proto();
    p.link = "ftp://x".into();
    assert_eq!(from_proto(p).unwrap_err(), "link");

    let mut p = proto();
    p.id = "has space".into();
    assert_eq!(from_proto(p).unwrap_err(), "id");

    let mut p = proto();
    p.decisions[0].options[1].key = "a".into();
    assert_eq!(from_proto(p).unwrap_err(), "decisions.1.key");

    let mut p = proto();
    p.decisions[1].options[0].suggested = true;
    p.decisions[1].options[1].suggested = true;
    assert_eq!(from_proto(p).unwrap_err(), "decisions.2.suggested");

    let mut p = proto();
    p.decisions[0].options[0].suggested = true;
    p.decisions[0].options[2].suggested = true;
    assert_eq!(from_proto(p).unwrap_err(), "decisions.1.suggested");
}

#[test]
fn an_id_over_32_or_a_field_over_2000_is_refused() {
    let mut p = proto();
    p.id = "a".repeat(33);
    assert_eq!(from_proto(p).unwrap_err(), "id");
    let mut p = proto();
    p.id = "a".repeat(32);
    assert!(from_proto(p).is_ok());

    let mut p = proto();
    p.intro = "x".repeat(2001);
    assert_eq!(from_proto(p).unwrap_err(), "intro");

    let mut p = proto();
    p.decisions[1].options[0].impact = "x".repeat(2001);
    assert_eq!(from_proto(p).unwrap_err(), "decisions.2.options.1.impact");

    let mut p = proto();
    p.decisions[0].ask = "x".repeat(2001);
    assert_eq!(from_proto(p).unwrap_err(), "decisions.1.ask");

    let mut p = proto();
    p.decisions[0].options = (0..10).map(|i| option(&i.to_string())).collect();
    assert_eq!(from_proto(p).unwrap_err(), "decisions.1.options");

    let mut p = proto();
    p.decisions.clear();
    assert_eq!(from_proto(p).unwrap_err(), "decisions");
}

#[test]
fn picks_must_fit_the_question() {
    let q = from_proto(proto()).unwrap();
    let good = [pick(0, "a", ""), pick(1, "b", "")];

    let answer = resolve_picks(&q, Kind::Answer, &good, "").unwrap();
    assert_eq!(answer.picks[0].ask, "First?");
    assert_eq!(answer.picks[0].label, "label a");
    assert!(!answer.picks[0].free);

    // too few, out of range, unknown key, wrong order, a free pick for answer
    assert_eq!(
        resolve_picks(&q, Kind::Answer, &good[..1], "").unwrap_err(),
        "picks"
    );
    assert_eq!(
        resolve_picks(&q, Kind::Answer, &[pick(0, "a", ""), pick(2, "a", "")], "").unwrap_err(),
        "picks"
    );
    assert_eq!(
        resolve_picks(&q, Kind::Answer, &[pick(0, "q", ""), pick(1, "a", "")], "").unwrap_err(),
        "picks"
    );
    assert_eq!(
        resolve_picks(&q, Kind::Answer, &[pick(1, "a", ""), pick(0, "a", "")], "").unwrap_err(),
        "picks"
    );
    assert_eq!(
        resolve_picks(
            &q,
            Kind::Answer,
            &[pick(0, "z", "mine"), pick(1, "a", "")],
            ""
        )
        .unwrap_err(),
        "picks"
    );

    // free_text: the last pick is the free one, with text
    let q2 = from_proto(pb::Question {
        decisions: vec![
            decision("First?", vec![option("a")]),
            decision("Second?", vec![option("a"), free_option("z")]),
        ],
        ..proto()
    })
    .unwrap();
    let ok = resolve_picks(
        &q2,
        Kind::FreeText,
        &[pick(0, "a", ""), pick(1, "z", "my words")],
        "",
    )
    .unwrap();
    assert_eq!(ok.picks[1].label, "my words");
    assert!(ok.picks[1].free);
    for bad in [
        vec![pick(0, "a", ""), pick(1, "z", "  ")],
        vec![pick(0, "a", ""), pick(1, "a", "")],
    ] {
        assert_eq!(
            resolve_picks(&q2, Kind::FreeText, &bad, "").unwrap_err(),
            "picks"
        );
    }

    // revise and reject: any picks in order, and a note
    let r = resolve_picks(&q, Kind::Revise, &[pick(1, "a", "")], "rethink").unwrap();
    assert_eq!(r.picks.len(), 1);
    assert_eq!(r.note, "rethink");
    assert_eq!(
        resolve_picks(&q, Kind::Reject, &[], "  ").unwrap_err(),
        "note"
    );
    assert!(resolve_picks(&q, Kind::Reject, &[], "no").is_ok());
    assert_eq!(
        resolve_picks(&q, Kind::Revise, &[pick(5, "a", "")], "n").unwrap_err(),
        "picks"
    );
}
