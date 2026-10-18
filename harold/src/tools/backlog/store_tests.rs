use super::*;
use crate::tools::backlog::markdown::{self, BacklogImport, QueueImport};
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

async fn add(store: &TursoBacklogStore, text: &str, place: Place) -> QueueItem {
    store.add_queue_item(text, place, 1).await.unwrap().unwrap()
}

/// The Now id, and the parked ids by position.
async fn queue_shape(store: &TursoBacklogStore) -> (Option<String>, Vec<String>) {
    let listing = store.list_queue(0).await.unwrap();
    for (n, item) in listing.parked.iter().enumerate() {
        assert_eq!(item.position, n as u32 + 1, "parked positions have gaps");
    }
    (
        listing.now.map(|it| it.id),
        listing.parked.into_iter().map(|it| it.id).collect(),
    )
}

async fn now_rows(store: &TursoBacklogStore) -> i64 {
    let conn = store.conn.lock().await;
    let mut rows = conn
        .query("SELECT COUNT(*) FROM queue_items WHERE state = 'now'", ())
        .await
        .unwrap();
    let row = rows.next().await.unwrap().unwrap();
    integer(&row, 0).unwrap()
}

#[tokio::test]
async fn queue_ids_count_up_from_n1() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    let added: Vec<String> = [
        add(&store, "a", Place::End).await,
        add(&store, "b", Place::End).await,
        add(&store, "c", Place::End).await,
    ]
    .into_iter()
    .map(|it| it.id)
    .collect();
    assert_eq!(added, ids(&["N1", "N2", "N3"]));
    assert_eq!(store.done_queue_item("N3", 2).await.unwrap(), Ok(()));
    assert_eq!(add(&store, "d", Place::End).await.id, "N4");
}

#[tokio::test]
async fn queue_ids_count_past_nine_by_number() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    for n in 0..10 {
        add(&store, &format!("t{n}"), Place::End).await;
    }
    assert_eq!(add(&store, "next", Place::End).await.id, "N11");
    let (_, parked) = queue_shape(&store).await;
    assert_eq!(parked[9], "N10");
}

#[tokio::test]
async fn add_parks_at_the_end_by_default_and_at_n_when_asked() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    let first = add(&store, "first", Place::End).await;
    assert_eq!(first.state, QueueState::Parked);
    assert_eq!(first.position, 1);
    assert_eq!(first.text, "first");
    assert_eq!(first.done_at_ms, 0);
    add(&store, "second", Place::End).await;
    let at_one = add(&store, "at one", Place::At(1)).await;
    assert_eq!(at_one.position, 1);
    let at_end = add(&store, "at end", Place::At(4)).await;
    assert_eq!(at_end.position, 4);
    assert_eq!(
        queue_shape(&store).await,
        (None, ids(&["N3", "N1", "N2", "N4"]))
    );

    for outside in [0, 6] {
        assert_eq!(
            store
                .add_queue_item("x", Place::At(outside), 1)
                .await
                .unwrap(),
            Err("at".to_string())
        );
    }
    assert_eq!(queue_shape(&store).await.1.len(), 4);
    // A refused add uses no id.
    assert_eq!(add(&store, "next", Place::End).await.id, "N5");
}

#[tokio::test]
async fn now_moves_the_old_now_to_parked_position_one() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    add(&store, "a", Place::Now).await;
    add(&store, "b", Place::End).await;
    add(&store, "c", Place::End).await;
    assert_eq!(
        queue_shape(&store).await,
        (Some("N1".into()), ids(&["N2", "N3"]))
    );
    assert_eq!(
        store.set_now("N3", 2).await.unwrap(),
        Ok(("N3".to_string(), 2))
    );
    assert_eq!(
        queue_shape(&store).await,
        (Some("N3".into()), ids(&["N1", "N2"]))
    );
    let listing = store.list_queue(0).await.unwrap();
    let now = listing.now.unwrap();
    assert_eq!(now.state, QueueState::Now);
    assert_eq!(now.position, 0);
    assert_eq!(now.updated_at_ms, 2);
}

