use super::*;

fn list(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

fn attr(key: &str, items: &[&str]) -> Attr {
    parse_attr(key, &list(items)).unwrap()
}

#[test]
fn a_good_name_has_its_prefix() {
    let name = parse_name("harold.release.attempts").unwrap();
    assert_eq!(name.full, "harold.release.attempts");
    assert_eq!(name.prefix, "harold");
    assert_eq!(parse_name("chief.a_1").unwrap().prefix, "chief");
}

#[test]
fn a_bad_name_is_refused() {
    for raw in [
        "Harold.x", ".x", "a..b", "a.", "", "abc", "a.b-c", "a.b c", "a.é",
    ] {
        assert!(
            matches!(parse_name(raw), Err(Refused::Invalid(_))),
            "{raw:?}"
        );
    }
}

#[test]
fn a_name_is_at_most_64_characters() {
    let name = |n: usize| format!("a.{}", "b".repeat(n - 2));
    assert!(parse_name(&name(64)).is_ok());
    assert!(matches!(parse_name(&name(65)), Err(Refused::Invalid(_))));
}

#[test]
fn a_range_is_read_and_never_expanded() {
    assert_eq!(
        parse_values(&list(&["0-50"])).unwrap(),
        ValueList::Range { lo: 0, hi: 50 }
    );
    let wide = parse_values(&list(&["0-4294967295"])).unwrap();
    assert_eq!(wide.count(), 4_294_967_296);
    let beyond = parse_values(&list(&["0-99999999999999999999"])).unwrap();
    assert_eq!(
        beyond,
        ValueList::Range {
            lo: 0,
            hi: u32::MAX
        }
    );
}

#[test]
fn a_bad_range_is_refused() {
    for raw in ["5-2", "0-", "-3", "1-2-3", "0-x"] {
        assert!(
            matches!(parse_values(&list(&[raw])), Err(Refused::Invalid(_))),
            "{raw}"
        );
    }
}

#[test]
fn a_value_list_has_plain_distinct_values() {
    assert_eq!(
        parse_values(&list(&["ok", "failed"])).unwrap(),
        ValueList::Values(list(&["ok", "failed"]))
    );
    assert!(parse_values(&[]).is_err());
    assert!(parse_values(&list(&["ok", "ok"])).is_err());
    assert!(parse_values(&list(&["Ok"])).is_err());
    assert!(parse_values(&list(&[""])).is_err());
    // A range among other values is a plain value, and `0-5` is not plain here.
    assert_eq!(
        parse_values(&list(&["a-b"])).unwrap(),
        ValueList::Values(list(&["a-b"]))
    );
}

#[test]
fn a_range_contains_whole_numbers_in_plain_form() {
    let range = ValueList::Range { lo: 0, hi: 50 };
    assert!(range.contains("0") && range.contains("50"));
    assert!(!range.contains("51") && !range.contains("007") && !range.contains("-1"));
    assert!(!range.contains("x"));
}

#[test]
fn a_list_is_described_as_a_hint() {
    assert_eq!(
        ValueList::Values(list(&["ok", "failed"])).describe(),
        "ok or failed"
    );
    assert_eq!(
        ValueList::Values(list(&["a", "b", "c"])).describe(),
        "a, b or c"
    );
    assert_eq!(ValueList::Range { lo: 0, hi: 50 }.describe(), "0-50");
}

#[test]
fn series_count_is_the_product() {
    let attrs = vec![attr("result", &["ok", "failed"]), attr("step", &["1-60"])];
    assert_eq!(series_count(&attrs), 120);
    assert_eq!(series_count(&[]), 1);
}

#[test]
fn series_count_saturates() {
    let wide = attr("a", &["0-4294967295"]);
    let attrs = vec![wide.clone(), wide.clone(), wide];
    assert_eq!(series_count(&attrs), u64::MAX);
}

#[test]
fn keys_are_checked() {
    assert!(parse_attr("Bad", &list(&["x"])).is_err());
    assert!(parse_attr("", &list(&["x"])).is_err());
    let one = attr("a", &["x"]);
    assert!(check_keys(&[one.clone(), one.clone()]).is_err());
    let four: Vec<Attr> = ["a", "b", "c", "d"]
        .iter()
        .map(|k| attr(k, &["x"]))
        .collect();
    assert!(check_keys(&four[..3]).is_ok());
    assert!(check_keys(&four).is_err());
}

#[test]
fn the_texts_are_the_spec_texts() {
    let p = |s: &str| s.to_string();
    let cases = [
        (
            Refused::NoPane,
            "no_pane",
            "run this from a pane: no TMUX_PANE or REX_BLOCK",
        ),
        (
            Refused::WrongPrefix {
                prefix: p("harold"),
            },
            "wrong_prefix",
            "harold. is for panes in harold's folder and the chief",
        ),
        (
            Refused::WrongPrefix { prefix: p("chief") },
            "wrong_prefix",
            "chief. is for the chief's pane",
        ),
        (
            Refused::UnknownPrefix { prefix: p("web") },
            "unknown_prefix",
            "unknown prefix web.: use chief. or a product name",
        ),
        (
            Refused::TooManyNames {
                prefix: p("harold"),
            },
            "too_many_names",
            "harold. has 50 names: remove one first",
        ),
        (
            Refused::TooManyEvents {
                prefix: p("harold"),
            },
            "too_many_names",
            "harold. has 20 events: remove one first",
        ),
        (
            Refused::TooManySeries {
                name: p("harold.release.attempts"),
                series: 120,
            },
            "too_many_series",
            "harold.release.attempts would have 120 series: the most is 100",
        ),
        (
            Refused::UndeclaredValue {
                key: p("result"),
                value: p("maybe"),
                allowed: p("ok or failed"),
            },
            "undeclared_value",
            "result=maybe is not declared: use ok or failed",
        ),
        (
            Refused::WrongKind {
                name: p("harold.release.attempts"),
                declared: Kind::Counter,
                asked: Kind::Gauge,
            },
            "wrong_kind",
            "harold.release.attempts is a counter, not a gauge",
        ),
        (
            Refused::Undeclared {
                name: p("harold.release.attempts"),
            },
            "undeclared",
            "harold.release.attempts is not declared: declare it first",
        ),
        (
            Refused::RateLimited {
                prefix: p("harold"),
            },
            "rate_limited",
            "harold. wrote 60 times this minute: wait",
        ),
        (
            Refused::MeaningTooLong { length: 95 },
            "meaning_too_long",
            "the meaning has 95 characters: the most is 80",
        ),
        (
            Refused::DeclarationDiffers {
                name: p("harold.release.attempts"),
            },
            "declaration_differs",
            "harold.release.attempts is declared with other values: remove it and declare it again",
        ),
    ];
    for (refused, reason, text) in cases {
        assert_eq!(refused.reason(), reason);
        assert_eq!(refused.text(), text);
    }
    assert_eq!(Refused::Invalid("x").reason(), "invalid");
}
