//! The state of a spec's evidence: the rule that turns query results and the release time
//! into `waiting`, `present` or `missing`. This file reads nothing and writes nothing.

use time::{Duration, OffsetDateTime};

/// The days after the release in which a missing signal may still arrive.
pub(crate) const WAIT_DAYS: i64 = 7;

/// The state of the evidence of one spec.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum State {
    Waiting,
    Present,
    Missing,
}

impl State {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Present => "present",
            Self::Missing => "missing",
        }
    }

    pub(crate) fn parse(word: &str) -> Option<Self> {
        [Self::Waiting, Self::Present, Self::Missing]
            .into_iter()
            .find(|state| state.as_str() == word)
    }

    /// A final state is not checked again by the pass.
    pub(crate) fn is_final(self) -> bool {
        self != Self::Waiting
    }
}

/// The result of one evidence query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct QueryResult {
    pub name: String,
    pub present: bool,
    pub reason: String,
}

/// How the release of a spec stands. The 7 days start with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Release {
    /// The first `v*` tag that holds the merge commit has this time.
    Tagged(OffsetDateTime),
    /// The product has tags, and none holds the merge commit yet.
    NotYet,
    /// The product has no `v*` tag at all. The 7 days start at the merge.
    NoTags,
}

/// The state: `present` when every result is present; `missing` when one is absent 7 days
/// after the release; else `waiting`. No result gives `waiting`.
pub(crate) fn state_of(
    results: &[QueryResult],
    since: OffsetDateTime,
    release: Release,
    now: OffsetDateTime,
) -> State {
    if results.is_empty() {
        return State::Waiting;
    }
    if results.iter().all(|result| result.present) {
        return State::Present;
    }
    let start = match release {
        Release::Tagged(time) => time,
        Release::NoTags => since,
        Release::NotYet => return State::Waiting,
    };
    if now >= start + Duration::days(WAIT_DAYS) {
        State::Missing
    } else {
        State::Waiting
    }
}
