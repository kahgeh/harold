use super::*;
use crate::tools::backlog::test_support::{TempFolder, item};

const P: &str = "harold";

async fn open_store(folder: &TempFolder) -> TursoBacklogStore {
    TursoBacklogStore::open(&folder.0).await.unwrap()
}

fn ids(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

async fn listed_ids(store: &TursoBacklogStore, product: &str) -> Vec<String> {
    store
        .list_items(product)
        .await
        .unwrap()
        .into_iter()
        .map(|it| it.id)
        .collect()
}

async fn put_all(store: &TursoBacklogStore, product: &str, items: &[&str]) {
    for id in items {
        store.put_item(product, item(id), 1).await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn open_creates_and_migrates_backlog_db() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    assert!(folder.0.join("backlog.db").exists());
    put_all(&store, P, &["a"]).await;
    drop(store);
    let store = open_store(&folder).await;
    assert_eq!(listed_ids(&store, P).await, ids(&["a"]));
}

#[tokio::test]
async fn a_sixth_item_is_full_and_nothing_is_stored() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    for (n, id) in ["a", "b", "c", "d", "e"].into_iter().enumerate() {
        let rank = store.put_item(P, item(id), 1).await.unwrap().unwrap();
        assert_eq!(rank, n as u32 + 1);
    }
    assert_eq!(
        store.put_item(P, item("f"), 2).await.unwrap(),
        Err(Refusal::Full)
    );
    assert_eq!(listed_ids(&store, P).await, ids(&["a", "b", "c", "d", "e"]));
}

#[tokio::test]
async fn putting_the_same_id_replaces_it_and_keeps_its_rank() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    put_all(&store, P, &["a", "b", "c", "d", "e"]).await;
    let mut changed = item("b");
    changed.title = "New title".into();
    changed.evidence = vec!["abc123".into(), "harold.log:9".into()];
    assert_eq!(store.put_item(P, changed, 7).await.unwrap(), Ok(2));
    let listed = store.list_items(P).await.unwrap();
    assert_eq!(listed.len(), 5);
    let b = &listed[1];
    assert_eq!(b.id, "b");
    assert_eq!(b.rank, 2);
    assert_eq!(b.title, "New title");
    assert_eq!(b.evidence, ids(&["abc123", "harold.log:9"]));
    assert_eq!(b.updated_at_ms, 7);
}

#[tokio::test]
async fn rank_puts_the_listed_ids_first_and_the_rest_follow() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    put_all(&store, P, &["a", "b", "c", "d", "e"]).await;
    assert_eq!(
        store.rank_items(P, &ids(&["d", "b"]), 2).await.unwrap(),
        Ok(5)
    );
    let listed = store.list_items(P).await.unwrap();
    let order: Vec<(&str, u32)> = listed.iter().map(|it| (it.id.as_str(), it.rank)).collect();
    assert_eq!(
        order,
        vec![("d", 1), ("b", 2), ("a", 3), ("c", 4), ("e", 5)]
    );
}

#[tokio::test]
async fn rank_with_an_unknown_id_is_not_found_and_changes_nothing() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    put_all(&store, P, &["a", "b", "c"]).await;
    assert_eq!(
        store.rank_items(P, &ids(&["c", "zz"]), 2).await.unwrap(),
        Err(Refusal::NotFound)
    );
    assert_eq!(listed_ids(&store, P).await, ids(&["a", "b", "c"]));
}

#[tokio::test]
async fn remove_with_a_reason_adds_a_history_note() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    put_all(&store, P, &["a", "b", "c", "d", "e"]).await;
    assert_eq!(
        store
            .remove_item(P, "b", Some("done elsewhere"), 3)
            .await
            .unwrap(),
        Ok(())
    );
    let listed = store.list_items(P).await.unwrap();
    let order: Vec<(&str, u32)> = listed.iter().map(|it| (it.id.as_str(), it.rank)).collect();
    assert_eq!(order, vec![("a", 1), ("c", 2), ("d", 3), ("e", 4)]);
    let notes = store.list_notes(P, None, None).await.unwrap();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].kind, NoteKind::History);
    assert_eq!(notes[0].title, "removed b");
    assert_eq!(notes[0].body, "removed b: done elsewhere");
    assert_eq!(notes[0].created_at_ms, 3);

    assert_eq!(store.remove_item(P, "c", None, 4).await.unwrap(), Ok(()));
    assert_eq!(store.list_notes(P, None, None).await.unwrap().len(), 1);
    assert_eq!(
        store.remove_item(P, "zz", Some("x"), 5).await.unwrap(),
        Err(Refusal::NotFound)
    );
    assert_eq!(store.list_notes(P, None, None).await.unwrap().len(), 1);
}

