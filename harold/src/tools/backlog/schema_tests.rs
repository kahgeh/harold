use super::*;
use crate::tools::backlog::test_support::item;

fn chars(c: char, n: usize) -> String {
    std::iter::repeat_n(c, n).collect()
}

#[test]
fn a_valid_item_passes() {
    assert_eq!(validate_item(&item("a-1")), Ok(()));
}

#[test]
fn an_item_with_no_evidence_is_invalid_evidence() {
    let mut it = item("a");
    it.evidence.clear();
    assert_eq!(validate_item(&it), Err("evidence".into()));
}

#[test]
fn a_21st_pointer_is_invalid_evidence() {
    let mut it = item("a");
    it.evidence = vec!["harold.log:1".into(); EVIDENCE_MAX];
    assert_eq!(validate_item(&it), Ok(()));
    it.evidence.push("harold.log:2".into());
    assert_eq!(validate_item(&it), Err("evidence".into()));
}

#[test]
fn a_500_character_pointer_is_invalid_evidence() {
    let mut it = item("a");
    it.evidence = vec![chars('x', POINTER_UNDER - 1)];
    assert_eq!(validate_item(&it), Ok(()));
    it.evidence = vec![chars('x', POINTER_UNDER)];
    assert_eq!(validate_item(&it), Err("evidence".into()));
}

#[test]
fn each_text_field_at_2000_characters_names_itself() {
    type Field = fn(&mut Item) -> &mut String;
    let fields: [(&str, Field); 6] = [
        ("value", |it| &mut it.value),
        ("risk", |it| &mut it.risk),
        ("reason", |it| &mut it.reason),
        ("paths", |it| &mut it.paths),
        ("after", |it| &mut it.after),
        ("title", |it| &mut it.title),
    ];
    for (name, field) in fields {
        let mut it = item("a");
        *field(&mut it) = chars('x', FIELD_UNDER - 1);
        assert_eq!(validate_item(&it), Ok(()), "{name} at 1,999");
        *field(&mut it) = chars('x', FIELD_UNDER);
        assert_eq!(validate_item(&it), Err(name.to_string()), "{name} at 2,000");
    }
}

#[test]
fn limits_count_characters_not_bytes() {
    let mut it = item("a");
    it.risk = chars('é', FIELD_UNDER - 1);
    assert_eq!(validate_item(&it), Ok(()));
    it.risk = chars('é', FIELD_UNDER);
    assert_eq!(validate_item(&it), Err("risk".into()));
}

#[test]
fn ids_are_letters_digits_and_dashes_up_to_48() {
    assert_eq!(validate_item(&item("a-1")), Ok(()));
    assert_eq!(validate_item(&item(&chars('a', 48))), Ok(()));
    for id in [chars('a', 49).as_str(), "a b", "a_b", ""] {
        assert_eq!(validate_item(&item(id)), Err("id".into()), "{id:?}");
    }
}

#[test]
fn a_title_is_one_line() {
    let mut it = item("a");
    it.title = "a\nb".into();
    assert_eq!(validate_item(&it), Err("title".into()));
    it.title = String::new();
    assert_eq!(validate_item(&it), Err("title".into()));
}

#[test]
fn size_is_empty_or_one_of_three() {
    for size in ["", "small", "medium", "large"] {
        let mut it = item("a");
        it.size = size.into();
        assert_eq!(validate_item(&it), Ok(()), "{size:?}");
    }
    let mut it = item("a");
    it.size = "huge".into();
    assert_eq!(validate_item(&it), Err("size".into()));
}

#[test]
fn a_note_body_is_under_20000() {
    assert_eq!(validate_note("t", &chars('x', BODY_UNDER - 1)), Ok(()));
    assert_eq!(
        validate_note("t", &chars('x', BODY_UNDER)),
        Err("body".into())
    );
    assert_eq!(validate_note("", "b"), Err("title".into()));
    assert_eq!(validate_note("a\nb", "b"), Err("title".into()));
}

#[test]
fn queue_text_is_under_4000() {
    assert_eq!(
        validate_queue_text(&chars('x', QUEUE_TEXT_UNDER - 1)),
        Ok(())
    );
    assert_eq!(
        validate_queue_text(&chars('x', QUEUE_TEXT_UNDER)),
        Err("text".into())
    );
}

#[test]
fn note_kinds_parse() {
    for kind in [
        NoteKind::Rule,
        NoteKind::History,
        NoteKind::Held,
        NoteKind::LeftOut,
    ] {
        assert_eq!(NoteKind::parse(kind.as_str()), Some(kind));
    }
    assert_eq!(NoteKind::LeftOut.as_str(), "left_out");
    assert_eq!(NoteKind::parse("left_out"), Some(NoteKind::LeftOut));
    assert_eq!(NoteKind::parse("other"), None);
}
