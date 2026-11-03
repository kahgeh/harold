use super::*;
use crate::tools::work::schema::{Mark, Priority, Status};
use crate::tools::work::test_support::{TempFolder, decision, record};

const FILE: &str = "work-2026-11-03.db";

async fn open_file(folder: &TempFolder) -> WorkFile {
    WorkFile::open(&folder.file(FILE)).await.unwrap()
}

async fn put(file: &WorkFile, name: &str, priority: Priority, status: Status) {
    let mut rec = record(name);
    rec.priority = priority;
    rec.status = status;
    file.put(rec, 10).await.unwrap().unwrap();
}

#[tokio::test]
async fn put_then_get_round_trips() {
    let folder = TempFolder::new();
    let file = open_file(&folder).await;
    let mut rec = record("a-1");
    rec.priority = Priority::Next;
    rec.status = Status::Building;
    rec.waits_for = "the user".into();
    rec.next_step = "write it".into();
    rec.decisions = vec![decision("one"), decision("two")];
    rec.links = vec!["spec:harold/a".into(), "note:x".into()];
    assert_eq!(file.put(rec, 100).await.unwrap(), Ok(PutOutcome::Created));
    let got = file.get("a-1").await.unwrap().unwrap();
    assert_eq!(got.title, "Title a-1");
    assert_eq!(got.priority, Priority::Next);
    assert_eq!(got.status, Status::Building);
    assert_eq!(got.waits_for, "the user");
    assert_eq!(got.next_step, "write it");
    assert_eq!(got.created_in, FILE);
    assert_eq!(
        (got.created_at_ms, got.updated_at_ms, got.status_since_ms),
        (100, 100, 100)
    );
    assert_eq!(got.decisions, vec![decision("one"), decision("two")]);
    assert_eq!(got.links.len(), 2);
    assert!(got.links.contains(&"note:x".to_string()));
    assert_eq!(file.get("none").await.unwrap(), None);
    drop(file);
    let file = open_file(&folder).await;
    assert_eq!(file.get("a-1").await.unwrap().unwrap(), got);
}

#[tokio::test]
async fn a_bad_name_is_refused() {
    let folder = TempFolder::new();
    let file = open_file(&folder).await;
    let outcome = file.put(record("Bad Name"), 1).await.unwrap();
    assert_eq!(outcome, Err(Refusal::BadName));
    assert_eq!(Refusal::BadName.reason(), "bad_name");
}

#[tokio::test]
async fn third_focus_is_refused_with_focus_full() {
    let folder = TempFolder::new();
    let file = open_file(&folder).await;
    put(&file, "a", Priority::Focus, Status::Idea).await;
    put(&file, "b", Priority::Focus, Status::Idea).await;
    // Via put.
    let mut c = record("c");
    c.priority = Priority::Focus;
    assert_eq!(file.put(c, 11).await.unwrap(), Err(Refusal::FocusFull));
    assert_eq!(file.get("c").await.unwrap(), None);
    // Via set.
    put(&file, "d", Priority::Next, Status::Idea).await;
    let patch = Patch {
        priority: Some(Priority::Focus),
        ..Patch::default()
    };
    assert_eq!(
        file.set("d", patch, 12).await.unwrap(),
        Err(Refusal::FocusFull)
    );
    assert_eq!(
        file.get("d").await.unwrap().unwrap().priority,
        Priority::Next
    );
    // Setting a focus record to focus again is not a third.
    let again = Patch {
        priority: Some(Priority::Focus),
        ..Patch::default()
    };
    assert!(file.set("a", again, 13).await.unwrap().is_ok());
    assert_eq!(Refusal::FocusFull.reason(), "focus_full");
    // A closed focus record is not counted.
    let closed = Patch {
        status: Some(Status::CheckedByTheUser),
        ..Patch::default()
    };
    file.set("b", closed, 14).await.unwrap().unwrap();
    let patch = Patch {
        priority: Some(Priority::Focus),
        ..Patch::default()
    };
    assert!(file.set("d", patch, 15).await.unwrap().is_ok());
    // A closed record in focus can also be put with a full focus.
    let mut e = record("e");
    e.priority = Priority::Focus;
    e.status = Status::CheckedByTheUser;
    assert!(file.put(e, 16).await.unwrap().is_ok());
}

#[tokio::test]
async fn name_in_use_by_open_record_is_refused() {
    let folder = TempFolder::new();
    let file = open_file(&folder).await;
    put(&file, "a", Priority::Later, Status::Idea).await;
    // A copy cannot take a name that an open record holds.
    let copy = file.get("a").await.unwrap().unwrap();
    assert_eq!(
        file.insert_copy(&copy).await.unwrap(),
        Err(Refusal::NameInUse)
    );
    // A put of the same name replaces the open record.
    let mut again = record("a");
    again.title = "New".into();
    assert_eq!(file.put(again, 20).await.unwrap(), Ok(PutOutcome::Replaced));
    assert_eq!(file.get("a").await.unwrap().unwrap().title, "New");
    // A closed record keeps its name for life: a put cannot reuse it.
    put(&file, "z", Priority::Later, Status::CheckedByTheUser).await;
    assert_eq!(
        file.put(record("z"), 21).await.unwrap(),
        Err(Refusal::NameInUse)
    );
    assert_eq!(Refusal::NameInUse.reason(), "name_in_use");
}