#[tokio::test]
async fn each_note_kind_is_added_and_listed() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    let kinds = [
        NoteKind::Rule,
        NoteKind::History,
        NoteKind::Held,
        NoteKind::LeftOut,
    ];
    let mut added = Vec::new();
    for (n, kind) in kinds.into_iter().enumerate() {
        let id = store
            .add_note(P, kind, &format!("t{n}"), &format!("b{n}"), n as i64)
            .await
            .unwrap();
        added.push(id);
    }
    let notes = store.list_notes(P, None, None).await.unwrap();
    assert_eq!(notes.iter().map(|n| n.id).collect::<Vec<_>>(), added);
    assert_eq!(notes.iter().map(|n| n.kind).collect::<Vec<_>>(), kinds);
    assert_eq!(notes[2].title, "t2");
    assert_eq!(notes[2].body, "b2");
    for kind in kinds {
        let only = store.list_notes(P, Some(kind), None).await.unwrap();
        assert_eq!(only.len(), 1);
        assert_eq!(only[0].kind, kind);
    }
}

#[tokio::test]
async fn a_history_note_cannot_be_updated_or_removed() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    let id = store
        .add_note(P, NoteKind::History, "2026-10-18", "shipped", 1)
        .await
        .unwrap();
    assert_eq!(
        store.update_note(P, id, "t", "b", 2).await.unwrap(),
        Err(Refusal::AppendOnly)
    );
    assert_eq!(
        store.remove_note(P, id).await.unwrap(),
        Err(Refusal::AppendOnly)
    );
    let notes = store.list_notes(P, None, None).await.unwrap();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].body, "shipped");
}

#[tokio::test]
async fn update_and_remove_work_on_other_kinds() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    let rule = store
        .add_note(P, NoteKind::Rule, "r", "rb", 1)
        .await
        .unwrap();
    let held = store
        .add_note(P, NoteKind::Held, "h", "hb", 1)
        .await
        .unwrap();
    assert_eq!(
        store.update_note(P, rule, "r2", "rb2", 5).await.unwrap(),
        Ok(())
    );
    assert_eq!(store.remove_note(P, held).await.unwrap(), Ok(()));
    let notes = store.list_notes(P, None, None).await.unwrap();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].title, "r2");
    assert_eq!(notes[0].body, "rb2");
    assert_eq!(notes[0].created_at_ms, 1);
    assert_eq!(notes[0].updated_at_ms, 5);
    assert_eq!(
        store.remove_note(P, held).await.unwrap(),
        Err(Refusal::NotFound)
    );
    assert_eq!(
        store.update_note(P, 999, "t", "b", 6).await.unwrap(),
        Err(Refusal::NotFound)
    );
    assert_eq!(
        store.update_note("turso", rule, "t", "b", 6).await.unwrap(),
        Err(Refusal::NotFound)
    );
}

#[tokio::test]
async fn notes_last_keeps_the_newest() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    let mut added = Vec::new();
    for n in 0..5 {
        added.push(
            store
                .add_note(P, NoteKind::History, &format!("t{n}"), "b", n)
                .await
                .unwrap(),
        );
    }
    let last = store.list_notes(P, None, Some(2)).await.unwrap();
    assert_eq!(last.iter().map(|n| n.id).collect::<Vec<_>>(), added[3..]);
    let all = store.list_notes(P, None, Some(10)).await.unwrap();
    assert_eq!(all.len(), 5);
}

#[tokio::test]
async fn products_are_kept_apart() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    assert!(store.product_is_empty(P).await.unwrap());
    put_all(&store, P, &["a", "b"]).await;
    put_all(&store, "turso", &["a"]).await;
    store
        .add_note(P, NoteKind::Rule, "r", "b", 1)
        .await
        .unwrap();
    assert_eq!(listed_ids(&store, "turso").await, ids(&["a"]));
    assert_eq!(listed_ids(&store, P).await, ids(&["a", "b"]));
    assert!(
        store
            .list_notes("turso", None, None)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(!store.product_is_empty(P).await.unwrap());
    assert!(store.product_is_empty("other").await.unwrap());
    let counts = store.item_counts().await.unwrap();
    assert_eq!(
        counts.into_iter().collect::<Vec<_>>(),
        vec![(P.to_string(), 2), ("turso".to_string(), 1)]
    );
}

#[tokio::test]
async fn a_product_with_only_a_note_is_not_empty() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    store
        .add_note(P, NoteKind::LeftOut, "l", "b", 1)
        .await
        .unwrap();
    assert!(!store.product_is_empty(P).await.unwrap());
}
