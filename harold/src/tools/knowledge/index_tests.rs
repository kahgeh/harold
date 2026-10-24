use super::*;
use crate::tools::knowledge::embed::EmbedError;
use crate::tools::knowledge::search::{Knowledge, SearchError};
use crate::tools::knowledge::test_support::{FakeEmbedder, TempFolder, memory_store};

struct Fixture {
    vault: TempFolder,
    _data: TempFolder,
    embedder: Arc<FakeEmbedder>,
    index: Arc<Index>,
}

fn note(name: &str, kind: &str, body: &str) -> String {
    format!("---\nname: {name}\ndescription: about {name}\nkind: {kind}\n---\n\n{body}\n")
}

fn fixture(exclude: &[&str]) -> Fixture {
    let vault = TempFolder::new();
    let data = TempFolder::new();
    let embedder = FakeEmbedder::new();
    let exclude: Vec<String> = exclude.iter().map(|s| s.to_string()).collect();
    let index = Index::new(
        memory_store(&data),
        embedder.clone(),
        vault.0.clone(),
        &exclude,
    );
    Fixture {
        vault,
        _data: data,
        embedder,
        index,
    }
}

async fn counts(f: &Fixture) -> (usize, usize) {
    blocking(&f.index.store, |s| s.counts()).await.unwrap()
}

#[tokio::test]
async fn the_first_pass_indexes_every_note_but_the_excluded() {
    let f = fixture(&["MEMORY.md", "log.md"]);
    f.vault
        .write("alpha.md", &note("alpha", "idea", "rivers and fish"));
    f.vault.write(
        "references/beta.md",
        &note("beta", "reference", "engines and roads"),
    );
    f.vault.write("MEMORY.md", "index of everything");
    f.vault.write("log.md", "changes");
    f.vault.write("readme.txt", "not a note");
    f.vault.write("plain.md", "no front matter at all");
    let done = f.index.pass(false).await.unwrap();
    assert_eq!(done.notes, 3);
    assert_eq!(counts(&f).await, (3, 3));
    let names: Vec<_> = blocking(&f.index.store, |s| s.recent(10))
        .await
        .unwrap()
        .into_iter()
        .map(|n| (n.name, n.kind))
        .collect();
    assert!(names.contains(&("references/beta".into(), "reference".into())));
    assert!(names.contains(&("plain".into(), "unknown".into())));
    assert!(f.index.status().last_ok.is_some());
    assert_eq!(f.index.status().pending, 0);
}

#[tokio::test]
async fn a_second_pass_with_nothing_changed_embeds_nothing() {
    let f = fixture(&[]);
    f.vault.write("a.md", &note("a", "idea", "one"));
    f.index.pass(false).await.unwrap();
    let before = f.embedder.embedded_count();
    let again = f.index.pass(false).await.unwrap();
    assert_eq!(again, PassCounts::default());
    assert_eq!(f.embedder.embedded_count(), before);
}

#[tokio::test]
async fn a_changed_note_is_embedded_again_and_the_others_are_not() {
    let f = fixture(&[]);
    f.vault.write("a.md", &note("a", "idea", "one"));
    f.vault.write("b.md", &note("b", "idea", "two"));
    f.index.pass(false).await.unwrap();
    let before = f.embedder.embedded_count();
    f.vault
        .write("a.md", &note("a", "idea", "one and some new words"));
    let done = f.index.pass(false).await.unwrap();
    assert_eq!(done.notes, 1);
    assert_eq!(f.embedder.embedded_count(), before + 1);
    let a = blocking(&f.index.store, |s| s.note("a"))
        .await
        .unwrap()
        .unwrap();
    assert!(a.body.contains("new words"));
}

#[tokio::test]
async fn a_deleted_note_is_removed_and_a_renamed_one_is_replaced_without_history() {
    let f = fixture(&[]);
    f.vault
        .write("old-name.md", &note("old-name", "idea", "words"));
    f.vault.write("gone.md", &note("gone", "idea", "bye"));
    f.index.pass(false).await.unwrap();
    std::fs::remove_file(f.vault.0.join("gone.md")).unwrap();
    std::fs::rename(f.vault.0.join("old-name.md"), f.vault.0.join("new-name.md")).unwrap();
    let done = f.index.pass(false).await.unwrap();
    assert_eq!((done.removed, done.notes), (2, 1));
    let names: Vec<_> = blocking(&f.index.store, |s| s.recent(10))
        .await
        .unwrap()
        .into_iter()
        .map(|n| n.name)
        .collect();
    assert_eq!(names, vec!["new-name"]);
}

