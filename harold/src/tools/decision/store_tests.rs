use super::*;

struct Temp(std::path::PathBuf);

impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("harold-decisiondb-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 2026-11-14 03:10 UTC, in milliseconds.
const NOV_14: i64 = 1_794_625_800_000;
const DAY_MS: i64 = 86_400_000;

fn new_decision(title: &str, check_on: &str) -> NewDecision {
    NewDecision {
        organisation: "personal-chief".into(),
        role: "pnem".into(),
        kind: "slicing".into(),
        details_owner: "pnem:harold".into(),
        accountable: "pnem".into(),
        title: title.into(),
        link: "spec:harold/pod-role-record".into(),
        check_on: check_on.into(),
        details: "pod:harold/specs/pod-role-record.decisions.md#d-20261114-02".into(),
        asked_at_ms: NOV_14 - 5_000,
    }
}

fn plain(filter_status: &[&str]) -> Filter {
    Filter {
        statuses: filter_status.iter().map(|s| (*s).to_owned()).collect(),
        ..Filter::default()
    }
}

#[tokio::test]
async fn add_keeps_the_given_id_and_refuses_a_used_one() {
    let dir = Temp::new();
    let store = DecisionStore::open(&dir.0).await.unwrap();
    let first = store
        .add("d-20261114-02", &new_decision("a", "2026-12-01"), NOV_14)
        .await
        .unwrap();
    assert_eq!(first.unwrap().id, "d-20261114-02");
    let used = store
        .add(
            "d-20261114-02",
            &new_decision("b", "2026-12-01"),
            NOV_14 + 1,
        )
        .await
        .unwrap();
    assert_eq!(used, None, "a used id is refused and nothing is stored");
    assert_eq!(store.list(&plain(&[])).await.unwrap().len(), 1);
}

#[tokio::test]
async fn decided_on_is_the_utc_date_of_the_add_even_for_an_earlier_id() {
    let dir = Temp::new();
    let store = DecisionStore::open(&dir.0).await.unwrap();
    let row = store
        .add("d-20261113-07", &new_decision("a", "2026-12-01"), NOV_14)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.decided_on, "2026-11-14");
}

#[tokio::test]
async fn each_field_is_kept_and_a_new_record_is_open() {
    let dir = Temp::new();
    let store = DecisionStore::open(&dir.0).await.unwrap();
    let added = store
        .add(
            "d-20261114-21",
            &new_decision("Fold the fix", "2026-12-01"),
            NOV_14,
        )
        .await
        .unwrap()
        .unwrap();
    let row = store.get(&added.id).await.unwrap().unwrap();
    assert_eq!(row.decided_on, "2026-11-14");
    assert_eq!(row.organisation, "personal-chief");
    assert_eq!(row.role, "pnem");
    assert_eq!(row.kind, "slicing");
    assert_eq!(row.details_owner, "pnem:harold");
    assert_eq!(row.accountable, "pnem");
    assert_eq!(row.title, "Fold the fix");
    assert_eq!(row.link, "spec:harold/pod-role-record");
    assert_eq!(row.check_on, "2026-12-01");
    assert_eq!(row.status, Status::Open);
    assert_eq!(row.process_grade, "");
    assert_eq!(row.outcome_grade, "");
    assert_eq!(
        row.details,
        "pod:harold/specs/pod-role-record.decisions.md#d-20261114-02"
    );
    assert_eq!(row.asked_at_ms, NOV_14 - 5_000);
    assert_eq!(row.added_at_ms, NOV_14);
    assert_eq!(row.evaluated_at_ms, 0);
    assert!(store.get("d-20990101-01").await.unwrap().is_none());
}

