//! Tests of the state rule, on fake results and a fake clock.

use super::state::{QueryResult, Release, State, state_of};

fn t(text: &str) -> time::OffsetDateTime {
    time::OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339).unwrap()
}

fn result(present: bool) -> QueryResult {
    QueryResult {
        name: "q-1".to_string(),
        present,
        reason: "r".to_string(),
    }
}

fn since() -> time::OffsetDateTime {
    t("2026-11-12T03:05:00Z")
}

fn tag() -> time::OffsetDateTime {
    t("2026-11-13T10:00:00Z")
}

#[test]
fn all_present_gives_present() {
    let results = [result(true), result(true)];
    let now = t("2026-11-14T00:00:00Z");
    assert_eq!(
        state_of(&results, since(), Release::Tagged(tag()), now),
        State::Present
    );
}

#[test]
fn one_absent_before_seven_days_gives_waiting() {
    let results = [result(true), result(false)];
    let now = t("2026-11-20T09:59:00Z");
    assert_eq!(
        state_of(&results, since(), Release::Tagged(tag()), now),
        State::Waiting
    );
}

#[test]
fn one_absent_after_seven_days_gives_missing() {
    let results = [result(true), result(false)];
    let now = t("2026-11-20T10:00:00Z");
    assert_eq!(
        state_of(&results, since(), Release::Tagged(tag()), now),
        State::Missing
    );
}

#[test]
fn no_tag_on_the_merge_gives_waiting_however_late() {
    let results = [result(false)];
    let now = t("2027-03-01T00:00:00Z");
    assert_eq!(
        state_of(&results, since(), Release::NotYet, now),
        State::Waiting
    );
}

#[test]
fn a_product_with_no_tag_counts_from_the_merge() {
    let results = [result(false)];
    assert_eq!(
        state_of(
            &results,
            since(),
            Release::NoTags,
            t("2026-11-19T03:04:00Z")
        ),
        State::Waiting
    );
    assert_eq!(
        state_of(
            &results,
            since(),
            Release::NoTags,
            t("2026-11-19T03:05:00Z")
        ),
        State::Missing
    );
}

#[test]
fn no_results_give_waiting() {
    let now = t("2027-03-01T00:00:00Z");
    assert_eq!(
        state_of(&[], since(), Release::Tagged(tag()), now),
        State::Waiting
    );
}

#[test]
fn a_present_result_after_the_window_still_gives_present() {
    let now = t("2027-03-01T00:00:00Z");
    assert_eq!(
        state_of(&[result(true)], since(), Release::Tagged(tag()), now),
        State::Present
    );
}
