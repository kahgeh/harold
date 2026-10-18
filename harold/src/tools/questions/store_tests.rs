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
        .answer(id, version, |_| Ok(an_answer()), 5)
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
    assert_eq!(migration_rows(&folder).await, ["001_questions"]);
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
    assert_eq!(migration_rows(&temp.0).await, ["001_questions"]);
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
        .answer("Q1", 1, |_| Err("picks".to_string()), 2)
        .await
        .unwrap();
    assert_eq!(refused, Ok(Err("picks".to_string())));
    assert_eq!(listed_ids(&store).await.len(), 1);

    let result = store
        .answer(
            "Q1",
            1,
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
    let old = store.answer("Q1", 1, |_| Ok(an_answer()), 3).await.unwrap();
    assert_eq!(old, Err(Refusal::VersionChanged));
    assert!(store.ready().await.unwrap().is_none());

    assert_eq!(store.dismiss("Q1", "withdrawn", 4).await.unwrap(), Ok(2));
    let gone = store.answer("Q1", 2, |_| Ok(an_answer()), 5).await.unwrap();
    assert_eq!(gone, Err(Refusal::NotWaiting));
    let unknown = store.answer("zz", 1, |_| Ok(an_answer()), 5).await.unwrap();
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
        .start_batch(&ids(&["Q1", "Q3"]), "ANSWERS 2", 10)
        .await
        .unwrap();
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
        .start_batch(&ids(&["Q1"]), "ANSWERS 1", 10)
        .await
        .unwrap();
    drop(store);

    let store = TursoQuestionStore::open(&temp.0).await.unwrap();
    assert_eq!(store.started_batch().await.unwrap(), Some(first.clone()));
    assert!(store.ready().await.unwrap().is_none());
    store.batch_typed(first.id, 11).await.unwrap();
    assert!(store.started_batch().await.unwrap().is_none());
}