#[tokio::test]
async fn a_link_to_a_closed_or_rotated_work_record_is_accepted() {
    let dir = Temp::new();
    let store = DecisionStore::open(&dir.0).await.unwrap();
    let mut decision = new_decision("t", "2026-12-01");
    decision.link = "work:a-record-that-was-rotated-away".into();
    let added = store
        .add("d-20261114-11", &decision, NOV_14)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(added.link, "work:a-record-that-was-rotated-away");
    let moved = store
        .set_link(&added.id, "work:another")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(moved.link, "work:another");
    assert!(
        store
            .set_link("d-20990101-01", "work:x")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn the_records_stay_after_the_store_is_opened_again() {
    let dir = Temp::new();
    {
        let store = DecisionStore::open(&dir.0).await.unwrap();
        store
            .add("d-20261114-22", &new_decision("keep", "2026-12-01"), NOV_14)
            .await
            .unwrap()
            .unwrap();
    }
    let store = DecisionStore::open(&dir.0).await.unwrap();
    assert_eq!(store.list(&plain(&["open"])).await.unwrap().len(), 1);
    assert!(dir.0.join("decisions.db").is_file());
}

#[tokio::test]
async fn evaluate_sets_the_grades_and_refuses_a_second_time() {
    let dir = Temp::new();
    let store = DecisionStore::open(&dir.0).await.unwrap();
    let added = store
        .add("d-20261114-23", &new_decision("t", "2026-11-01"), NOV_14)
        .await
        .unwrap()
        .unwrap();
    let done = store
        .evaluate(&added.id, "good", "poor", NOV_14 + DAY_MS)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(done.status, Status::Evaluated);
    assert_eq!(
        (done.process_grade.as_str(), done.outcome_grade.as_str()),
        ("good", "poor")
    );
    assert_eq!(done.evaluated_at_ms, NOV_14 + DAY_MS);
    let again = store
        .evaluate(&added.id, "poor", "poor", NOV_14 + 2 * DAY_MS)
        .await
        .unwrap();
    assert_eq!(again, Err(Refusal::AlreadyEvaluated));
    let unknown = store
        .evaluate("d-20990101-01", "good", "good", NOV_14)
        .await
        .unwrap();
    assert_eq!(unknown, Err(Refusal::Unknown));
    let kept = store.get(&added.id).await.unwrap().unwrap();
    assert_eq!(kept.process_grade, "good");
}

#[tokio::test]
async fn mark_due_sets_each_open_record_whose_check_date_has_come() {
    let dir = Temp::new();
    let store = DecisionStore::open(&dir.0).await.unwrap();
    let past = store
        .add("d-20261114-24", &new_decision("past", "2026-11-13"), NOV_14)
        .await
        .unwrap()
        .unwrap();
    let today = store
        .add(
            "d-20261114-25",
            &new_decision("today", "2026-11-14"),
            NOV_14,
        )
        .await
        .unwrap()
        .unwrap();
    let later = store
        .add(
            "d-20261114-26",
            &new_decision("later", "2026-11-15"),
            NOV_14,
        )
        .await
        .unwrap()
        .unwrap();
    let turned: Vec<String> = store
        .mark_due("2026-11-14")
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.id)
        .collect();
    assert_eq!(turned, [past.id.clone(), today.id.clone()]);
    assert_eq!(
        store.get(&later.id).await.unwrap().unwrap().status,
        Status::Open
    );
    assert!(
        store.mark_due("2026-11-14").await.unwrap().is_empty(),
        "a due record is not turned twice"
    );
    assert_eq!(store.counts().await.unwrap(), (1, 2));
}

#[tokio::test]
async fn recheck_moves_the_date_and_sends_a_due_record_back_to_open() {
    let dir = Temp::new();
    let store = DecisionStore::open(&dir.0).await.unwrap();
    let added = store
        .add("d-20261114-27", &new_decision("t", "2026-11-01"), NOV_14)
        .await
        .unwrap()
        .unwrap();
    store.mark_due("2026-11-14").await.unwrap();
    let back = store
        .recheck(&added.id, "2026-12-25")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (back.status, back.check_on.as_str()),
        (Status::Open, "2026-12-25")
    );
    store
        .evaluate(&added.id, "good", "good", NOV_14)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        store.recheck(&added.id, "2027-01-01").await.unwrap(),
        Err(Refusal::AlreadyEvaluated)
    );
    assert_eq!(
        store.recheck("d-20990101-01", "2027-01-01").await.unwrap(),
        Err(Refusal::Unknown)
    );
}

#[tokio::test]
async fn list_gives_the_oldest_check_date_first_and_each_filter_works() {
    let dir = Temp::new();
    let store = DecisionStore::open(&dir.0).await.unwrap();
    let late = store
        .add("d-20261114-28", &new_decision("late", "2026-12-20"), NOV_14)
        .await
        .unwrap()
        .unwrap();
    let mut other = new_decision("early", "2026-11-02");
    other.role = "chief".into();
    other.kind = "model".into();
    other.details_owner = "chief".into();
    other.accountable = "user".into();
    let early = store
        .add("d-20261114-12", &other, NOV_14 + 1)
        .await
        .unwrap()
        .unwrap();
    let ids = |rows: Vec<DecisionRow>| rows.into_iter().map(|r| r.id).collect::<Vec<_>>();
    assert_eq!(
        ids(store.list(&plain(&["open", "due"])).await.unwrap()),
        [early.id.clone(), late.id.clone()]
    );
    for (filter, expected) in [
        (
            Filter {
                role: Some("chief".into()),
                ..plain(&["open"])
            },
            vec![early.id.clone()],
        ),
        (
            Filter {
                kind: Some("slicing".into()),
                ..plain(&["open"])
            },
            vec![late.id.clone()],
        ),
        (
            Filter {
                details_owner: Some("pnem:harold".into()),
                ..plain(&["open"])
            },
            vec![late.id.clone()],
        ),
        (
            Filter {
                accountable: Some("user".into()),
                ..plain(&["open"])
            },
            vec![early.id.clone()],
        ),
        (plain(&["due"]), vec![]),
    ] {
        assert_eq!(ids(store.list(&filter).await.unwrap()), expected);
    }
    store
        .evaluate(&early.id, "good", "good", NOV_14)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        store.list(&plain(&[])).await.unwrap().len(),
        2,
        "no status filter means all"
    );
}
