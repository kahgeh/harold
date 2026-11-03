use super::*;
use crate::tools::work::schema::Status;
use crate::tools::work::store::WorkFile;
use crate::tools::work::test_support::{TempFolder, decision, record};

async fn make_file(folder: &TempFolder, day: &str, records: Vec<Record>) {
    let files = WorkFiles::new(folder.0.clone());
    let file = WorkFile::open(&files.file_path(day)).await.unwrap();
    for rec in records {
        file.put(rec, 10).await.unwrap().unwrap();
    }
}

fn names(found: &[(String, Record)]) -> Vec<(String, String)> {
    found
        .iter()
        .map(|(file, rec)| (file.clone(), rec.name.clone()))
        .collect()
}

#[test]
fn day_of_gives_the_utc_date() {
    assert_eq!(day_of(0), "1970-01-01");
    assert_eq!(day_of(1_793_664_000_000), "2026-11-03");
    assert_eq!(day_of(1_793_750_399_999), "2026-11-03");
    assert_eq!(day_of(1_793_750_400_000), "2026-11-04");
}

#[test]
fn newest_name_is_current() {
    let folder = TempFolder::new();
    let files = WorkFiles::new(folder.0.clone());
    assert_eq!(files.current_name(), None);
    for day in ["2026-10-01", "2026-11-03", "2026-10-20"] {
        std::fs::write(files.file_path(day), b"").unwrap();
    }
    assert_eq!(files.current_name().as_deref(), Some("work-2026-11-03.db"));
    let listed: Vec<String> = files
        .list_newest_first()
        .iter()
        .map(|p| p.file_name().unwrap().to_str().unwrap().to_string())
        .collect();
    assert_eq!(
        listed,
        [
            "work-2026-11-03.db",
            "work-2026-10-20.db",
            "work-2026-10-01.db"
        ]
    );
}

#[test]
fn tmp_and_foreign_files_are_ignored() {
    let folder = TempFolder::new();
    let files = WorkFiles::new(folder.0.clone());
    std::fs::write(files.file_path("2026-10-01"), b"").unwrap();
    for other in [
        "work-2026-12-01.db.tmp",
        "work-2026-12-02.tmp",
        "work-2026-12-03.db-wal",
        "work-2026-12-04.db-shm",
        "work-2026-13-01.db",
        "work-2026-1-01.db",
        "work-abcd-ef-gh.db",
        "backlog.db",
        "notes.txt",
    ] {
        std::fs::write(folder.file(other), b"").unwrap();
    }
    std::fs::create_dir(folder.file("work-2026-12-09.db")).unwrap();
    assert_eq!(files.current_name().as_deref(), Some("work-2026-10-01.db"));
    assert_eq!(files.list_newest_first().len(), 1);
}

#[test]
fn a_missing_folder_has_no_files() {
    let folder = TempFolder::new();
    let files = WorkFiles::new(folder.file("nowhere"));
    assert_eq!(files.current_name(), None);
    assert!(files.list_newest_first().is_empty());
}

#[tokio::test]
async fn history_finds_closed_record_in_old_file() {
    let folder = TempFolder::new();
    let mut closed = record("done-1");
    closed.status = Status::CheckedByTheUser;
    closed.decisions = vec![decision("kept")];
    make_file(&folder, "2026-10-01", vec![closed, record("open-1")]).await;
    make_file(&folder, "2026-11-03", vec![record("open-1")]).await;
    let files = WorkFiles::new(folder.0.clone());
    let (file, rec, _) = files.history("done-1").await.unwrap().unwrap();
    assert_eq!(file, "work-2026-10-01.db");
    assert_eq!(rec.status, Status::CheckedByTheUser);
    assert_eq!(rec.decisions, vec![decision("kept")]);
    assert_eq!(files.history("nothing").await.unwrap(), None);
}

#[tokio::test]
async fn history_first_file_wins() {
    let folder = TempFolder::new();
    let mut old = record("same");
    old.title = "old".into();
    let mut new = record("same");
    new.title = "new".into();
    make_file(&folder, "2026-10-01", vec![old]).await;
    make_file(&folder, "2026-11-03", vec![new]).await;
    let files = WorkFiles::new(folder.0.clone());
    let (file, rec, _) = files.history("same").await.unwrap().unwrap();
    assert_eq!(file, "work-2026-11-03.db");
    assert_eq!(rec.title, "new");
}

#[tokio::test]
async fn search_reads_newest_first_and_dedupes_names() {
    let folder = TempFolder::new();
    let mut a_old = record("alpha");
    a_old.title = "needle old".into();
    let mut b_old = record("beta");
    b_old.next_step = "find the needle".into();
    let mut c_old = record("gamma");
    c_old.status = Status::CheckedByTheUser;
    c_old.decisions = vec![decision("a needle here")];
    let miss = record("delta");
    let mut a_new = record("alpha");
    a_new.title = "needle new".into();
    make_file(&folder, "2026-10-01", vec![a_old, b_old, c_old, miss]).await;
    make_file(&folder, "2026-11-03", vec![a_new]).await;
    let files = WorkFiles::new(folder.0.clone());
    let found = files.search("NEEDLE").await.unwrap();
    let new = "work-2026-11-03.db".to_string();
    let old = "work-2026-10-01.db".to_string();
    assert_eq!(
        names(&found),
        vec![
            (new, "alpha".to_string()),
            (old.clone(), "beta".to_string()),
            (old, "gamma".to_string()),
        ]
    );
    assert_eq!(found[0].1.title, "needle new");
}

#[tokio::test]
async fn search_matches_the_name() {
    let folder = TempFolder::new();
    make_file(
        &folder,
        "2026-11-03",
        vec![record("find-me"), record("other")],
    )
    .await;
    let files = WorkFiles::new(folder.0.clone());
    let found = files.search("find-").await.unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].1.name, "find-me");
}

#[tokio::test]
async fn search_word_with_quote_and_percent_is_plain_text() {
    let folder = TempFolder::new();
    let mut quoted = record("quoted");
    quoted.title = "it's 100% done".into();
    let mut plain = record("plain");
    plain.title = "it is 1000 done".into();
    make_file(&folder, "2026-11-03", vec![quoted, plain]).await;
    let files = WorkFiles::new(folder.0.clone());
    let found = files.search("'s 100%").await.unwrap();
    assert_eq!(names(&found).len(), 1);
    assert_eq!(found[0].1.name, "quoted");
    assert_eq!(files.search("%").await.unwrap().len(), 1);
    assert_eq!(files.search("_").await.unwrap().len(), 0);
    assert_eq!(
        files
            .search("'; DROP TABLE records; --")
            .await
            .unwrap()
            .len(),
        0
    );
}