#[tokio::test]
async fn status_since_moves_only_when_status_changes() {
    let folder = TempFolder::new();
    let file = open_file(&folder).await;
    file.put(record("a"), 100).await.unwrap().unwrap();
    let title = Patch {
        title: Some("T2".into()),
        ..Patch::default()
    };
    assert_eq!(
        file.set("a", title, 200).await.unwrap(),
        Ok(PutOutcome::Replaced)
    );
    let got = file.get("a").await.unwrap().unwrap();
    assert_eq!((got.updated_at_ms, got.status_since_ms), (200, 100));
    let status = Patch {
        status: Some(Status::Discovery),
        ..Patch::default()
    };
    assert_eq!(
        file.set("a", status, 300).await.unwrap(),
        Ok(PutOutcome::StatusChanged {
            from: Status::Idea,
            since_ms: 100
        })
    );
    let got = file.get("a").await.unwrap().unwrap();
    assert_eq!((got.updated_at_ms, got.status_since_ms), (300, 300));
    // Put with the same status keeps it; put with another moves it.
    let mut same = record("a");
    same.status = Status::Discovery;
    assert_eq!(file.put(same, 400).await.unwrap(), Ok(PutOutcome::Replaced));
    assert_eq!(file.get("a").await.unwrap().unwrap().status_since_ms, 300);
    let mut next = record("a");
    next.status = Status::Approved;
    assert_eq!(
        file.put(next, 500).await.unwrap(),
        Ok(PutOutcome::StatusChanged {
            from: Status::Discovery,
            since_ms: 300
        })
    );
    let got = file.get("a").await.unwrap().unwrap();
    assert_eq!((got.created_at_ms, got.status_since_ms), (100, 500));
    assert_eq!(
        file.set("none", Patch::default(), 1).await.unwrap(),
        Err(Refusal::NotFound)
    );
    assert_eq!(Refusal::NotFound.reason(), "not_found");
}

#[tokio::test]
async fn decide_adds_and_mark_changes_and_no_removal() {
    let folder = TempFolder::new();
    let file = open_file(&folder).await;
    put(&file, "a", Priority::Later, Status::Idea).await;
    assert_eq!(file.decide("a", decision("one"), 20).await.unwrap(), Ok(1));
    assert_eq!(file.decide("a", decision("two"), 30).await.unwrap(), Ok(2));
    assert_eq!(
        file.decide("none", decision("x"), 30).await.unwrap(),
        Err(Refusal::NotFound)
    );
    assert_eq!(file.mark("a", 2, Mark::Agreed, 40).await.unwrap(), Ok(()));
    assert_eq!(
        file.mark("a", 3, Mark::Agreed, 40).await.unwrap(),
        Err(Refusal::NotFound)
    );
    let got = file.get("a").await.unwrap().unwrap();
    assert_eq!(got.decisions.len(), 2);
    assert_eq!(got.decisions[0].mark, Mark::Draft);
    assert_eq!(got.decisions[1].mark, Mark::Agreed);
    assert_eq!(got.updated_at_ms, 40);
    // A put keeps the decisions: none is removed.
    file.put(record("a"), 50).await.unwrap().unwrap();
    assert_eq!(file.get("a").await.unwrap().unwrap().decisions.len(), 2);
}

#[tokio::test]
async fn link_and_unlink() {
    let folder = TempFolder::new();
    let file = open_file(&folder).await;
    put(&file, "a", Priority::Later, Status::Idea).await;
    file.link("a", "question:Q1", 20).await.unwrap().unwrap();
    file.link("a", "question:Q1", 21).await.unwrap().unwrap();
    file.link("a", "note:x", 22).await.unwrap().unwrap();
    assert_eq!(file.get("a").await.unwrap().unwrap().links.len(), 2);
    file.unlink("a", "question:Q1", 30).await.unwrap().unwrap();
    assert_eq!(
        file.get("a").await.unwrap().unwrap().links,
        vec!["note:x".to_string()]
    );
    assert_eq!(
        file.unlink("a", "question:Q1", 31).await.unwrap(),
        Err(Refusal::NotFound)
    );
    assert_eq!(
        file.link("none", "note:x", 31).await.unwrap(),
        Err(Refusal::NotFound)
    );
}

