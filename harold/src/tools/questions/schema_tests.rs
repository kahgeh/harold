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
        title: String::new(),
        steps: vec![],
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
fn an_id_over_32_or_a_field_of_2000_is_refused() {
    let mut p = proto();
    p.id = "a".repeat(33);
    assert_eq!(from_proto(p).unwrap_err(), "id");
    let mut p = proto();
    p.id = "a".repeat(32);
    assert!(from_proto(p).is_ok());

    let mut p = proto();
    p.intro = "x".repeat(2000);
    assert_eq!(from_proto(p).unwrap_err(), "intro");
    let mut p = proto();
    p.intro = "x".repeat(1999);
    assert!(from_proto(p).is_ok());

    let mut p = proto();
    p.decisions[1].options[0].impact = "x".repeat(2000);
    assert_eq!(from_proto(p).unwrap_err(), "decisions.2.options.1.impact");

    let mut p = proto();
    p.decisions[0].ask = "x".repeat(2000);
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

    // free_text: one pick per decision, at least one free, each free pick with text
    let q2 = from_proto(pb::Question {
        decisions: vec![
            decision("First?", vec![option("a"), free_option("y")]),
            decision("Second?", vec![option("a"), free_option("z")]),
        ],
        ..proto()
    })
    .unwrap();
    let last = resolve_picks(
        &q2,
        Kind::FreeText,
        &[pick(0, "a", ""), pick(1, "z", "my words")],
        "",
    )
    .unwrap();
    assert_eq!(last.picks[1].label, "my words");
    assert!(last.picks[1].free);
    let early = resolve_picks(
        &q2,
        Kind::FreeText,
        &[pick(0, "y", "early words"), pick(1, "a", "")],
        "",
    )
    .unwrap();
    assert_eq!(early.picks[0].label, "early words");
    assert!(early.picks[0].free);
    assert!(!early.picks[1].free);
    for bad in [
        vec![pick(0, "a", ""), pick(1, "z", "  ")],
        vec![pick(0, "a", ""), pick(1, "a", "")],
        vec![pick(0, "y", "early words")],
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

fn step(text: &str, detail: &str) -> pb::Step {
    pb::Step {
        text: text.into(),
        detail: detail.into(),
    }
}

fn instructions_proto() -> pb::Question {
    pb::Question {
        id: "I1".into(),
        product: "harold".into(),
        spec: String::new(),
        sha: String::new(),
        link: String::new(),
        intro: "Do these".into(),
        decisions: vec![],
        title: "Set up the thing".into(),
        steps: vec![step("First", "detail one"), step("Second", "")],
    }
}

#[test]
fn instructions_from_proto_keep_title_and_steps() {
    let q = from_proto(instructions_proto()).unwrap();
    assert!(q.is_instructions());
    assert_eq!(q.title, "Set up the thing");
    assert_eq!(q.steps.len(), 2);
    assert_eq!(q.steps[0].text, "First");
    assert_eq!(q.steps[0].detail, "detail one");
    assert!(q.decisions.is_empty());
    assert!(!from_proto(proto()).unwrap().is_instructions());
}

#[test]
fn question_and_steps_together_are_refused() {
    let mut p = proto();
    p.steps = vec![step("First", "")];
    assert_eq!(from_proto(p).unwrap_err(), "steps");
    let mut p = instructions_proto();
    p.decisions = proto().decisions;
    assert_eq!(from_proto(p).unwrap_err(), "steps");
}

#[test]
fn neither_decisions_nor_steps_is_refused() {
    let mut p = instructions_proto();
    p.steps.clear();
    assert_eq!(from_proto(p).unwrap_err(), "decisions");
}

#[test]
fn instructions_need_a_title() {
    let mut p = instructions_proto();
    p.title = "  ".into();
    assert_eq!(from_proto(p).unwrap_err(), "title");
    let mut p = instructions_proto();
    p.title = "x".repeat(2000);
    assert_eq!(from_proto(p).unwrap_err(), "title");
}

#[test]
fn instructions_accept_empty_spec_sha_link_but_a_set_link_must_be_http() {
    assert!(from_proto(instructions_proto()).is_ok());
    let mut p = instructions_proto();
    p.link = "https://claude.ai/x".into();
    p.spec = "s".into();
    p.sha = "abc".into();
    assert!(from_proto(p).is_ok());
    let mut p = instructions_proto();
    p.link = "ftp://x".into();
    assert_eq!(from_proto(p).unwrap_err(), "link");
}

#[test]
fn step_rules() {
    let mut p = instructions_proto();
    p.steps = (0..20).map(|i| step(&format!("s{i}"), "")).collect();
    assert!(from_proto(p).is_ok());
    let mut p = instructions_proto();
    p.steps = (0..21).map(|i| step(&format!("s{i}"), "")).collect();
    assert_eq!(from_proto(p).unwrap_err(), "steps");
    let mut p = instructions_proto();
    p.steps[1].text = "  ".into();
    assert_eq!(from_proto(p).unwrap_err(), "steps.2.text");
    let mut p = instructions_proto();
    p.steps[0].text = "x".repeat(2000);
    assert_eq!(from_proto(p).unwrap_err(), "steps.1.text");
    let mut p = instructions_proto();
    p.steps[0].detail = "x".repeat(2000);
    assert_eq!(from_proto(p).unwrap_err(), "steps.1.detail");
    let mut p = instructions_proto();
    p.intro = "x".repeat(2000);
    assert_eq!(from_proto(p).unwrap_err(), "intro");
}

#[test]
fn to_proto_round_trips_instructions() {
    let q = from_proto(instructions_proto()).unwrap();
    assert_eq!(to_proto(&q), instructions_proto());
}

#[test]
fn a_stored_old_body_without_steps_loads() {
    let body = r#"{"id":"Q1","product":"p","spec":"s","sha":"a","link":"https://x","intro":"","decisions":[{"ask":"A?","options":[{"key":"a","label":"A","impact":"","suggested":false,"free":false}]}]}"#;
    let q: Question = serde_json::from_str(body).unwrap();
    assert_eq!(q.title, "");
    assert!(q.steps.is_empty());
    assert!(!q.is_instructions());
    let answer: Answer = serde_json::from_str(r#"{"kind":"Answer","picks":[],"note":""}"#).unwrap();
    assert_eq!(answer.ticked, 0);
}

#[test]
fn resolve_done_and_ignore_for_instructions() {
    let q = from_proto(instructions_proto()).unwrap();
    let done = resolve(&q, Kind::Done, &[], "").unwrap();
    assert_eq!(done.kind, Kind::Done);
    assert!(done.picks.is_empty());
    assert_eq!(
        resolve_instructions(&q, Kind::Ignore, &[], "")
            .unwrap()
            .kind,
        Kind::Ignore
    );
}

#[test]
fn resolve_refuses_done_for_a_question_and_answer_for_instructions() {
    let q = from_proto(proto()).unwrap();
    assert_eq!(resolve(&q, Kind::Done, &[], "").unwrap_err(), "kind");
    assert_eq!(resolve(&q, Kind::Ignore, &[], "").unwrap_err(), "kind");
    assert_eq!(resolve_picks(&q, Kind::Done, &[], "").unwrap_err(), "kind");
    let good = [pick(0, "a", ""), pick(1, "b", "")];
    assert!(resolve(&q, Kind::Answer, &good, "").is_ok());
    let i = from_proto(instructions_proto()).unwrap();
    for kind in [Kind::Answer, Kind::Revise, Kind::Reject, Kind::FreeText] {
        assert_eq!(resolve(&i, kind, &[], "n").unwrap_err(), "kind");
    }
}

#[test]
fn done_with_picks_or_note_is_refused() {
    let i = from_proto(instructions_proto()).unwrap();
    assert_eq!(
        resolve(&i, Kind::Done, &[pick(0, "a", "")], "").unwrap_err(),
        "picks"
    );
    assert_eq!(resolve(&i, Kind::Ignore, &[], "why").unwrap_err(), "note");
}