#[tokio::test]
async fn now_on_the_now_item_changes_nothing() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    add(&store, "a", Place::Now).await;
    add(&store, "b", Place::End).await;
    assert_eq!(
        store.set_now("N1", 2).await.unwrap(),
        Ok(("N1".to_string(), 1))
    );
    assert_eq!(queue_shape(&store).await, (Some("N1".into()), ids(&["N2"])));
}

#[tokio::test]
async fn adding_with_now_parks_the_old_now() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    add(&store, "a", Place::Now).await;
    add(&store, "b", Place::End).await;
    let new = add(&store, "c", Place::Now).await;
    assert_eq!(new.state, QueueState::Now);
    assert_eq!(new.position, 0);
    assert_eq!(
        queue_shape(&store).await,
        (Some("N3".into()), ids(&["N1", "N2"]))
    );
}

#[tokio::test]
async fn there_is_never_a_second_now() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    add(&store, "a", Place::Now).await;
    add(&store, "b", Place::Now).await;
    add(&store, "c", Place::End).await;
    store.set_now("N3", 2).await.unwrap().unwrap();
    add(&store, "d", Place::Now).await;
    store.set_now("N1", 3).await.unwrap().unwrap();
    store.set_now("N1", 4).await.unwrap().unwrap();
    let listing = store.list_queue(0).await.unwrap();
    assert_eq!(listing.now.map(|it| it.id), Some("N1".into()));
    assert_eq!(listing.parked.len(), 3);
    assert_eq!(now_rows(&store).await, 1);
}

#[tokio::test]
async fn the_one_now_index_refuses_a_second_now_row() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    add(&store, "a", Place::Now).await;
    let conn = store.conn.lock().await;
    let second = conn
        .execute(
            "INSERT INTO queue_items (id, state, position, text, updated_at_ms) \
             VALUES ('N99', 'now', 0, 'b', 1)",
            (),
        )
        .await;
    let error = second.expect_err("a second 'now' row was stored");
    assert!(
        error.to_string().to_uppercase().contains("UNIQUE"),
        "{error}"
    );
    drop(conn);
    assert_eq!(now_rows(&store).await, 1);
}

#[tokio::test]
async fn order_reorders_the_parked_items() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    for text in ["a", "b", "c", "d"] {
        add(&store, text, Place::End).await;
    }
    assert_eq!(
        store.order_queue(&ids(&["N4", "N2"]), 2).await.unwrap(),
        Ok(4)
    );
    assert_eq!(
        queue_shape(&store).await,
        (None, ids(&["N4", "N2", "N1", "N3"]))
    );
}

#[tokio::test]
async fn order_with_the_now_id_is_not_parked() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    add(&store, "a", Place::Now).await;
    add(&store, "b", Place::End).await;
    add(&store, "c", Place::End).await;
    store.done_queue_item("N3", 2).await.unwrap().unwrap();
    add(&store, "d", Place::End).await;
    for listed in [ids(&["N4", "N1"]), ids(&["N3"])] {
        assert_eq!(
            store.order_queue(&listed, 3).await.unwrap(),
            Err(Refusal::NotParked)
        );
    }
    assert_eq!(
        queue_shape(&store).await,
        (Some("N1".into()), ids(&["N2", "N4"]))
    );
}

