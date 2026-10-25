use super::*;

fn put_relation(from: &str, relation: &str, to: &str) -> Op {
    Op::PutRelation {
        from: from.into(),
        relation: relation.into(),
        to: to.into(),
    }
}

#[test]
fn a_record_is_json_with_its_op_and_without_vault_note_text() {
    let record = DataRecord {
        op: put_relation("a", "part_of", "b"),
        at_ms: 5,
        by: By::Call,
    };
    let json = serde_json::to_string(&record).unwrap();
    assert_eq!(
        json,
        r#"{"op":"put_relation","from":"a","relation":"part_of","to":"b","at_ms":5,"by":"call"}"#
    );
    assert_eq!(serde_json::from_str::<DataRecord>(&json).unwrap(), record);
}

#[test]
fn a_void_record_holds_the_sequence_and_the_reason() {
    let record = DataRecord {
        op: Op::VoidRecord {
            sequence: 7,
            reason: "bug".into(),
        },
        at_ms: 1,
        by: By::Call,
    };
    let json = serde_json::to_string(&record).unwrap();
    assert!(json.contains(r#""op":"void_record""#) && json.contains(r#""sequence":7"#));
}

#[test]
fn an_unknown_relation_type_or_a_self_relation_is_malformed() {
    assert_eq!(
        put_relation("a", "likes", "b").well_formed(),
        Err(Malformed::Relation)
    );
    assert_eq!(
        put_relation("a", "part_of", "a").well_formed(),
        Err(Malformed::Relation)
    );
    assert_eq!(put_relation("a", "part_of", "b").well_formed(), Ok(()));
}

#[test]
fn a_name_must_be_a_plain_relative_name() {
    for bad in ["", "/etc/x", "a/../b", "a.md", "a\nb"] {
        assert_eq!(
            Op::RemoveVaultNote { name: bad.into() }.well_formed(),
            Err(Malformed::VaultNote),
            "{bad:?}"
        );
    }
    assert_eq!(
        Op::RemoveVaultNote {
            name: "lessons/a-note".into()
        }
        .well_formed(),
        Ok(())
    );
}

#[test]
fn a_void_reason_is_one_word() {
    let void = |reason: &str| Op::VoidRecord {
        sequence: 1,
        reason: reason.into(),
    };
    assert_eq!(void("bad record").well_formed(), Err(Malformed::Reason));
    assert_eq!(void("").well_formed(), Err(Malformed::Reason));
    assert_eq!(void("typo").well_formed(), Ok(()));
}
