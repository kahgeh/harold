use super::*;
use crate::tools::knowledge::test_support::{DIMENSIONS, FakeEmbedder, TempFolder, memory_store};
use crate::tools::knowledge::text::parse_note;

fn note(name: &str, text: &str) -> ParsedNote {
    parse_note(name, text)
}

fn put(store: &Store, name: &str, text: &str, at: i64) {
    let parsed = note(name, text);
    let vector = FakeEmbedder::vector(text);
    store
        .replace_note(
            &parsed,
            &format!("{name}.md"),
            &format!("hash-{name}"),
            at,
            &[(text.into(), vector)],
        )
        .unwrap();
}

#[test]
fn a_new_store_is_empty_and_has_no_model() {
    let folder = TempFolder::new();
    let store = memory_store(&folder);
    assert_eq!(store.counts().unwrap(), (0, 0));
    assert_eq!(store.index_info().unwrap(), None);
}

#[test]
fn replacing_a_note_keeps_one_row_and_its_new_passages() {
    let folder = TempFolder::new();
    let store = memory_store(&folder);
    put(
        &store,
        "a",
        "---\nname: A\nkind: idea\n---\nfirst words [[b]]",
        10,
    );
    put(
        &store,
        "a",
        "---\nname: A\nkind: idea\n---\nsecond words",
        20,
    );
    assert_eq!(store.counts().unwrap(), (1, 1));
    let row = store.note("a").unwrap().unwrap();
    assert_eq!(row.title, "A");
    assert_eq!(row.kind, "idea");
    assert_eq!(row.changed_at_ms, 20);
    assert_eq!(row.hash, "hash-a");
    assert!(row.body.contains("second words"));
    assert!(row.links.is_empty());
}

#[test]
fn links_come_back_as_stored() {
    let folder = TempFolder::new();
    let store = memory_store(&folder);
    put(&store, "a", "see [[b]] and [[refs/c]]", 1);
    assert_eq!(store.note("a").unwrap().unwrap().links, vec!["b", "refs/c"]);
}

#[test]
fn recent_is_newest_first_and_limited() {
    let folder = TempFolder::new();
    let store = memory_store(&folder);
    put(&store, "old", "x", 1);
    put(&store, "new", "y", 3);
    put(&store, "mid", "z", 2);
    let names: Vec<_> = store
        .recent(2)
        .unwrap()
        .into_iter()
        .map(|n| n.name)
        .collect();
    assert_eq!(names, vec!["new", "mid"]);
}

#[test]
fn nearest_orders_passages_by_cosine_distance() {
    let folder = TempFolder::new();
    let store = memory_store(&folder);
    put(&store, "fish", "fish fish river", 1);
    put(&store, "cars", "engine wheel road", 1);
    put(&store, "mixed", "river road", 1);
    let query = FakeEmbedder::vector("fish river");
    assert_eq!(query.len(), DIMENSIONS);
    let found = store.nearest(&query, 3).unwrap();
    assert_eq!(found.first().unwrap().note, "fish");
    assert_eq!(found.last().unwrap().note, "cars");
    assert!(found[0].similarity > found[1].similarity);
    assert!(found[0].similarity > 0.9, "{found:?}");
}

#[test]
fn nearest_to_a_note_leaves_the_note_out() {
    let folder = TempFolder::new();
    let store = memory_store(&folder);
    put(&store, "fish", "fish river", 1);
    put(&store, "fish2", "fish river water", 1);
    put(&store, "cars", "engine road", 1);
    let found = store.nearest_to_note("fish", 10).unwrap();
    assert!(found.iter().all(|n| n.note != "fish"));
    assert_eq!(found[0].note, "fish2");
}

#[test]
fn deleting_a_note_removes_its_passages() {
    let folder = TempFolder::new();
    let store = memory_store(&folder);
    put(&store, "a", "one", 1);
    store.delete_note("a").unwrap();
    assert_eq!(store.counts().unwrap(), (0, 0));
    assert_eq!(store.note("a").unwrap(), None);
}

#[test]
fn clear_empties_everything_and_the_model_record() {
    let folder = TempFolder::new();
    let store = memory_store(&folder);
    put(&store, "a", "one", 1);
    let info = IndexInfo {
        runtime: "r".into(),
        model: "m".into(),
        dimensions: 3,
        prompts: "p".into(),
    };
    store.set_index_info(&info, 5).unwrap();
    assert_eq!(store.index_info().unwrap(), Some(info));
    store.clear().unwrap();
    assert_eq!(store.counts().unwrap(), (0, 0));
    assert_eq!(store.index_info().unwrap(), None);
}

#[test]
fn a_quote_in_a_title_or_text_is_stored_as_it_is() {
    let folder = TempFolder::new();
    let store = memory_store(&folder);
    put(
        &store,
        "q",
        "---\nname: \"it's \\\"x\\\"\"\n---\nbody ' \" ; DROP TABLE vault_notes;",
        1,
    );
    let row = store.note("q").unwrap().unwrap();
    assert_eq!(row.title, "it's \"x\"");
    assert!(row.body.contains("DROP TABLE"));
    assert_eq!(store.counts().unwrap().0, 1);
}
