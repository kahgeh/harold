use time::{Date, Month, Time};

use super::*;

/// Midnight UTC of a day.
fn day(year: i32, month: u8, day: u8) -> OffsetDateTime {
    let month = Month::try_from(month).expect("a valid month");
    let date = Date::from_calendar_date(year, month, day).expect("a valid date");
    date.with_time(Time::MIDNIGHT).assume_utc()
}

fn now() -> OffsetDateTime {
    day(2026, 11, 3)
}

fn tag_time() -> OffsetDateTime {
    day(2026, 10, 20)
}

fn spec(id: &str, state: SpecState) -> SpecFacts {
    SpecFacts {
        id: id.to_string(),
        state,
        ..Default::default()
    }
}

fn completed(id: &str, commit: Option<&str>, merged_at: OffsetDateTime) -> SpecFacts {
    SpecFacts {
        merged_commit: commit.map(str::to_string),
        merged_at: Some(merged_at),
        ..spec(id, SpecState::Completed)
    }
}

/// Owns the sets that `Facts` borrows.
#[derive(Default)]
struct Fixture {
    specs: Vec<SpecFacts>,
    titles: HashMap<String, String>,
    parked: HashSet<String>,
    branch_commits: HashMap<String, Branch>,
    in_release: HashSet<String>,
    release: Option<ReleaseTag>,
    first_merged_file: Option<OffsetDateTime>,
    awaiting: HashSet<String>,
    other_commits: u32,
}

impl Fixture {
    fn with_release() -> Self {
        Self {
            release: Some(ReleaseTag {
                name: "v1.0.0".to_string(),
                commit_time: tag_time(),
            }),
            ..Default::default()
        }
    }

    fn run(&self) -> ProductNext {
        classify(
            "demo",
            &Facts {
                specs: &self.specs,
                titles: &self.titles,
                parked: &self.parked,
                branch_commits: &self.branch_commits,
                in_release: &self.in_release,
                release: self.release.clone(),
                first_merged_file: self.first_merged_file,
                awaiting: &self.awaiting,
                now: now(),
                other_commits: self.other_commits,
            },
        )
    }
}

fn ids(lines: &[Line]) -> Vec<&str> {
    lines.iter().map(|line| line.id.as_str()).collect()
}

fn set(ids: &[&str]) -> HashSet<String> {
    ids.iter().map(|id| id.to_string()).collect()
}

#[test]
fn commit_in_the_tag_is_not_merged_since_the_release() {
    let mut fixture = Fixture::with_release();
    fixture.specs = vec![completed("old", Some("aaa"), day(2026, 10, 10))];
    fixture.in_release = set(&["aaa"]);
    let next = fixture.run();
    assert!(next.merged.is_empty());
    assert_eq!(next.release, "v1.0.0");
}

#[test]
fn commit_after_the_tag_is_listed() {
    let mut fixture = Fixture::with_release();
    fixture.specs = vec![completed("new", Some("bbb"), day(2026, 10, 25))];
    fixture
        .titles
        .insert("new".to_string(), "A new thing".to_string());
    let next = fixture.run();
    assert_eq!(
        next.merged,
        vec![Line {
            id: "new".to_string(),
            title: "A new thing".to_string(),
            commits: None,
        }]
    );
}

#[test]
fn no_commit_uses_merge_time_against_the_tag_time() {
    let mut fixture = Fixture::with_release();
    fixture.specs = vec![
        completed("before", None, day(2026, 10, 19)),
        completed("after", None, day(2026, 10, 21)),
    ];
    assert_eq!(ids(&fixture.run().merged), vec!["after"]);
}

#[test]
fn no_release_lists_the_last_14_days_only() {
    let fixture = Fixture {
        specs: vec![
            completed("recent", Some("ccc"), day(2026, 10, 25)),
            completed("stale", Some("ddd"), day(2026, 10, 1)),
        ],
        ..Default::default()
    };
    let next = fixture.run();
    assert_eq!(ids(&next.merged), vec!["recent"]);
    assert_eq!(next.release, "");
}

#[test]
fn in_progress_shows_the_branch_commits() {
    let mut fixture = Fixture::with_release();
    fixture.specs = vec![spec("building", SpecState::InProgress)];
    fixture
        .branch_commits
        .insert("building".to_string(), Branch::Commits(4));
    let next = fixture.run();
    assert_eq!(next.in_progress.len(), 1);
    assert_eq!(next.in_progress[0].commits, Some(4));
    assert!(next.not_clear.is_empty());
}

#[test]
fn started_spec_with_no_branch_is_not_clear() {
    let mut fixture = Fixture::with_release();
    fixture.specs = vec![
        spec("lost", SpecState::InProgress),
        spec("unknown-branch", SpecState::InProgress),
    ];
    fixture
        .branch_commits
        .insert("lost".to_string(), Branch::None);
    let next = fixture.run();
    assert_eq!(ids(&next.not_clear), vec!["lost", "unknown-branch"]);
    assert!(next.in_progress.is_empty());
}

