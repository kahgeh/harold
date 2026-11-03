use super::*;

#[test]
fn names_of_48_pass_49_fail() {
    assert!(valid_name(&"a".repeat(48)));
    assert!(!valid_name(&"a".repeat(49)));
    assert!(valid_name("a"));
    assert!(valid_name("work-records-2"));
    assert!(!valid_name(""));
}

#[test]
fn names_refuse_upper_case_and_spaces() {
    for bad in ["Work", "a b", "a_b", "a/b", "é", "a.b"] {
        assert!(!valid_name(bad), "{bad}");
    }
}

#[test]
fn status_order_and_open() {
    let words = [
        "idea",
        "discovery",
        "spec drafted",
        "architect checked",
        "waiting for the user",
        "approved",
        "building",
        "merged",
        "released",
        "checked by the user",
    ];
    for (i, word) in words.iter().enumerate() {
        let status = Status::parse(word).unwrap();
        assert_eq!(status.ordinal() as usize, i);
        assert_eq!(status.as_str(), *word);
        assert_eq!(status.is_open(), i < 9, "{word}");
    }
    assert_eq!(Status::parse("done"), None);
    assert!(Priority::Focus.ordinal() < Priority::Next.ordinal());
    assert!(Priority::Next.ordinal() < Priority::Later.ordinal());
    assert_eq!(Mark::parse("draft"), Some(Mark::Draft));
    assert_eq!(Mark::parse("x"), None);
}

#[test]
fn link_forms() {
    for good in [
        "spec:harold/work-records",
        "question:Q12",
        "question:I3",
        "note:any text, with spaces",
    ] {
        assert!(valid_link(good), "{good}");
    }
    for bad in [
        "",
        "spec:harold",
        "spec:/x",
        "spec:harold/",
        "spec:a/b/c",
        "spec:a b/c",
        "question:",
        "question:a b",
        "note:",
        "note:  ",
        "note:a\nb",
        "http://x",
    ] {
        assert!(!valid_link(bad), "{bad}");
    }
}
