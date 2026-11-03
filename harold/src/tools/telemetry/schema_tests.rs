use super::*;
use crate::tools::telemetry::name::{ValueList, parse_attr};

fn list(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

fn declaration(kind: Kind, meaning: &str, attrs: Vec<Attr>) -> Declaration {
    Declaration {
        name: "harold.release.attempts".into(),
        kind,
        meaning: meaning.into(),
        attrs,
        created_ms: 1,
        last_values: LastValues::new(),
    }
}

#[test]
fn a_good_declaration_passes() {
    let attrs = vec![parse_attr("result", &list(&["ok", "failed"])).unwrap()];
    assert_eq!(
        declaration(Kind::Counter, "Attempts", attrs).validate(),
        Ok(())
    );
}

#[test]
fn a_meaning_over_80_characters_is_refused() {
    let ok = declaration(Kind::Counter, &"é".repeat(80), vec![]);
    assert_eq!(ok.validate(), Ok(()));
    let long = declaration(Kind::Counter, &"é".repeat(95), vec![]);
    assert_eq!(long.validate(), Err(Refused::MeaningTooLong { length: 95 }));
}

#[test]
fn over_100_series_is_refused_for_a_counter_and_a_gauge_only() {
    let attrs = vec![
        parse_attr("result", &list(&["ok", "failed"])).unwrap(),
        parse_attr("step", &list(&["1-60"])).unwrap(),
    ];
    let expect = Err(Refused::TooManySeries {
        name: "harold.release.attempts".into(),
        series: 120,
    });
    assert_eq!(
        declaration(Kind::Counter, "", attrs.clone()).validate(),
        expect
    );
    assert_eq!(
        declaration(Kind::Gauge, "", attrs.clone()).validate(),
        expect
    );
    assert_eq!(declaration(Kind::Event, "", attrs).validate(), Ok(()));
}

#[test]
fn a_huge_range_is_too_many_series() {
    let attrs = vec![parse_attr("n", &list(&["0-1000000"])).unwrap()];
    let refused = declaration(Kind::Counter, "", attrs)
        .validate()
        .unwrap_err();
    assert_eq!(refused.reason(), "too_many_series");
}

#[test]
fn four_keys_are_invalid() {
    let attrs = ["a", "b", "c", "d"]
        .iter()
        .map(|k| parse_attr(k, &list(&["x"])).unwrap())
        .collect();
    let refused = declaration(Kind::Event, "", attrs).validate().unwrap_err();
    assert_eq!(refused.reason(), "invalid");
}

#[test]
fn a_bad_name_is_invalid() {
    let mut bad = declaration(Kind::Counter, "", vec![]);
    bad.name = "Harold.x".into();
    assert_eq!(bad.validate().unwrap_err().reason(), "invalid");
}

#[test]
fn same_shape_ignores_meaning_and_order() {
    let a = vec![
        parse_attr("result", &list(&["ok", "failed"])).unwrap(),
        parse_attr("step", &list(&["1-5"])).unwrap(),
    ];
    let b = vec![
        parse_attr("step", &list(&["1-5"])).unwrap(),
        parse_attr("result", &list(&["failed", "ok"])).unwrap(),
    ];
    let one = declaration(Kind::Counter, "one", a.clone());
    assert!(one.same_shape(&declaration(Kind::Counter, "other", b)));
    assert!(!one.same_shape(&declaration(Kind::Gauge, "one", a.clone())));
    let mut changed = a;
    changed[1].values = ValueList::Range { lo: 1, hi: 6 };
    assert!(!one.same_shape(&declaration(Kind::Counter, "one", changed)));
}