#[test]
fn parked_spec_is_in_no_other_list() {
    let mut fixture = Fixture::with_release();
    fixture.specs = vec![
        spec("p-progress", SpecState::InProgress),
        spec("p-none", SpecState::InProgress),
        SpecFacts {
            approved_at: Some(now()),
            ..spec("p-approved", SpecState::Approved)
        },
        spec("p-open", SpecState::Open),
    ];
    fixture
        .branch_commits
        .insert("p-progress".to_string(), Branch::Commits(2));
    fixture.awaiting = set(&["p-open"]);
    fixture.parked = set(&["p-progress", "p-none", "p-approved", "p-open", "p-gone"]);
    let next = fixture.run();
    assert_eq!(
        ids(&next.parked),
        vec!["p-approved", "p-gone", "p-none", "p-open", "p-progress"]
    );
    assert!(next.in_progress.is_empty());
    assert!(next.not_clear.is_empty());
    assert!(next.approved.is_empty());
    assert!(next.awaiting.is_empty());
}

#[test]
fn approval_before_the_first_merged_file_is_history() {
    let mut fixture = Fixture::with_release();
    fixture.first_merged_file = Some(day(2026, 10, 26));
    fixture.specs = vec![
        SpecFacts {
            approved_at: Some(day(2026, 10, 20)),
            ..spec("history", SpecState::Approved)
        },
        SpecFacts {
            approved_at: Some(day(2026, 10, 26)),
            ..spec("current", SpecState::Approved)
        },
    ];
    assert_eq!(ids(&fixture.run().approved), vec!["current"]);
    fixture.first_merged_file = None;
    assert_eq!(ids(&fixture.run().approved), vec!["current", "history"]);
}

#[test]
fn open_spec_without_the_waiting_line_is_not_awaiting() {
    let mut fixture = Fixture::with_release();
    fixture.specs = vec![
        spec("waits", SpecState::Open),
        spec("waiver", SpecState::Open),
    ];
    fixture.awaiting = set(&["waits"]);
    assert_eq!(ids(&fixture.run().awaiting), vec!["waits"]);
}

#[test]
fn lists_are_sorted_by_id() {
    let mut fixture = Fixture::with_release();
    fixture.specs = vec![
        completed("c", None, day(2026, 10, 25)),
        completed("a", None, day(2026, 10, 25)),
        completed("b", None, day(2026, 10, 25)),
    ];
    fixture.other_commits = 3;
    let next = fixture.run();
    assert_eq!(ids(&next.merged), vec!["a", "b", "c"]);
    assert_eq!(next.other_commits, 3);
}

#[test]
fn withdrawn_spec_is_in_no_list() {
    let mut fixture = Fixture::with_release();
    fixture.specs = vec![spec("gone", SpecState::Withdrawn)];
    fixture.awaiting = set(&["gone"]);
    let next = fixture.run();
    assert!(next.merged.is_empty());
    assert!(next.in_progress.is_empty());
    assert!(next.approved.is_empty());
    assert!(next.parked.is_empty());
    assert!(next.awaiting.is_empty());
    assert!(next.not_clear.is_empty());
}

fn stale(id: &str, approved_at: OffsetDateTime) -> SpecFacts {
    SpecFacts {
        approved_at: Some(approved_at),
        approval_current: false,
        ..spec(id, SpecState::Approved)
    }
}

#[test]
fn a_stale_approval_lists_in_awaiting_with_the_note() {
    let fixture = Fixture {
        specs: vec![stale("old", now())],
        titles: HashMap::from([("old".to_string(), "T".to_string())]),
        ..Default::default()
    };
    let next = fixture.run();
    assert!(next.approved.is_empty());
    assert_eq!(ids(&next.awaiting), vec!["old"]);
    assert_eq!(next.awaiting[0].title, "T (approved at an older text)");
}

#[test]
fn a_stale_approval_with_no_title_gets_the_note_alone() {
    let fixture = Fixture {
        specs: vec![stale("old", now())],
        ..Default::default()
    };
    let next = fixture.run();
    assert_eq!(next.awaiting[0].title, "(approved at an older text)");
}

#[test]
fn a_current_approval_lists_in_approved() {
    let fixture = Fixture {
        specs: vec![SpecFacts {
            approved_at: Some(now()),
            ..spec("ok", SpecState::Approved)
        }],
        ..Default::default()
    };
    let next = fixture.run();
    assert_eq!(ids(&next.approved), vec!["ok"]);
    assert!(next.awaiting.is_empty());
}

#[test]
fn a_stale_approval_before_the_first_merged_file_is_in_no_list() {
    let fixture = Fixture {
        first_merged_file: Some(day(2026, 10, 22)),
        specs: vec![stale("history", day(2026, 10, 20))],
        ..Default::default()
    };
    let next = fixture.run();
    assert!(next.approved.is_empty());
    assert!(next.awaiting.is_empty());
}

#[test]
fn a_parked_stale_approval_is_only_parked() {
    let fixture = Fixture {
        specs: vec![stale("p", now())],
        parked: set(&["p"]),
        ..Default::default()
    };
    let next = fixture.run();
    assert_eq!(ids(&next.parked), vec!["p"]);
    assert!(next.awaiting.is_empty());
    assert!(next.approved.is_empty());
}