#[tokio::test]
async fn done_moves_an_item_to_done_and_closes_up_the_parked_positions() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    add(&store, "a", Place::Now).await;
    for text in ["b", "c", "d"] {
        add(&store, text, Place::End).await;
    }
    assert_eq!(store.done_queue_item("N3", 5).await.unwrap(), Ok(()));
    assert_eq!(
        queue_shape(&store).await,
        (Some("N1".into()), ids(&["N2", "N4"]))
    );
    assert_eq!(store.done_queue_item("N1", 6).await.unwrap(), Ok(()));
    let listing = store.list_queue(0).await.unwrap();
    assert_eq!(listing.now, None);
    let done: Vec<(&str, QueueState, u32, i64)> = listing
        .done
        .iter()
        .map(|it| (it.id.as_str(), it.state, it.position, it.done_at_ms))
        .collect();
    assert_eq!(
        done,
        vec![
            ("N3", QueueState::Done, 0, 5),
            ("N1", QueueState::Done, 0, 6)
        ]
    );
    assert_eq!(
        store.done_queue_item("N1", 7).await.unwrap(),
        Err(Refusal::AlreadyDone)
    );
    assert_eq!(store.list_queue(0).await.unwrap().done[1].done_at_ms, 6);
}

#[tokio::test]
async fn done_since_filters_by_time() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    add(&store, "a", Place::End).await;
    add(&store, "b", Place::End).await;
    store.done_queue_item("N1", 100).await.unwrap().unwrap();
    store.done_queue_item("N2", 200).await.unwrap().unwrap();
    let since = store.list_queue(150).await.unwrap();
    assert_eq!(
        since
            .done
            .iter()
            .map(|it| it.id.as_str())
            .collect::<Vec<_>>(),
        vec!["N2"]
    );
    assert_eq!(store.list_queue(200).await.unwrap().done.len(), 1);
    assert_eq!(store.list_queue(0).await.unwrap().done.len(), 2);
}

#[tokio::test]
async fn now_on_a_done_item_is_already_done() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    add(&store, "a", Place::End).await;
    store.done_queue_item("N1", 2).await.unwrap().unwrap();
    assert_eq!(
        store.set_now("N1", 3).await.unwrap(),
        Err(Refusal::AlreadyDone)
    );
    let listing = store.list_queue(0).await.unwrap();
    assert_eq!(listing.now, None);
    assert_eq!(listing.done.len(), 1);
}

#[tokio::test]
async fn an_unknown_id_is_not_found() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    add(&store, "a", Place::End).await;
    assert_eq!(
        store.set_now("N9", 2).await.unwrap(),
        Err(Refusal::NotFound)
    );
    assert_eq!(
        store.done_queue_item("N9", 2).await.unwrap(),
        Err(Refusal::NotFound)
    );
    assert_eq!(
        store.update_queue_item("N9", "x", 2).await.unwrap(),
        Err(Refusal::NotFound)
    );
    assert_eq!(
        store.order_queue(&ids(&["N1", "N9"]), 2).await.unwrap(),
        Err(Refusal::NotFound)
    );
    assert_eq!(queue_shape(&store).await, (None, ids(&["N1"])));
}

#[tokio::test]
async fn update_replaces_a_queue_items_text() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    add(&store, "a", Place::End).await;
    assert_eq!(
        store.update_queue_item("N1", "changed", 4).await.unwrap(),
        Ok(())
    );
    let parked = store.list_queue(0).await.unwrap().parked;
    assert_eq!(parked[0].text, "changed");
    assert_eq!(parked[0].position, 1);
    assert_eq!(parked[0].updated_at_ms, 4);
}

#[tokio::test]
async fn the_queue_note_is_replaced() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    assert!(store.queue_is_empty().await.unwrap());
    assert_eq!(store.list_queue(0).await.unwrap().note, "");
    store.set_queue_note("first").await.unwrap();
    assert!(!store.queue_is_empty().await.unwrap());
    store.set_queue_note("second").await.unwrap();
    assert_eq!(store.list_queue(0).await.unwrap().note, "second");
}

#[tokio::test]
async fn a_queue_with_only_an_item_is_not_empty() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    add(&store, "a", Place::End).await;
    assert!(!store.queue_is_empty().await.unwrap());
}

const BACKLOG: &str = include_str!("fixtures/backlog.md");
const QUEUE: &str = include_str!("fixtures/queue.md");