#[tokio::test]
async fn a_missing_vault_fails_with_vault_unreadable_and_keeps_the_index() {
    let f = fixture(&[]);
    f.vault.write("a.md", &note("a", "idea", "one"));
    f.index.pass(false).await.unwrap();
    std::fs::remove_dir_all(&f.vault.0).unwrap();
    assert_eq!(f.index.pass(false).await, Err(Failure::VaultUnreadable));
    assert_eq!(
        f.index.status().last_failure,
        Some(Failure::VaultUnreadable)
    );
    assert_eq!(counts(&f).await, (1, 1));
    let found = Knowledge::new(f.index.clone())
        .search("one", 10, "chief")
        .await
        .unwrap();
    assert_eq!(found.len(), 1);
    std::fs::create_dir_all(&f.vault.0).unwrap();
}

#[tokio::test]
async fn a_worker_that_cannot_load_fails_the_pass_and_changes_nothing() {
    let f = fixture(&[]);
    f.vault.write("a.md", &note("a", "idea", "one"));
    *f.embedder.fail.lock().unwrap() = Some(EmbedError::Unavailable("failed_to_load"));
    assert_eq!(f.index.pass(false).await, Err(Failure::WorkerFailed));
    assert_eq!(counts(&f).await, (0, 0));
    assert!(f.index.status().last_ok.is_none());
    *f.embedder.fail.lock().unwrap() = None;
    assert_eq!(f.index.pass(false).await.unwrap().notes, 1);
    assert_eq!(f.index.status().last_failure, None);
}

#[tokio::test]
async fn a_search_while_the_worker_is_unavailable_says_why() {
    let f = fixture(&[]);
    f.vault.write("a.md", &note("a", "idea", "one"));
    f.index.pass(false).await.unwrap();
    *f.embedder.fail.lock().unwrap() = Some(EmbedError::Unavailable("failed_to_load"));
    let error = Knowledge::new(f.index.clone())
        .search("one", 10, "app")
        .await
        .unwrap_err();
    assert_eq!(error, SearchError::Resting("failed_to_load"));
}

#[tokio::test]
async fn another_model_rebuilds_every_passage() {
    let f = fixture(&[]);
    f.vault.write("a.md", &note("a", "idea", "one"));
    f.vault.write("b.md", &note("b", "idea", "two"));
    f.index.pass(false).await.unwrap();
    let before = f.embedder.embedded_count();
    *f.embedder.model.lock().unwrap() = "another-model".into();
    f.vault.write("a.md", &note("a", "idea", "one changed"));
    f.index.pass(false).await.unwrap();
    assert_eq!(f.embedder.embedded_count(), before + 2);
    let info = blocking(&f.index.store, |s| s.index_info())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(info.model, "another-model");
}

#[tokio::test]
async fn another_model_alone_rebuilds_the_index_without_a_changed_file() {
    let f = fixture(&[]);
    f.vault.write("a.md", &note("a", "idea", "one"));
    f.vault.write("b.md", &note("b", "idea", "two"));
    f.index.pass(false).await.unwrap();
    let before = f.embedder.embedded_count();
    *f.embedder.model.lock().unwrap() = "another-model".into();
    f.index.pass(false).await.unwrap();
    assert_eq!(f.embedder.embedded_count(), before + 2);
    let info = blocking(&f.index.store, |s| s.index_info())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(info.model, "another-model");
}

#[tokio::test]
async fn a_rebuild_with_a_model_that_cannot_load_keeps_the_index() {
    let f = fixture(&[]);
    f.vault.write("a.md", &note("a", "idea", "one"));
    f.index.pass(false).await.unwrap();
    *f.embedder.fail.lock().unwrap() = Some(EmbedError::Unavailable("failed_to_load"));
    assert_eq!(f.index.pass(true).await, Err(Failure::WorkerFailed));
    assert_eq!(counts(&f).await, (1, 1));
}

