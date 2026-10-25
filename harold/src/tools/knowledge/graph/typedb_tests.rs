use super::super::records::Op;
use super::*;

fn texts(op: &Op) -> Vec<String> {
    statements(op)
        .unwrap()
        .into_iter()
        .map(|s| s.text)
        .collect()
}

#[test]
fn a_string_literal_escapes_quotes_backslashes_and_line_ends() {
    assert_eq!(quote("plain"), "\"plain\"");
    assert_eq!(quote("a\"b\\c\nd\te"), "\"a\\\"b\\\\c\\nd\\te\"");
    assert_eq!(quote("bell\u{7}"), "\"bell \"");
}

#[test]
fn a_name_cannot_close_the_literal_and_add_a_statement() {
    let text = texts(&Op::RemoveVaultNote {
        name: "x\"; delete $n; match $m isa vault_note; \"".into(),
    });
    assert!(text[1].contains("\\\"; delete $n; match"), "{}", text[1]);
    assert_eq!(text[1].matches("delete $n").count(), 2);
}

#[test]
fn putting_a_note_replaces_its_title_kind_and_missing_mark() {
    let text = texts(&Op::PutVaultNote {
        name: "a".into(),
        title: "A".into(),
        kind: "lesson".into(),
    });
    assert_eq!(text.len(), 5);
    assert!(text[0].starts_with("put $n isa vault_note, has path \"a\""));
    assert!(text[4].contains("has missing false"));
}

#[test]
fn a_link_and_a_relation_must_find_both_vault_notes() {
    let link = statements(&Op::PutLink {
        from: "a".into(),
        to: "b".into(),
    })
    .unwrap();
    assert!(link[0].must_match);
    assert!(link[0].text.contains("note_link"));
    let relation = statements(&Op::PutRelation {
        from: "a".into(),
        relation: "supersedes".into(),
        to: "b".into(),
    })
    .unwrap();
    assert!(relation[0].must_match);
    assert!(relation[0].text.contains("isa supersedes"));
}

#[test]
fn an_unknown_relation_type_is_refused_before_any_query() {
    let result = statements(&Op::PutRelation {
        from: "a".into(),
        relation: "friends; delete $n".into(),
        to: "b".into(),
    });
    assert!(matches!(result, Err(BackendError::Refused(word)) if word == "invalid_relation"));
}

#[test]
fn removing_a_note_first_removes_the_links_that_name_it() {
    let text = texts(&Op::RemoveVaultNote { name: "a".into() });
    assert!(text[0].contains("note_link"));
    assert!(text[1].ends_with("delete $n;"));
}

#[test]
fn a_position_move_replaces_one_sequence_and_keeps_the_other() {
    let data = position_statements(Which::Data, 42);
    assert!(data.iter().all(|s| !s.contains("schema_sequence")));
    assert!(data[2].contains("insert $g has data_sequence 42"));
    let schema = position_statements(Which::Schema, 7);
    assert!(schema.iter().all(|s| !s.contains("data_sequence")));
}

#[test]
fn a_void_record_changes_nothing_but_the_position() {
    assert!(
        statements(&Op::VoidRecord {
            sequence: 3,
            reason: "bad".into()
        })
        .unwrap()
        .is_empty()
    );
}

#[test]
fn a_typedb_error_body_gives_a_one_word_reason() {
    assert_eq!(
        reason(&serde_json::json!({"code": "CNT9", "message": "x"})),
        "cnt9"
    );
    assert_eq!(reason(&serde_json::Value::Null), "server_error");
}

#[test]
fn only_a_coded_400_is_a_refusal_and_every_other_failure_is_the_servers() {
    let reply = |status: u16, body: serde_json::Value| Reply { status, body };
    let coded = serde_json::json!({"code": "CNT9"});
    assert_eq!(
        refusal(&reply(400, coded.clone())),
        BackendError::Refused("cnt9".into())
    );
    for status in [401, 404, 500, 503] {
        assert_eq!(
            refusal(&reply(status, coded.clone())),
            BackendError::Unreachable,
            "{status}"
        );
    }
    assert_eq!(
        refusal(&reply(400, serde_json::Value::Null)),
        BackendError::Unreachable
    );
}
