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
        kind: Kind::Answer,
        picks: vec![Picked {
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
        ["001_questions", "002_answer_send_now"]
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
        ["001_questions", "002_answer_send_now"]
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

    let ready = store.ready().await.unwrap().unwrap();
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
    assert!(store.ready().await.unwrap().is_none());

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
async fn ready_waits_for_every_waiting_question() {
    let temp = TempFolder::new();
    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    assert!(store.ready().await.unwrap().is_none());
    store.put(question("Q1"), 1).await.unwrap();
    store.put(question("Q2"), 1).await.unwrap();
    store.put(question("Q3"), 1).await.unwrap();
    answer_ok(&store, "Q1", 1).await;
    assert!(store.ready().await.unwrap().is_none());

    store.defer("Q2", 2).await.unwrap().unwrap();
    assert!(store.ready().await.unwrap().is_none());

    answer_ok(&store, "Q3", 1).await;
    let ready = store.ready().await.unwrap().unwrap();
    let answered: Vec<&str> = ready.answers.iter().map(|a| a.0.as_str()).collect();
    assert_eq!(answered, ["Q1", "Q3"]);
    assert_eq!(ready.later, ["Q2"]);
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
    assert!(store.ready().await.unwrap().is_none());
    assert_eq!(store.started_batch().await.unwrap(), Some(batch.clone()));

    store.batch_typed(batch.id, 11).await.unwrap();
    assert!(store.started_batch().await.unwrap().is_none());
    assert_eq!(listed_ids(&store).await, [("Q2".to_string(), false)]);
    assert!(store.ready().await.unwrap().is_none());

    // sent answers are not sent again, and the returned question holds the next batch back
    answer_ok(&store, "Q2", 1).await;
    let ready = store.ready().await.unwrap().unwrap();
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
    assert!(store.ready().await.unwrap().is_none());
    store.batch_typed(first.id, 11).await.unwrap();
    assert!(store.started_batch().await.unwrap().is_none());
}

#[tokio::test]
async fn start_batch_refuses_when_a_question_waits() {
    let temp = TempFolder::new();
    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    store.put(question("Q1"), 1).await.unwrap();
    answer_ok(&store, "Q1", 1).await;
    let ready = store.ready().await.unwrap().expect("Q1 is ready");

    // Q2 lands between `ready` and `start_batch`.
    store.put(question("Q2"), 2).await.unwrap();
    let started = store
        .start_batch(&ready_answers(&ready), "ANSWERS 1", 10)
        .await
        .unwrap();
    assert_eq!(started, None);
    assert_eq!(store.started_batch().await.unwrap(), None);

    // Q1 was not claimed: once Q2 is answered, both are ready together.
    answer_ok(&store, "Q2", 1).await;
    let ready = store.ready().await.unwrap().unwrap();
    assert_eq!(ready_answers(&ready), at_v1(&["Q1", "Q2"]));
    println!(
        "evidence: start_batch with Q2 waiting -> None, no batch stored; later ready = Q1, Q2"
    );
}

#[tokio::test]
async fn start_batch_refuses_when_a_listed_answer_changed() {
    let temp = TempFolder::new();
    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    store.put(question("Q1"), 1).await.unwrap();
    store.put(question("Q2"), 1).await.unwrap();
    answer_ok(&store, "Q1", 1).await;
    answer_ok(&store, "Q2", 1).await;
    let ready = store.ready().await.unwrap().expect("Q1 and Q2 are ready");

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

    let ready = store.ready().await.unwrap().unwrap();
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
    assert!(store.ready().await.unwrap().is_none());
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

    assert!(store.release_batch(batch.id, 11).await.unwrap());

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
    let ready = store
        .ready()
        .await
        .unwrap()
        .expect("the answers are unsent again");
    assert_eq!(ready_answers(&ready), at_v1(&["Q1", "Q2"]));
    assert_eq!(ready.later, ["Q3"]);
    // A question put now holds the next batch.
    store.put(question("Q4"), 12).await.unwrap();
    assert!(store.ready().await.unwrap().is_none());
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

    assert!(!store.release_batch(batch.id, 12).await.unwrap());
    assert!(!store.release_batch(batch.id + 1, 12).await.unwrap());

    assert_eq!(batch_rows(&store).await, [(batch.id, true)]);
    assert_eq!(
        batch_columns(&store).await,
        [("Q1".to_string(), "sent".to_string(), Some(batch.id))]
    );
    assert!(store.ready().await.unwrap().is_none());
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
        ["001_questions", "002_answer_send_now"]
    );

    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    assert!(!marked(&store, "Q1").await);
    drop(store);
    assert_eq!(migration_rows(&temp.0).await.len(), 2);
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