#[tokio::test]
async fn read_only_file_refuses_every_write() {
    let folder = TempFolder::new();
    let path = folder.file(FILE);
    {
        let file = WorkFile::open(&path).await.unwrap();
        put(&file, "a", Priority::Later, Status::Idea).await;
        file.decide("a", decision("one"), 2).await.unwrap().unwrap();
    }
    let file = WorkFile::open_read_only(&path).await.unwrap();
    assert_eq!(
        file.put(record("b"), 3).await.unwrap().unwrap_err(),
        Refusal::ReadOnly
    );
    assert_eq!(
        file.set("a", Patch::default(), 3)
            .await
            .unwrap()
            .unwrap_err(),
        Refusal::ReadOnly
    );
    assert_eq!(
        file.decide("a", decision("x"), 3)
            .await
            .unwrap()
            .unwrap_err(),
        Refusal::ReadOnly
    );
    assert_eq!(
        file.mark("a", 1, Mark::Open, 3).await.unwrap().unwrap_err(),
        Refusal::ReadOnly
    );
    assert_eq!(
        file.link("a", "note:x", 3).await.unwrap().unwrap_err(),
        Refusal::ReadOnly
    );
    assert_eq!(
        file.unlink("a", "note:x", 3).await.unwrap().unwrap_err(),
        Refusal::ReadOnly
    );
    let copy = record("c");
    assert_eq!(
        file.insert_copy(&copy).await.unwrap().unwrap_err(),
        Refusal::ReadOnly
    );
    assert_eq!(Refusal::ReadOnly.reason(), "read_only");
    // Reads still work, and nothing changed.
    assert_eq!(file.get("a").await.unwrap().unwrap().decisions.len(), 1);
    assert_eq!(file.get("b").await.unwrap(), None);
    // A read-only open of a missing file fails and creates nothing.
    let missing = folder.file("work-2000-01-01.db");
    assert!(WorkFile::open_read_only(&missing).await.is_err());
    assert!(!missing.exists());
}

#[tokio::test]
async fn list_orders_focus_next_later() {
    let folder = TempFolder::new();
    let file = open_file(&folder).await;
    put(&file, "l1", Priority::Later, Status::Idea).await;
    put(&file, "n1", Priority::Next, Status::Idea).await;
    put(&file, "f1", Priority::Focus, Status::Idea).await;
    put(&file, "c1", Priority::Focus, Status::CheckedByTheUser).await;
    let names = |records: Vec<crate::tools::work::schema::Record>| {
        records.into_iter().map(|r| r.name).collect::<Vec<_>>()
    };
    assert_eq!(names(file.list(false).await.unwrap()), ["f1", "n1", "l1"]);
    assert_eq!(
        names(file.list(true).await.unwrap()),
        ["f1", "n1", "l1", "c1"]
    );
    assert_eq!(
        names(file.open_records().await.unwrap()),
        ["f1", "n1", "l1"]
    );
}

#[tokio::test]
async fn counts_match_content() {
    let folder = TempFolder::new();
    let file = open_file(&folder).await;
    assert_eq!(file.counts().await.unwrap(), Counts::default());
    put(&file, "a", Priority::Later, Status::Idea).await;
    put(&file, "b", Priority::Later, Status::Idea).await;
    put(&file, "c", Priority::Later, Status::CheckedByTheUser).await;
    file.decide("a", decision("1"), 2).await.unwrap().unwrap();
    file.decide("a", decision("2"), 2).await.unwrap().unwrap();
    file.decide("c", decision("3"), 2).await.unwrap().unwrap();
    file.link("b", "note:x", 2).await.unwrap().unwrap();
    file.link("c", "note:y", 2).await.unwrap().unwrap();
    // The counts cover the open records only.
    assert_eq!(
        file.counts().await.unwrap(),
        Counts {
            open_records: 2,
            decisions: 2,
            links: 1
        }
    );
}

#[tokio::test]
async fn insert_copy_keeps_every_field() {
    let folder = TempFolder::new();
    let source = open_file(&folder).await;
    let mut rec = record("a");
    rec.decisions = vec![decision("one")];
    rec.links = vec!["note:x".into()];
    source.put(rec, 100).await.unwrap().unwrap();
    source
        .set(
            "a",
            Patch {
                status: Some(Status::Building),
                ..Patch::default()
            },
            200,
        )
        .await
        .unwrap()
        .unwrap();
    let original = source.get("a").await.unwrap().unwrap();
    let target = WorkFile::open(&folder.file("work-2026-11-17.db"))
        .await
        .unwrap();
    target.insert_copy(&original).await.unwrap().unwrap();
    assert_eq!(target.get("a").await.unwrap().unwrap(), original);
    assert_eq!(
        target.counts().await.unwrap(),
        source.counts().await.unwrap()
    );
}

#[tokio::test]
async fn opening_read_only_leaves_the_bytes_unchanged() {
    let folder = TempFolder::new();
    {
        let file = open_file(&folder).await;
        put(&file, "a-1", Priority::Later, Status::Idea).await;
    }
    let path = folder.file(FILE);
    let before = std::fs::read(&path).unwrap();
    let names_before = dir_names(&folder);
    {
        let file = WorkFile::open_read_only(&path).await.unwrap();
        assert!(file.get("a-1").await.unwrap().is_some());
    }
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(dir_names(&folder), names_before);
}

fn dir_names(folder: &TempFolder) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(&folder.0)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}