#[tokio::test]
async fn one_vault_note_the_model_cannot_take_does_not_stop_the_others() {
    let f = fixture(&[]);
    f.vault
        .write("a-first.md", &note("a-first", "idea", "fine words"));
    f.vault
        .write("b-bad.md", &note("b-bad", "idea", "poisonous words"));
    f.vault
        .write("c-last.md", &note("c-last", "idea", "more fine words"));
    *f.embedder.poison.lock().unwrap() = Some("poisonous".into());
    assert_eq!(f.index.pass(false).await, Err(Failure::WorkerFailed));
    assert_eq!(
        counts(&f).await,
        (2, 2),
        "the notes before and after were indexed"
    );
    assert_eq!(f.index.status().last_failure, Some(Failure::WorkerFailed));
    *f.embedder.poison.lock().unwrap() = None;
    assert_eq!(f.index.pass(false).await.unwrap().notes, 1);
    assert_eq!(counts(&f).await, (3, 3));
}

#[tokio::test]
async fn rebuild_clears_and_builds_again() {
    let f = fixture(&[]);
    f.vault.write("a.md", &note("a", "idea", "one"));
    f.index.pass(false).await.unwrap();
    let before = f.embedder.embedded_count();
    let done = f.index.pass(true).await.unwrap();
    assert_eq!(done.notes, 1);
    assert_eq!(f.embedder.embedded_count(), before + 1);
}

#[tokio::test]
async fn search_finds_the_note_that_shares_the_words_and_reports_nothing_for_a_short_query() {
    let f = fixture(&[]);
    f.vault.write(
        "rivers.md",
        &note("rivers", "idea", "fish swim in the river water"),
    );
    f.vault
        .write("cars.md", &note("cars", "idea", "engine wheel road"));
    f.index.pass(false).await.unwrap();
    let knowledge = Knowledge::new(f.index.clone());
    let hits = knowledge
        .search("fish in the river", 10, "app")
        .await
        .unwrap();
    assert_eq!(hits[0].note.name, "rivers");
    assert!(knowledge.search("a", 10, "app").await.unwrap().is_empty());
}

#[tokio::test]
async fn related_marks_linked_notes_and_suggestions_leave_them_out() {
    let f = fixture(&[]);
    f.vault.write(
        "main.md",
        &note("main", "idea", "fish river water swim [[linked]]"),
    );
    f.vault.write(
        "linked.md",
        &note("linked", "idea", "fish river water swim"),
    );
    f.vault.write(
        "unlinked.md",
        &note("unlinked", "idea", "fish river water swim"),
    );
    f.vault
        .write("far.md", &note("far", "idea", "engine wheel road"));
    f.index.pass(false).await.unwrap();
    let knowledge = Knowledge::new(f.index.clone());
    let related = knowledge.related("main", 10).await.unwrap();
    assert!(related.iter().all(|h| h.note.name != "main"));
    let linked: Vec<_> = related
        .iter()
        .map(|h| (h.note.name.as_str(), h.linked))
        .collect();
    assert!(linked.contains(&("linked", true)), "{linked:?}");
    assert!(linked.contains(&("unlinked", false)), "{linked:?}");
    let suggested: Vec<_> = knowledge
        .suggest_links("main")
        .await
        .unwrap()
        .into_iter()
        .map(|h| h.note.name)
        .collect();
    assert_eq!(suggested, vec!["unlinked"]);
    assert_eq!(
        knowledge.related("nope", 10).await.unwrap_err(),
        SearchError::NotFound
    );
}

#[tokio::test]
async fn a_search_exports_its_span_with_counts_and_no_query_or_note_text() {
    use crate::telemetry::test_support as exported;
    exported::install();
    let f = fixture(&[]);
    f.vault.write(
        "rivers.md",
        &note(
            "rivers",
            "idea",
            "fish swim in the zebrafoxtrot river water",
        ),
    );
    f.index.pass(false).await.unwrap();
    let marker = format!("qq{}", uuid::Uuid::new_v4().simple());
    let query = format!("fish {marker}");
    let hits = Knowledge::new(f.index.clone())
        .search(&query, 10, "dash")
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    let span = exported::spans()
        .into_iter()
        .find(|s| {
            s.name == "harold.knowledge.search"
                && exported::span_attr(s, "source").as_deref() == Some("dash")
                && exported::span_attr(s, "results").as_deref() == Some("1")
        })
        .expect("the search span was exported");
    assert!(exported::span_attr(&span, "embed_ms").is_some());
    assert!(exported::span_attr(&span, "search_ms").is_some());
    let everything = exported::exported_text();
    assert!(!everything.contains(&marker));
    assert!(!everything.contains("zebrafoxtrot"));
}