fn backlog_import() -> BacklogImport {
    markdown::parse_backlog(BACKLOG).unwrap()
}

fn queue_import() -> QueueImport {
    markdown::parse_queue(QUEUE).unwrap()
}

#[tokio::test]
async fn importing_twice_is_not_empty_and_stores_nothing_the_second_time() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    assert_eq!(
        store.import_backlog(P, backlog_import(), 1).await.unwrap(),
        Ok(())
    );
    assert_eq!(
        store.import_backlog(P, backlog_import(), 2).await.unwrap(),
        Err(Refusal::NotEmpty)
    );
    assert_eq!(store.list_items(P).await.unwrap().len(), 5);
    let notes = store.list_notes(P, None, None).await.unwrap();
    assert_eq!(notes.len(), 12);
    assert!(notes.iter().all(|note| note.created_at_ms == 1));

    assert_eq!(
        store.import_queue(queue_import(), 5, 1).await.unwrap(),
        Ok(())
    );
    assert_eq!(
        store.import_queue(queue_import(), 6, 2).await.unwrap(),
        Err(Refusal::NotEmpty)
    );
    let listing = store.list_queue(0).await.unwrap();
    assert_eq!(listing.parked.len(), 4);
    assert_eq!(listing.done.len(), 2);
    assert!(listing.done.iter().all(|item| item.done_at_ms == 5));
}

#[tokio::test]
async fn a_product_with_a_note_refuses_an_import_and_keeps_only_that_note() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    store
        .add_note(P, NoteKind::Rule, "r", "r", 1)
        .await
        .unwrap();
    assert_eq!(
        store.import_backlog(P, backlog_import(), 2).await.unwrap(),
        Err(Refusal::NotEmpty)
    );
    assert!(store.list_items(P).await.unwrap().is_empty());
    assert_eq!(store.list_notes(P, None, None).await.unwrap().len(), 1);
    // Another product is still empty and takes the import.
    assert_eq!(
        store
            .import_backlog("other", backlog_import(), 3)
            .await
            .unwrap(),
        Ok(())
    );

    store.set_queue_note("kept").await.unwrap();
    assert_eq!(
        store.import_queue(queue_import(), 4, 4).await.unwrap(),
        Err(Refusal::NotEmpty)
    );
    let listing = store.list_queue(0).await.unwrap();
    assert_eq!((listing.now, listing.parked.len()), (None, 0));
    assert_eq!(listing.note, "kept");
}

#[tokio::test]
async fn an_import_round_trips_through_the_store() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    store
        .import_backlog(P, backlog_import(), 1)
        .await
        .unwrap()
        .unwrap();
    let items = store.list_items(P).await.unwrap();
    let notes = store.list_notes(P, None, None).await.unwrap();
    let ranks: Vec<u32> = items.iter().map(|item| item.rank).collect();
    assert_eq!(ranks, [1, 2, 3, 4, 5]);
    let export = markdown::export_backlog(P, &items, &notes);
    assert_eq!(
        markdown::content_lines(&export),
        markdown::content_lines(BACKLOG)
    );

    store
        .import_queue(queue_import(), 9, 1)
        .await
        .unwrap()
        .unwrap();
    let listing = store.list_queue(0).await.unwrap();
    let now = listing.now.as_ref().unwrap();
    assert_eq!((now.id.as_str(), now.state), ("N1", QueueState::Now));
    let positions: Vec<u32> = listing.parked.iter().map(|item| item.position).collect();
    assert_eq!(positions, [1, 2, 3, 4]);
    let export = markdown::export_queue(&listing);
    assert_eq!(
        markdown::content_lines(&export),
        markdown::content_lines(QUEUE)
    );
    // The next item added gets the next id.
    let added = store.add_queue_item("later", Place::End, 2).await.unwrap();
    assert_eq!(added.unwrap().id, "N8");
}
