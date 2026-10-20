use std::path::PathBuf;

use super::*;
use crate::tools::questions::schema::{Choice, Decision, Kind, Picked};

struct TempFolder(PathBuf);

impl TempFolder {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!("harold-questions-{}", uuid::Uuid::new_v4())))
    }
}

impl Drop for TempFolder {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn question(id: &str) -> Question {
    Question {
        id: id.into(),
        product: "p".into(),
        spec: "s".into(),
        sha: "abc".into(),
        link: "https://example.com".into(),
        title: String::new(),
        steps: vec![],
        intro: "intro".into(),
        decisions: vec![Decision {
            ask: "Which?".into(),
            options: vec![Choice {
                key: "a".into(),
                label: "A".into(),
                impact: "i".into(),
                suggested: false,
                free: false,
            }],
        }],
    }
}

fn an_answer() -> Answer {
    Answer {
        ticked: 0,
        kind: Kind::Answer,
        picks: vec![Picked {
            decision: 0,
            key: "a".into(),
            ask: "Which?".into(),
            label: "A".into(),
            free: false,
        }],
        note: String::new(),
    }
}

fn ids(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

fn at_v1(items: &[&str]) -> Vec<(String, u32)> {
    items.iter().map(|s| (s.to_string(), 1)).collect()
}

/// The `(id, version)` of each answer `ready` returns.
const SETTLE: i64 = 3_000;

/// The unsent answers, as `ready` reports them long after they settled.
async fn ready_of(store: &TursoQuestionStore) -> Option<Ready> {
    match store.ready(i64::MAX / 2, SETTLE).await.unwrap() {
        Readiness::Ready(ready) => Some(ready),
        _ => None,
    }
}

async fn answer_at(store: &TursoQuestionStore, id: &str, send_now: bool, now_ms: i64) {
    let result = store
        .answer(id, 1, send_now, |_| Ok(an_answer()), now_ms)
        .await
        .unwrap();
    assert_eq!(result, Ok(Ok(())));
}

fn ready_ids(readiness: Readiness) -> Vec<String> {
    let Readiness::Ready(ready) = readiness else {
        panic!("expected Ready, got {readiness:?}");
    };
    ready.answers.into_iter().map(|a| a.0).collect()
}

fn ready_answers(ready: &Ready) -> Vec<(String, u32)> {
    ready
        .answers
        .iter()
        .map(|(id, version, ..)| (id.clone(), *version))
        .collect()
}

async fn listed_ids(store: &TursoQuestionStore) -> Vec<(String, bool)> {
    store
        .list(0)
        .await
        .unwrap()
        .questions
        .into_iter()
        .map(|l| (l.question.id, l.deferred))
        .collect()
}

async fn answer_ok(store: &TursoQuestionStore, id: &str, version: u32) {
    let result = store
        .answer(id, version, false, |_| Ok(an_answer()), 5)
        .await
        .unwrap();
    assert_eq!(result, Ok(Ok(())));
}

async fn migration_rows(folder: &Path) -> Vec<String> {
    let database = turso::Builder::new_local(folder.join(DATABASE).to_str().unwrap())
        .build()
        .await
        .unwrap();
    let conn = database.connect().unwrap();
    let mut rows = conn
        .query("SELECT name FROM _migrations ORDER BY id", ())
        .await
        .unwrap();
    let mut names = Vec::new();
    while let Some(row) = rows.next().await.unwrap() {
        names.push(text(&row, 0).unwrap());
    }
    names
}

#[tokio::test]
async fn open_creates_and_migrates_questions_db() {
    let temp = TempFolder::new();
    let folder = temp.0.join("tools").join("questions");
    let store = TursoQuestionStore::open(&folder).await.unwrap();
    assert!(folder.join(DATABASE).exists());
    assert_eq!(store.put(question("Q1"), 1).await.unwrap(), 1);
    drop(store);
    assert_eq!(
        migration_rows(&folder).await,
        [
            "001_questions",
            "002_answer_send_now",
            "003_step_ticks",
            "004_saved_picks"
        ]
    );
}

#[tokio::test]
async fn reopening_keeps_the_rows_and_applies_nothing_twice() {
    let temp = TempFolder::new();
    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    store.put(question("Q1"), 1).await.unwrap();
    drop(store);

    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    assert_eq!(listed_ids(&store).await, [("Q1".to_string(), false)]);
    drop(store);
    assert_eq!(
        migration_rows(&temp.0).await,
        [
            "001_questions",
            "002_answer_send_now",
            "003_step_ticks",
            "004_saved_picks"
        ]
    );
}

#[tokio::test]
async fn put_stores_version_1_and_again_gives_version_2() {
    let temp = TempFolder::new();
    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    assert_eq!(store.put(question("Q1"), 1).await.unwrap(), 1);
    store.put(question("Q2"), 2).await.unwrap();
    assert_eq!(store.put(question("Q1"), 3).await.unwrap(), 2);
    // a waiting question keeps its place
    assert_eq!(
        listed_ids(&store).await,
        [("Q1".to_string(), false), ("Q2".to_string(), false)]
    );
    let listing = store.list(0).await.unwrap();
    assert_eq!(listing.questions[0].version, 2);

    // an answered one is waiting again, last, without its answer
    answer_ok(&store, "Q1", 2).await;
    assert_eq!(store.put(question("Q1"), 4).await.unwrap(), 3);
    assert_eq!(
        listed_ids(&store).await,
        [("Q2".to_string(), false), ("Q1".to_string(), false)]
    );
}

#[tokio::test]
async fn reorder_puts_the_listed_ids_first() {
    let temp = TempFolder::new();
    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    for id in ["Q1", "Q2", "Q3", "Q4"] {
        store.put(question(id), 1).await.unwrap();
    }
    store.dismiss("Q4", "gone", 2).await.unwrap().unwrap();
    store
        .reorder(&ids(&["Q3", "nope", "Q4", "Q2"]), 3)
        .await
        .unwrap();
    let order: Vec<String> = listed_ids(&store).await.into_iter().map(|l| l.0).collect();
    assert_eq!(order, ["Q3", "Q2", "Q1"]);
}

#[tokio::test]
async fn defer_moves_one_to_the_end_as_deferred() {
    let temp = TempFolder::new();
    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    for id in ["Q1", "Q2", "Q3"] {
        store.put(question(id), 1).await.unwrap();
    }
    assert_eq!(store.defer("Q1", 2).await.unwrap(), Ok(1));
    assert_eq!(
        listed_ids(&store).await,
        [
            ("Q2".to_string(), false),
            ("Q3".to_string(), false),
            ("Q1".to_string(), true)
        ]
    );
    assert_eq!(
        store.defer("missing", 2).await.unwrap(),
        Err(Refusal::NotWaiting)
    );
    // a deferred question can still be answered
    answer_ok(&store, "Q1", 1).await;
}

#[tokio::test]
async fn concurrent_reorder_and_defer_lose_neither() {
    let temp = TempFolder::new();
    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    for id in ["Q1", "Q2", "Q3"] {
        store.put(question(id), 1).await.unwrap();
    }
    let order = ids(&["Q3", "Q2"]);
    let (reordered, deferred) = tokio::join!(store.reorder(&order, 2), store.defer("Q1", 2));
    reordered.unwrap();
    assert_eq!(deferred.unwrap(), Ok(1));
    let listed = listed_ids(&store).await;
    assert_eq!(listed.len(), 3);
    assert_eq!(listed.last().unwrap(), &("Q1".to_string(), true));
    // whichever ran first, the reorder's relative order holds for Q3 and Q2
    let names: Vec<&str> = listed.iter().map(|l| l.0.as_str()).collect();
    assert_eq!(names, ["Q3", "Q2", "Q1"]);
}

#[tokio::test]
async fn answer_with_the_current_version_records_it() {
    let temp = TempFolder::new();
    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    store.put(question("Q1"), 1).await.unwrap();

    // the closure's refusal stores nothing
    let refused = store
        .answer("Q1", 1, false, |_| Err("picks".to_string()), 2)
        .await
        .unwrap();
    assert_eq!(refused, Ok(Err("picks".to_string())));
    assert_eq!(listed_ids(&store).await.len(), 1);

    let result = store
        .answer(
            "Q1",
            1,
            false,
            |q| {
                assert_eq!(q.id, "Q1");
                Ok(an_answer())
            },
            3,
        )
        .await
        .unwrap();
    assert_eq!(result, Ok(Ok(())));
    assert!(listed_ids(&store).await.is_empty());

    let ready = ready_of(&store).await.unwrap();
    assert_eq!(ready.answers.len(), 1);
    let (id, version, q, answer) = &ready.answers[0];
    assert_eq!((id.as_str(), *version), ("Q1", 1));
    assert_eq!(q, &question("Q1"));
    assert_eq!(answer, &an_answer());
    assert!(ready.later.is_empty());
}

#[tokio::test]
async fn an_old_version_is_version_changed_and_a_dismissed_one_not_waiting() {
    let temp = TempFolder::new();
    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    store.put(question("Q1"), 1).await.unwrap();
    store.put(question("Q1"), 2).await.unwrap();
    let old = store
        .answer("Q1", 1, false, |_| Ok(an_answer()), 3)
        .await
        .unwrap();
    assert_eq!(old, Err(Refusal::VersionChanged));
    assert!(ready_of(&store).await.is_none());

    assert_eq!(store.dismiss("Q1", "withdrawn", 4).await.unwrap(), Ok(2));
    let gone = store
        .answer("Q1", 2, false, |_| Ok(an_answer()), 5)
        .await
        .unwrap();
    assert_eq!(gone, Err(Refusal::NotWaiting));
    let unknown = store
        .answer("zz", 1, false, |_| Ok(an_answer()), 5)
        .await
        .unwrap();
    assert_eq!(unknown, Err(Refusal::NotWaiting));
    assert_eq!(
        store.dismiss("Q1", "again", 6).await.unwrap(),
        Err(Refusal::NotWaiting)
    );

    // the reason is listed for an hour
    let listing = store.list(5).await.unwrap();
    assert_eq!(
        listing.dismissed,
        [("Q1".to_string(), "withdrawn".to_string())]
    );
    let later = store.list(4 + 3_600_001).await.unwrap();
    assert!(later.dismissed.is_empty());
}

#[tokio::test]
async fn a_batch_typed_marks_answers_sent_and_deferred_waiting() {
    let temp = TempFolder::new();
    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    for id in ["Q1", "Q2", "Q3"] {
        store.put(question(id), 1).await.unwrap();
    }
    answer_ok(&store, "Q1", 1).await;
    answer_ok(&store, "Q3", 1).await;
    store.defer("Q2", 2).await.unwrap().unwrap();

    let batch = store
        .start_batch(&at_v1(&["Q1", "Q3"]), "ANSWERS 2", 10)
        .await
        .unwrap()
        .expect("batch started");
    assert_eq!(batch.text, "ANSWERS 2");
    // started answers are no longer unsent
    assert!(ready_of(&store).await.is_none());
    assert_eq!(store.started_batch().await.unwrap(), Some(batch.clone()));

    store.batch_typed(batch.id, 11).await.unwrap();
    assert!(store.started_batch().await.unwrap().is_none());
    assert_eq!(listed_ids(&store).await, [("Q2".to_string(), false)]);
    assert!(ready_of(&store).await.is_none());

    // sent answers are not sent again; the returned question is ready once answered
    answer_ok(&store, "Q2", 1).await;
    let ready = ready_of(&store).await.unwrap();
    assert_eq!(ready.answers.len(), 1);
    assert_eq!(ready.answers[0].0, "Q2");
}

#[tokio::test]
async fn a_started_batch_survives_reopening() {
    let temp = TempFolder::new();
    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    store.put(question("Q1"), 1).await.unwrap();
    answer_ok(&store, "Q1", 1).await;
    let first = store
        .start_batch(&at_v1(&["Q1"]), "ANSWERS 1", 10)
        .await
        .unwrap()
        .expect("batch started");
    drop(store);

    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    assert_eq!(store.started_batch().await.unwrap(), Some(first.clone()));
    assert!(ready_of(&store).await.is_none());
    store.batch_typed(first.id, 11).await.unwrap();
    assert!(store.started_batch().await.unwrap().is_none());
}

#[tokio::test]
async fn start_batch_refuses_when_a_listed_answer_changed() {
    let temp = TempFolder::new();
    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    store.put(question("Q1"), 1).await.unwrap();
    store.put(question("Q2"), 1).await.unwrap();
    answer_ok(&store, "Q1", 1).await;
    answer_ok(&store, "Q2", 1).await;
    let ready = ready_of(&store).await.expect("Q1 and Q2 are ready");

    // The chief re-puts the answered Q1 between `ready` and `start_batch`.
    assert_eq!(store.put(question("Q1"), 2).await.unwrap(), 2);
    let started = store
        .start_batch(&ready_answers(&ready), "ANSWERS 2 (stale)", 10)
        .await
        .unwrap();
    assert_eq!(started, None);
    assert_eq!(store.started_batch().await.unwrap(), None);

    // Even answered again, Q1 v2 is not the v1 answer `ready` listed.
    answer_ok(&store, "Q1", 2).await;
    let started = store
        .start_batch(&ready_answers(&ready), "ANSWERS 2 (stale)", 11)
        .await
        .unwrap();
    assert_eq!(started, None);

    let ready = ready_of(&store).await.unwrap();
    assert_eq!(
        ready_answers(&ready),
        vec![("Q2".to_string(), 1), ("Q1".to_string(), 2)]
    );
    let batch = store
        .start_batch(&ready_answers(&ready), "ANSWERS 2", 12)
        .await
        .unwrap()
        .expect("the fresh answers start a batch");
    assert_eq!(store.started_batch().await.unwrap(), Some(batch));
    println!(
        "evidence: start_batch after a re-put of Q1 -> None, and again after Q1 v2 was answered; the fresh ready (Q2 v1, Q1 v2) starts it"
    );
}

#[tokio::test]
async fn a_write_dropped_inside_its_transaction_does_not_block_the_next() {
    let temp = TempFolder::new();
    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    // A cancelled call leaves the connection inside BEGIN IMMEDIATE with the
    // mutex released. Reproduced by hand, since the drop cannot be timed
    // reliably between two awaits.
    store.put(question("Q1"), 1).await.unwrap();
    {
        let conn = store.conn.lock().await;
        begin(&conn).await.unwrap();
        let applied = answer_in(&conn, "Q1", 1, false, |_| Ok(an_answer()), 2)
            .await
            .unwrap();
        assert_eq!(applied, Ok(Ok(())));
        assert!(!conn.is_autocommit().unwrap());
    }
    // The reads roll the half-applied answer back instead of showing it.
    assert_eq!(listed_ids(&store).await, vec![("Q1".to_string(), false)]);
    {
        let conn = store.conn.lock().await;
        begin(&conn).await.unwrap();
        answer_in(&conn, "Q1", 1, false, |_| Ok(an_answer()), 2)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
    assert!(ready_of(&store).await.is_none());
    {
        let conn = store.conn.lock().await;
        begin(&conn).await.unwrap();
        answer_in(&conn, "Q1", 1, false, |_| Ok(an_answer()), 2)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
    assert!(store.started_batch().await.unwrap().is_none());
    {
        let conn = store.conn.lock().await;
        begin(&conn).await.unwrap();
        assert!(!conn.is_autocommit().unwrap());
    }
    assert_eq!(store.put(question("Q2"), 3).await.unwrap(), 1);
    assert_eq!(listed_ids(&store).await.len(), 2);
}

/// Each question's `(id, state, batch_id)`, by id.
async fn batch_columns(store: &TursoQuestionStore) -> Vec<(String, String, Option<i64>)> {
    let conn = store.conn.lock().await;
    let mut rows = conn
        .query("SELECT id, state, batch_id FROM questions ORDER BY id", ())
        .await
        .unwrap();
    let mut columns = Vec::new();
    while let Some(row) = rows.next().await.unwrap() {
        let batch_id = row.get_value(2).unwrap().as_integer().copied();
        columns.push((text(&row, 0).unwrap(), text(&row, 1).unwrap(), batch_id));
    }
    columns
}

/// Each batch row's `(id, typed)`.
async fn batch_rows(store: &TursoQuestionStore) -> Vec<(i64, bool)> {
    let conn = store.conn.lock().await;
    let mut rows = conn
        .query("SELECT id, typed_at_ms FROM batches ORDER BY id", ())
        .await
        .unwrap();
    let mut batches = Vec::new();
    while let Some(row) = rows.next().await.unwrap() {
        let typed = row.get_value(1).unwrap().as_integer().is_some();
        batches.push((integer(&row, 0).unwrap(), typed));
    }
    batches
}

#[tokio::test]
async fn release_batch_deletes_the_row_and_returns_its_answers_unsent() {
    let temp = TempFolder::new();
    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    for id in ["Q1", "Q2", "Q3"] {
        store.put(question(id), 1).await.unwrap();
    }
    answer_ok(&store, "Q1", 1).await;
    answer_ok(&store, "Q2", 1).await;
    store.defer("Q3", 2).await.unwrap().unwrap();
    let batch = store
        .start_batch(&at_v1(&["Q1", "Q2"]), "ANSWERS 2", 10)
        .await
        .unwrap()
        .expect("batch started");
    assert_eq!(batch_rows(&store).await, [(batch.id, false)]);

    assert!(store.release_batch(batch.id).await.unwrap());

    assert!(batch_rows(&store).await.is_empty(), "the batch row is gone");
    assert_eq!(store.started_batch().await.unwrap(), None);
    let columns = batch_columns(&store).await;
    assert_eq!(
        columns,
        [
            ("Q1".to_string(), "answered".to_string(), None),
            ("Q2".to_string(), "answered".to_string(), None),
            ("Q3".to_string(), "deferred".to_string(), None),
        ]
    );
    let ready = store.ready(6_000, SETTLE).await.unwrap();
    let Readiness::Ready(ready) = ready else {
        panic!("the answers are unsent again, with no second settle: {ready:?}");
    };
    assert_eq!(ready_answers(&ready), at_v1(&["Q1", "Q2"]));
    assert_eq!(ready.later, ["Q3"]);
    // A question put now does not hold the answers.
    store.put(question("Q4"), 12).await.unwrap();
    assert!(ready_of(&store).await.is_some());
    println!(
        "evidence: released batch {}: no batch rows; questions {columns:?}",
        batch.id
    );
}

#[tokio::test]
async fn a_typed_or_unknown_batch_is_not_released() {
    let temp = TempFolder::new();
    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    store.put(question("Q1"), 1).await.unwrap();
    answer_ok(&store, "Q1", 1).await;
    let batch = store
        .start_batch(&at_v1(&["Q1"]), "ANSWERS 1", 10)
        .await
        .unwrap()
        .expect("batch started");
    store.batch_typed(batch.id, 11).await.unwrap();

    assert!(!store.release_batch(batch.id).await.unwrap());
    assert!(!store.release_batch(batch.id + 1).await.unwrap());

    assert_eq!(batch_rows(&store).await, [(batch.id, true)]);
    assert_eq!(
        batch_columns(&store).await,
        [("Q1".to_string(), "sent".to_string(), Some(batch.id))]
    );
    assert!(ready_of(&store).await.is_none());
}

async fn marked(store: &TursoQuestionStore, id: &str) -> bool {
    store.marked(id).await
}

#[tokio::test]
async fn migration_002_adds_send_now_defaulting_to_zero_and_a_second_open_applies_nothing() {
    let temp = TempFolder::new();
    std::fs::create_dir_all(&temp.0).unwrap();
    // A database as the first release left it: migration 001 only, one row.
    let database = turso::Builder::new_local(temp.0.join(DATABASE).to_str().unwrap())
        .build()
        .await
        .unwrap();
    let conn = database.connect().unwrap();
    let first = MIGRATIONS[0].1;
    conn.execute_batch(first).await.unwrap();
    conn.execute(
        "CREATE TABLE _migrations (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE, \
         checksum TEXT NOT NULL, applied_at_ms INTEGER NOT NULL)",
        (),
    )
    .await
    .unwrap();
    conn.execute(
        "INSERT INTO _migrations (name, checksum, applied_at_ms) VALUES (?1, ?2, 0)",
        (
            "001_questions",
            hex::encode(Sha256::digest(first.as_bytes())),
        ),
    )
    .await
    .unwrap();
    conn.execute(
        "INSERT INTO questions (id, version, position, state, body, updated_at_ms) \
         VALUES ('Q1', 1, 1, 'waiting', ?1, 0)",
        [serde_json::to_string(&question("Q1")).unwrap()],
    )
    .await
    .unwrap();
    drop(conn);
    drop(database);

    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    assert!(!marked(&store, "Q1").await);
    drop(store);
    assert_eq!(
        migration_rows(&temp.0).await,
        [
            "001_questions",
            "002_answer_send_now",
            "003_step_ticks",
            "004_saved_picks"
        ]
    );

    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    assert!(!marked(&store, "Q1").await);
    drop(store);
    assert_eq!(migration_rows(&temp.0).await.len(), 4);
}

#[tokio::test]
async fn answer_with_send_now_stores_the_mark() {
    let temp = TempFolder::new();
    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    store.put(question("Q1"), 1).await.unwrap();
    store.put(question("Q2"), 1).await.unwrap();
    let marked_answer = store
        .answer("Q1", 1, true, |_| Ok(an_answer()), 2)
        .await
        .unwrap();
    assert_eq!(marked_answer, Ok(Ok(())));
    answer_ok(&store, "Q2", 1).await;
    assert!(marked(&store, "Q1").await);
    assert!(!marked(&store, "Q2").await);
}

#[tokio::test]
async fn put_again_clears_the_mark() {
    let temp = TempFolder::new();
    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    store.put(question("Q1"), 1).await.unwrap();
    store
        .answer("Q1", 1, true, |_| Ok(an_answer()), 2)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(marked(&store, "Q1").await);
    // an answered question is not open: the put takes the re-queue branch
    store.put(question("Q1"), 3).await.unwrap();
    assert!(!marked(&store, "Q1").await);
}

#[tokio::test]
async fn no_unsent_answers_is_nothing() {
    let temp = TempFolder::new();
    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    assert_eq!(store.ready(0, SETTLE).await.unwrap(), Readiness::Nothing);
    store.put(question("Q1"), 1).await.unwrap();
    assert_eq!(
        store.ready(9_000, SETTLE).await.unwrap(),
        Readiness::Nothing
    );
}

#[tokio::test]
async fn unmarked_answer_settles() {
    let temp = TempFolder::new();
    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    store.put(question("Q1"), 0).await.unwrap();
    answer_at(&store, "Q1", false, 0).await;
    assert_eq!(
        store.ready(1_000, SETTLE).await.unwrap(),
        Readiness::Settling { wait_ms: 2_000 }
    );
    assert_eq!(ready_ids(store.ready(3_000, SETTLE).await.unwrap()), ["Q1"]);
}

#[tokio::test]
async fn settle_runs_from_the_newest_unsent_answer() {
    let temp = TempFolder::new();
    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    store.put(question("Q1"), 0).await.unwrap();
    store.put(question("Q2"), 0).await.unwrap();
    answer_at(&store, "Q1", false, 0).await;
    answer_at(&store, "Q2", false, 2_000).await;
    assert_eq!(
        store.ready(3_000, SETTLE).await.unwrap(),
        Readiness::Settling { wait_ms: 2_000 }
    );
    assert_eq!(
        ready_ids(store.ready(5_000, SETTLE).await.unwrap()),
        ["Q1", "Q2"]
    );
}

#[tokio::test]
async fn a_marked_answer_is_ready_at_once_and_takes_unmarked_ones_with_it() {
    let temp = TempFolder::new();
    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    store.put(question("Q1"), 0).await.unwrap();
    store.put(question("Q2"), 0).await.unwrap();
    answer_at(&store, "Q2", false, 0).await;
    answer_at(&store, "Q1", true, 1_000).await;
    assert_eq!(
        ready_ids(store.ready(1_000, SETTLE).await.unwrap()),
        ["Q1", "Q2"]
    );
}

#[tokio::test]
async fn waiting_questions_no_longer_hold_readiness() {
    let temp = TempFolder::new();
    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    for n in 25..=29 {
        store.put(question(&format!("Q{n}")), 0).await.unwrap();
    }
    answer_at(&store, "Q25", false, 0).await;
    answer_at(&store, "Q26", false, 0).await;
    assert_eq!(
        ready_ids(store.ready(3_000, SETTLE).await.unwrap()),
        ["Q25", "Q26"]
    );
    assert_eq!(listed_ids(&store).await.len(), 3);
}

#[tokio::test]
async fn a_wall_clock_behind_the_answer_does_not_hold_the_line_back() {
    let temp = TempFolder::new();
    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    store.put(question("Q1"), 0).await.unwrap();
    answer_at(&store, "Q1", false, 10_000).await;
    assert_eq!(ready_ids(store.ready(4_000, SETTLE).await.unwrap()), ["Q1"]);
}

#[tokio::test]
async fn release_keeps_the_answers_updated_at() {
    let temp = TempFolder::new();
    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    store.put(question("Q1"), 0).await.unwrap();
    answer_at(&store, "Q1", false, 0).await;
    let batch = store
        .start_batch(&at_v1(&["Q1"]), "ANSWERS 1", 5_000)
        .await
        .unwrap()
        .expect("batch started");
    assert!(store.release_batch(batch.id).await.unwrap());
    assert_eq!(ready_ids(store.ready(6_000, SETTLE).await.unwrap()), ["Q1"]);
}

#[tokio::test]
async fn start_batch_ignores_waiting_questions_but_refuses_a_put_again_answer() {
    let temp = TempFolder::new();
    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    store.put(question("Q1"), 0).await.unwrap();
    store.put(question("Q2"), 0).await.unwrap();
    answer_at(&store, "Q1", false, 0).await;
    let started = store
        .start_batch(&at_v1(&["Q1"]), "ANSWERS 1", 10)
        .await
        .unwrap();
    assert!(started.is_some(), "Q2 waiting does not hold the batch");

    answer_at(&store, "Q2", false, 20).await;
    store.put(question("Q2"), 30).await.unwrap();
    let refused = store
        .start_batch(&at_v1(&["Q2"]), "ANSWERS 1", 40)
        .await
        .unwrap();
    assert_eq!(refused, None);
}

#[tokio::test]
async fn batch_typed_returns_deferred_only_when_nothing_waits() {
    let temp = TempFolder::new();
    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    for id in ["Q1", "Q2", "Q3", "Q4"] {
        store.put(question(id), 0).await.unwrap();
    }
    store.defer("Q4", 1).await.unwrap().unwrap();
    answer_at(&store, "Q1", false, 2).await;
    let first = store
        .start_batch(&at_v1(&["Q1"]), "ANSWERS 1", 10)
        .await
        .unwrap()
        .unwrap();
    store.batch_typed(first.id, 11).await.unwrap();
    assert_eq!(
        listed_ids(&store).await,
        [
            ("Q2".to_string(), false),
            ("Q3".to_string(), false),
            ("Q4".to_string(), true)
        ],
        "Q2 and Q3 still wait, so Q4 stays deferred"
    );

    answer_at(&store, "Q2", false, 20).await;
    answer_at(&store, "Q3", false, 20).await;
    let second = store
        .start_batch(&at_v1(&["Q2", "Q3"]), "ANSWERS 2", 30)
        .await
        .unwrap()
        .unwrap();
    store.batch_typed(second.id, 31).await.unwrap();
    assert_eq!(listed_ids(&store).await, [("Q4".to_string(), false)]);
}

#[tokio::test]
async fn defer_that_leaves_nothing_waiting_and_nothing_answered_returns_the_deferred() {
    let temp = TempFolder::new();
    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    for id in ["Q1", "Q2", "Q3"] {
        store.put(question(id), 0).await.unwrap();
    }
    store.defer("Q1", 1).await.unwrap().unwrap();
    store.defer("Q2", 2).await.unwrap().unwrap();
    assert_eq!(
        listed_ids(&store).await,
        [
            ("Q3".to_string(), false),
            ("Q1".to_string(), true),
            ("Q2".to_string(), true)
        ]
    );
    store.defer("Q3", 3).await.unwrap().unwrap();
    assert_eq!(
        listed_ids(&store).await,
        [
            ("Q1".to_string(), false),
            ("Q2".to_string(), false),
            ("Q3".to_string(), false)
        ]
    );
}

#[tokio::test]
async fn a_defer_with_an_answer_in_the_way_leaves_the_deferred_deferred() {
    let temp = TempFolder::new();
    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    for id in ["Q1", "Q2", "Q3"] {
        store.put(question(id), 0).await.unwrap();
    }
    store.defer("Q1", 1).await.unwrap().unwrap();
    answer_at(&store, "Q2", false, 2).await;
    // Q2 answered and unsent.
    store.defer("Q3", 3).await.unwrap().unwrap();
    assert_eq!(
        listed_ids(&store).await,
        [("Q1".to_string(), true), ("Q3".to_string(), true)]
    );

    // Q2 in a started batch that is not typed yet.
    store
        .start_batch(&at_v1(&["Q2"]), "ANSWERS 1", 10)
        .await
        .unwrap()
        .unwrap();
    store.put(question("Q4"), 11).await.unwrap();
    store.defer("Q4", 12).await.unwrap().unwrap();
    assert_eq!(
        listed_ids(&store).await,
        [
            ("Q1".to_string(), true),
            ("Q3".to_string(), true),
            ("Q4".to_string(), true)
        ]
    );
}

fn step(text: &str) -> crate::tools::questions::schema::Step {
    crate::tools::questions::schema::Step {
        text: text.into(),
        detail: String::new(),
    }
}

fn instructions(id: &str, steps: &[&str]) -> Question {
    Question {
        id: id.into(),
        product: "p".into(),
        spec: String::new(),
        sha: String::new(),
        link: String::new(),
        title: "Do things".into(),
        steps: steps.iter().map(|s| step(s)).collect(),
        intro: String::new(),
        decisions: vec![],
    }
}

async fn ticked_of(store: &TursoQuestionStore, id: &str) -> Vec<u32> {
    store
        .list(0)
        .await
        .unwrap()
        .questions
        .into_iter()
        .find(|l| l.question.id == id)
        .expect("listed")
        .ticked
}

async fn tick_ok(store: &TursoQuestionStore, id: &str, step: u32, ticked: bool) {
    let result = store.tick(id, 1, step, ticked, 50).await.unwrap();
    assert_eq!(result, Ok(()));
}

#[tokio::test]
async fn migration_003_adds_ticks_to_a_database_with_rows() {
    let temp = TempFolder::new();
    std::fs::create_dir_all(&temp.0).unwrap();
    let database = turso::Builder::new_local(temp.0.join(DATABASE).to_str().unwrap())
        .build()
        .await
        .unwrap();
    let conn = database.connect().unwrap();
    conn.execute(
        "CREATE TABLE _migrations (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE, \
         checksum TEXT NOT NULL, applied_at_ms INTEGER NOT NULL)",
        (),
    )
    .await
    .unwrap();
    for (name, sql) in &MIGRATIONS[..2] {
        conn.execute_batch(sql).await.unwrap();
        conn.execute(
            "INSERT INTO _migrations (name, checksum, applied_at_ms) VALUES (?1, ?2, 0)",
            (*name, hex::encode(Sha256::digest(sql.as_bytes()))),
        )
        .await
        .unwrap();
    }
    conn.execute(
        "INSERT INTO questions (id, version, position, state, body, updated_at_ms) \
         VALUES ('Q1', 1, 1, 'waiting', ?1, 0)",
        [serde_json::to_string(&question("Q1")).unwrap()],
    )
    .await
    .unwrap();
    drop(conn);
    drop(database);

    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    assert!(ticked_of(&store, "Q1").await.is_empty());
    drop(store);
    assert_eq!(
        migration_rows(&temp.0).await,
        [
            "001_questions",
            "002_answer_send_now",
            "003_step_ticks",
            "004_saved_picks"
        ]
    );
}

#[tokio::test]
async fn put_instructions_lists_with_no_ticks() {
    let temp = TempFolder::new();
    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    assert_eq!(
        store.put(instructions("I1", &["a", "b"]), 1).await.unwrap(),
        1
    );
    let listing = store.list(2).await.unwrap();
    assert_eq!(listing.questions.len(), 1);
    assert_eq!(listing.questions[0].question.steps.len(), 2);
    assert!(listing.questions[0].ticked.is_empty());
}

#[tokio::test]
async fn tick_and_untick_are_stored_and_listed_sorted() {
    let temp = TempFolder::new();
    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    store
        .put(instructions("I1", &["a", "b", "c"]), 1)
        .await
        .unwrap();
    tick_ok(&store, "I1", 3, true).await;
    tick_ok(&store, "I1", 1, true).await;
    tick_ok(&store, "I1", 1, true).await;
    assert_eq!(ticked_of(&store, "I1").await, [1, 3]);
    tick_ok(&store, "I1", 3, false).await;
    tick_ok(&store, "I1", 2, false).await;
    assert_eq!(ticked_of(&store, "I1").await, [1]);
}

#[tokio::test]
async fn tick_refuses_step_zero_step_past_the_end_a_question_a_stale_version_and_an_answered_row() {
    let temp = TempFolder::new();
    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    store.put(instructions("I1", &["a", "b"]), 1).await.unwrap();
    store.put(question("Q1"), 1).await.unwrap();
    assert_eq!(
        store.tick("I1", 1, 0, true, 2).await.unwrap(),
        Err(TickRefusal::InvalidStep)
    );
    assert_eq!(
        store.tick("I1", 1, 3, true, 2).await.unwrap(),
        Err(TickRefusal::InvalidStep)
    );
    assert_eq!(
        store.tick("Q1", 1, 1, true, 2).await.unwrap(),
        Err(TickRefusal::InvalidStep)
    );
    assert_eq!(
        store.tick("I1", 2, 1, true, 2).await.unwrap(),
        Err(TickRefusal::VersionChanged)
    );
    assert_eq!(
        store.tick("nope", 1, 1, true, 2).await.unwrap(),
        Err(TickRefusal::NotWaiting)
    );
    let done = |_: &Question| {
        Ok(Answer {
            ticked: 0,
            kind: Kind::Ignore,
            picks: vec![],
            note: String::new(),
        })
    };
    store
        .answer("I1", 1, false, done, 3)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        store.tick("I1", 1, 1, true, 4).await.unwrap(),
        Err(TickRefusal::NotWaiting)
    );
    assert!(ticked_of_all_empty(&store).await);
}

async fn ticked_of_all_empty(store: &TursoQuestionStore) -> bool {
    store
        .list(0)
        .await
        .unwrap()
        .questions
        .iter()
        .all(|l| l.ticked.is_empty())
}

#[tokio::test]
async fn a_new_version_keeps_ticks_for_unchanged_step_text() {
    let temp = TempFolder::new();
    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    store
        .put(instructions("I1", &["a", "b", "c"]), 1)
        .await
        .unwrap();
    tick_ok(&store, "I1", 1, true).await;
    tick_ok(&store, "I1", 2, true).await;
    // New first step, step b changed: old "a" is now step 2; "c" was not ticked.
    assert_eq!(
        store
            .put(instructions("I1", &["z", "a", "b2", "c"]), 5)
            .await
            .unwrap(),
        2
    );
    assert_eq!(ticked_of(&store, "I1").await, [2]);
}

#[tokio::test]
async fn a_repeated_step_text_uses_each_old_tick_once() {
    let temp = TempFolder::new();
    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    store.put(instructions("I1", &["a", "a"]), 1).await.unwrap();
    tick_ok(&store, "I1", 2, true).await;
    store
        .put(instructions("I1", &["a", "a", "a"]), 5)
        .await
        .unwrap();
    assert_eq!(ticked_of(&store, "I1").await, [1]);
}

#[tokio::test]
async fn a_new_version_with_fewer_steps_drops_ticks_past_the_end() {
    let temp = TempFolder::new();
    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    store
        .put(instructions("I1", &["a", "b", "c"]), 1)
        .await
        .unwrap();
    for n in 1..=3 {
        tick_ok(&store, "I1", n, true).await;
    }
    store.put(instructions("I1", &["a", "b"]), 5).await.unwrap();
    assert_eq!(ticked_of(&store, "I1").await, [1, 2]);
}

#[tokio::test]
async fn done_answer_records_the_ticked_count() {
    let temp = TempFolder::new();
    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    store
        .put(instructions("I1", &["a", "b", "c"]), 1)
        .await
        .unwrap();
    tick_ok(&store, "I1", 1, true).await;
    tick_ok(&store, "I1", 3, true).await;
    let done = |_: &Question| {
        Ok(Answer {
            ticked: 0,
            kind: Kind::Done,
            picks: vec![],
            note: String::new(),
        })
    };
    store
        .answer("I1", 1, false, done, 9)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let ready = ready_of(&store).await.expect("ready");
    assert_eq!(ready.answers[0].3.kind, Kind::Done);
    assert_eq!(ready.answers[0].3.ticked, 2);
}

#[tokio::test]
async fn ignore_answer_is_stored_and_leaves_the_list() {
    let temp = TempFolder::new();
    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    store.put(instructions("I1", &["a"]), 1).await.unwrap();
    let ignore = |_: &Question| {
        Ok(Answer {
            ticked: 0,
            kind: Kind::Ignore,
            picks: vec![],
            note: String::new(),
        })
    };
    store
        .answer("I1", 1, false, ignore, 9)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(listed_ids(&store).await.is_empty());
    let ready = ready_of(&store).await.expect("ready");
    assert_eq!(ready.answers[0].3.kind, Kind::Ignore);
    assert_eq!(ready.answers[0].3.ticked, 0);
}

#[tokio::test]
async fn ticks_carry_over_when_ignored_instructions_are_put_again() {
    let temp = TempFolder::new();
    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    store
        .put(instructions("I1", &["a", "b", "c"]), 1)
        .await
        .unwrap();
    tick_ok(&store, "I1", 1, true).await;
    tick_ok(&store, "I1", 2, true).await;
    let ignore = |_: &Question| {
        Ok(Answer {
            ticked: 0,
            kind: Kind::Ignore,
            picks: vec![],
            note: String::new(),
        })
    };
    store
        .answer("I1", 1, false, ignore, 9)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(listed_ids(&store).await.is_empty());
    // Step "a" is unchanged, "b" changed, "c" was never ticked.
    store
        .put(instructions("I1", &["a", "b2", "c"]), 10)
        .await
        .unwrap();
    assert_eq!(listed_ids(&store).await.len(), 1);
    assert_eq!(ticked_of(&store, "I1").await, [1]);
}

#[tokio::test]
async fn reorder_mixes_questions_and_instructions() {
    let temp = TempFolder::new();
    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    store.put(question("Q1"), 1).await.unwrap();
    store.put(instructions("I1", &["a"]), 1).await.unwrap();
    store.put(question("Q2"), 1).await.unwrap();
    store.reorder(&ids(&["I1", "Q2", "Q1"]), 2).await.unwrap();
    assert_eq!(
        listed_ids(&store).await,
        [
            ("I1".to_string(), false),
            ("Q2".to_string(), false),
            ("Q1".to_string(), false)
        ]
    );
}

#[tokio::test]
async fn a_waiting_instruction_does_not_hold_back_deferred_questions() {
    let temp = TempFolder::new();
    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    store.put(question("Q1"), 1).await.unwrap();
    store.put(instructions("I1", &["a"]), 1).await.unwrap();
    store.defer("Q1", 2).await.unwrap().unwrap();
    assert_eq!(
        listed_ids(&store).await,
        [("I1".to_string(), false), ("Q1".to_string(), false)]
    );
}

#[tokio::test]
async fn defer_refuses_instructions() {
    let temp = TempFolder::new();
    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    store.put(instructions("I1", &["a"]), 1).await.unwrap();
    assert_eq!(
        store.defer("I1", 2).await.unwrap(),
        Err(Refusal::NotAQuestion)
    );
    assert_eq!(listed_ids(&store).await, [("I1".to_string(), false)]);
}

fn choice(key: &str, label: &str, suggested: bool, free: bool) -> Choice {
    Choice {
        key: key.into(),
        label: label.into(),
        impact: format!("impact {key}"),
        suggested,
        free,
    }
}

/// A question whose decisions are `(ask, options)`; the options are `a`, `b` and free `z`.
fn asking(id: &str, asks: &[&str]) -> Question {
    let mut q = question(id);
    q.decisions = asks
        .iter()
        .map(|ask| Decision {
            ask: (*ask).into(),
            options: vec![
                choice("a", "A", false, false),
                choice("b", "B", false, false),
                choice("z", "Other", false, true),
            ],
        })
        .collect();
    q
}

fn pick(decision: u32, key: &str, text: &str) -> pb::Pick {
    pb::Pick {
        decision,
        key: key.into(),
        text: text.into(),
    }
}

fn saved(decision: u32, key: &str, text: &str) -> SavedPick {
    SavedPick {
        decision,
        key: key.into(),
        text: text.into(),
    }
}

async fn save_ok(store: &TursoQuestionStore, id: &str, version: u32, picks: &[pb::Pick]) {
    assert_eq!(
        store.save_picks(id, version, picks).await.unwrap(),
        Ok(Ok(()))
    );
}

async fn picks_of(store: &TursoQuestionStore, id: &str) -> Vec<SavedPick> {
    store.show(id).await.unwrap().expect("stored").picks
}

#[tokio::test]
async fn save_picks_replaces_picks_and_keeps_state_answer_and_updated_at() {
    let temp = TempFolder::new();
    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    store.put(asking("Q1", &["x", "y"]), 100).await.unwrap();
    save_ok(&store, "Q1", 1, &[pick(0, "a", ""), pick(1, "z", "typed")]).await;
    assert_eq!(
        picks_of(&store, "Q1").await,
        [saved(0, "a", ""), saved(1, "z", "typed")]
    );
    let listing = store.list(100).await.unwrap();
    assert_eq!(listing.questions[0].picks.len(), 2);
    save_ok(&store, "Q1", 1, &[pick(1, "b", "")]).await;
    assert_eq!(picks_of(&store, "Q1").await, [saved(1, "b", "")]);
    save_ok(&store, "Q1", 1, &[]).await;
    assert!(picks_of(&store, "Q1").await.is_empty());
    let shown = store.show("Q1").await.unwrap().unwrap();
    assert_eq!((shown.version, shown.state.as_str()), (1, "waiting"));
}

#[tokio::test]
async fn save_picks_drops_text_sent_with_a_plain_option() {
    let temp = TempFolder::new();
    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    store.put(asking("Q1", &["x"]), 1).await.unwrap();
    save_ok(&store, "Q1", 1, &[pick(0, "a", "stray text")]).await;
    assert_eq!(picks_of(&store, "Q1").await, [saved(0, "a", "")]);
}

#[tokio::test]
async fn save_picks_leaves_updated_at_alone() {
    let temp = TempFolder::new();
    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    store.put(asking("Q1", &["x"]), 100).await.unwrap();
    let before = store.ready(100, 1).await.unwrap();
    save_ok(&store, "Q1", 1, &[pick(0, "a", "")]).await;
    assert_eq!(store.ready(100, 1).await.unwrap(), before);
    let conn = store.conn.lock().await;
    let mut rows = conn
        .query("SELECT updated_at_ms FROM questions WHERE id = 'Q1'", ())
        .await
        .unwrap();
    let row = rows.next().await.unwrap().unwrap();
    assert_eq!(integer(&row, 0).unwrap(), 100);
}

#[tokio::test]
async fn save_picks_refuses_wrong_version_answered_dismissed_and_bad_picks() {
    let temp = TempFolder::new();
    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    store.put(asking("Q1", &["x", "y"]), 1).await.unwrap();
    assert_eq!(
        store.save_picks("Q1", 2, &[]).await.unwrap(),
        Err(Refusal::VersionChanged)
    );
    assert_eq!(
        store.save_picks("nope", 1, &[]).await.unwrap(),
        Err(Refusal::NotWaiting)
    );
    for bad in [
        vec![pick(0, "nope", "")],
        vec![pick(1, "a", ""), pick(0, "a", "")],
        vec![pick(5, "a", "")],
        vec![pick(0, "z", "  ")],
        vec![pick(0, "a", ""), pick(0, "b", "")],
    ] {
        assert_eq!(
            store.save_picks("Q1", 1, &bad).await.unwrap(),
            Ok(Err("picks".to_string())),
            "{bad:?}"
        );
    }
    assert!(picks_of(&store, "Q1").await.is_empty());
    store.put(instructions("I1", &["a"]), 1).await.unwrap();
    assert_eq!(
        store
            .save_picks("I1", 1, &[pick(0, "a", "")])
            .await
            .unwrap(),
        Ok(Err("picks".to_string()))
    );
    answer_at(&store, "Q1", false, 2).await;
    assert_eq!(
        store.save_picks("Q1", 1, &[]).await.unwrap(),
        Err(Refusal::NotWaiting)
    );
    store.put(asking("Q2", &["x"]), 3).await.unwrap();
    store.dismiss("Q2", "r", 4).await.unwrap().unwrap();
    assert_eq!(
        store.save_picks("Q2", 1, &[]).await.unwrap(),
        Err(Refusal::NotWaiting)
    );
}

#[tokio::test]
async fn answer_writes_its_picks_to_the_column() {
    let temp = TempFolder::new();
    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    store.put(asking("Q1", &["x", "y"]), 1).await.unwrap();
    let picks = [pick(0, "b", ""), pick(1, "z", "typed")];
    let result = store
        .answer(
            "Q1",
            1,
            false,
            |q| crate::tools::questions::schema::resolve(q, Kind::FreeText, &picks, "typed"),
            2,
        )
        .await
        .unwrap();
    assert_eq!(result, Ok(Ok(())));
    assert_eq!(
        picks_of(&store, "Q1").await,
        [saved(0, "b", ""), saved(1, "z", "typed")]
    );
}

#[tokio::test]
async fn put_again_carries_picks_of_unchanged_decisions_renumbered() {
    let temp = TempFolder::new();
    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    store.put(asking("Q1", &["x", "y"]), 1).await.unwrap();
    save_ok(&store, "Q1", 1, &[pick(0, "a", ""), pick(1, "z", "typed")]).await;
    // After a save: a new decision first, then y, then x.
    store.put(asking("Q1", &["w", "y", "x"]), 2).await.unwrap();
    assert_eq!(
        picks_of(&store, "Q1").await,
        [saved(1, "z", "typed"), saved(2, "a", "")]
    );
    // After dismissal.
    store.dismiss("Q1", "r", 3).await.unwrap().unwrap();
    assert_eq!(picks_of(&store, "Q1").await.len(), 2);
    store.put(asking("Q1", &["y"]), 4).await.unwrap();
    assert_eq!(picks_of(&store, "Q1").await, [saved(0, "z", "typed")]);
    // After an answer.
    let picks = [pick(0, "b", "")];
    store
        .answer(
            "Q1",
            3,
            false,
            |q| crate::tools::questions::schema::resolve(q, Kind::Revise, &picks, "n"),
            5,
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    store.put(asking("Q1", &["y", "y"]), 6).await.unwrap();
    assert_eq!(picks_of(&store, "Q1").await, [saved(0, "b", "")]);
}

#[tokio::test]
async fn put_again_drops_a_pick_when_ask_or_any_option_field_changed() {
    let temp = TempFolder::new();
    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    store.put(asking("Q1", &["x"]), 1).await.unwrap();
    let changes: [fn(&mut Question); 6] = [
        |q| q.decisions[0].ask = "other".into(),
        |q| q.decisions[0].options[0].label = "L".into(),
        |q| q.decisions[0].options[0].impact = "I".into(),
        |q| q.decisions[0].options[0].suggested = true,
        |q| q.decisions[0].options[2].free = false,
        |q| q.decisions[0].options[0].key = "k".into(),
    ];
    for (i, change) in changes.iter().enumerate() {
        // Each round puts the question twice: the changed one, then the original.
        let version = u32::try_from(i).unwrap() * 2 + 1;
        save_ok(&store, "Q1", version, &[pick(0, "b", "")]).await;
        let mut changed = asking("Q1", &["x"]);
        change(&mut changed);
        store.put(changed, 10).await.unwrap();
        assert!(picks_of(&store, "Q1").await.is_empty(), "change {i}");
        store.put(asking("Q1", &["x"]), 11).await.unwrap();
    }
}

#[tokio::test]
async fn a_row_with_empty_or_unparsable_picks_reads_as_none() {
    let temp = TempFolder::new();
    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    store.put(asking("Q1", &["x"]), 1).await.unwrap();
    assert!(picks_of(&store, "Q1").await.is_empty());
    {
        let conn = store.conn.lock().await;
        conn.execute(
            "UPDATE questions SET picks = 'not json' WHERE id = 'Q1'",
            (),
        )
        .await
        .unwrap();
    }
    assert!(picks_of(&store, "Q1").await.is_empty());
    assert!(store.list(1).await.unwrap().questions[0].picks.is_empty());
}

#[tokio::test]
async fn show_returns_a_dismissed_questions_picks_and_none_for_an_unknown_id() {
    let temp = TempFolder::new();
    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    store.put(asking("Q1", &["x"]), 1).await.unwrap();
    save_ok(&store, "Q1", 1, &[pick(0, "a", "")]).await;
    store.dismiss("Q1", "stale", 2).await.unwrap().unwrap();
    let shown = store.show("Q1").await.unwrap().unwrap();
    assert_eq!(
        (shown.state.as_str(), shown.reason.as_str(), shown.version),
        ("dismissed", "stale", 1)
    );
    assert_eq!(shown.picks, [saved(0, "a", "")]);
    assert!(store.show("nope").await.unwrap().is_none());
}
