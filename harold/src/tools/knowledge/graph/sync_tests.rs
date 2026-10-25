use std::sync::Arc;

use super::super::backend::GraphBackend;
use super::super::fake::FakeGraph;
use super::super::log::tests::MemoryLog;
use super::*;

struct Rig {
    graph: Arc<FakeGraph>,
    data: Arc<MemoryLog>,
    writer: GraphWriter,
}

async fn rig() -> Rig {
    let graph = Arc::new(FakeGraph::default());
    let data = Arc::new(MemoryLog::default());
    let writer = GraphWriter::start(graph.clone(), Arc::new(MemoryLog::default()), data.clone())
        .await
        .unwrap();
    Rig {
        graph,
        data,
        writer,
    }
}

fn desired(name: &str, links: &[&str]) -> Desired {
    Desired {
        name: name.into(),
        title: format!("Title {name}"),
        kind: "lesson".into(),
        links: links.iter().map(|l| l.to_string()).collect(),
    }
}

fn relate(from: &str, to: &str) -> Op {
    Op::PutRelation {
        from: from.into(),
        relation: "supersedes".into(),
        to: to.into(),
    }
}

#[tokio::test]
async fn a_pass_puts_vault_notes_and_links_and_a_second_pass_logs_nothing() {
    let rig = rig().await;
    // c is linked but not in the vault: no link is made.
    let vault = [desired("a", &["b", "c"]), desired("b", &[])];
    let first = sync_vault(&rig.writer, &vault).await.unwrap();
    assert_eq!(first.logged, 3);
    let snapshot = rig.graph.snapshot().await.unwrap();
    assert_eq!(snapshot.notes.len(), 2);
    assert_eq!(snapshot.links.len(), 1);
    let logged = rig.data.len();
    let second = sync_vault(&rig.writer, &vault).await.unwrap();
    assert_eq!(second.logged, 0);
    assert_eq!(rig.data.len(), logged);
}

#[tokio::test]
async fn a_changed_title_and_a_removed_link_are_logged() {
    let rig = rig().await;
    sync_vault(&rig.writer, &[desired("a", &["b"]), desired("b", &[])])
        .await
        .unwrap();
    let mut changed = desired("a", &[]);
    changed.title = "New title".into();
    let synced = sync_vault(&rig.writer, &[changed, desired("b", &[])])
        .await
        .unwrap();
    assert_eq!(synced.logged, 2);
    let snapshot = rig.graph.snapshot().await.unwrap();
    assert!(snapshot.links.is_empty());
    assert_eq!(snapshot.notes["a"].title, "New title");
}

#[tokio::test]
async fn a_vault_note_without_a_typed_relation_goes_with_its_links() {
    let rig = rig().await;
    sync_vault(&rig.writer, &[desired("a", &["b"]), desired("b", &[])])
        .await
        .unwrap();
    sync_vault(&rig.writer, &[desired("a", &["b"])])
        .await
        .unwrap();
    let snapshot = rig.graph.snapshot().await.unwrap();
    assert!(!snapshot.notes.contains_key("b"));
    assert!(snapshot.links.is_empty());
}

#[tokio::test]
async fn a_vault_note_a_typed_relation_names_is_kept_missing_and_comes_back() {
    let rig = rig().await;
    let both = [desired("a", &["b"]), desired("b", &[])];
    sync_vault(&rig.writer, &both).await.unwrap();
    rig.writer.submit(relate("a", "b"), By::Call).await.unwrap();

    sync_vault(&rig.writer, &[desired("a", &[])]).await.unwrap();
    let snapshot = rig.graph.snapshot().await.unwrap();
    assert!(snapshot.notes["b"].missing);
    assert!(snapshot.links.is_empty());
    assert_eq!(snapshot.relations.len(), 1);
    let rows = rig.writer.relations_of("a").await.unwrap();
    assert!(rows.iter().all(|row| row.other_missing));
    // A pass again logs nothing.
    let logged = rig.data.len();
    sync_vault(&rig.writer, &[desired("a", &[])]).await.unwrap();
    assert_eq!(rig.data.len(), logged);

    // The file comes back: the mark clears and the link returns.
    sync_vault(&rig.writer, &both).await.unwrap();
    let snapshot = rig.graph.snapshot().await.unwrap();
    assert!(!snapshot.notes["b"].missing);
    assert_eq!(snapshot.links.len(), 1);
}

#[tokio::test]
async fn a_rebuild_after_each_step_matches_the_live_graph() {
    let rig = rig().await;
    let steps: [Vec<Desired>; 3] = [
        vec![desired("a", &["b"]), desired("b", &[])],
        vec![desired("a", &[])],
        vec![desired("a", &["b"]), desired("b", &[])],
    ];
    for (index, vault) in steps.iter().enumerate() {
        sync_vault(&rig.writer, vault).await.unwrap();
        if index == 0 {
            rig.writer.submit(relate("a", "b"), By::Call).await.unwrap();
        }
        let live = rig.graph.snapshot().await.unwrap();
        rig.writer.rebuild().await.unwrap();
        assert_eq!(rig.graph.snapshot().await.unwrap(), live, "step {index}");
    }
}

#[tokio::test]
async fn a_pass_goes_past_a_vault_note_the_graph_cannot_name_and_keeps_going() {
    let rig = rig().await;
    // The title of b is too long for the graph, so b is never put, and the link to it names
    // a vault note the graph does not hold.
    let mut unnameable = desired("b", &[]);
    unnameable.title = "x".repeat(501);
    let vault = [
        desired("a", &["b", "c"]),
        unnameable,
        desired("c", &[]),
        desired("d", &[]),
    ];
    let synced = sync_vault(&rig.writer, &vault).await.unwrap();
    let snapshot = rig.graph.snapshot().await.unwrap();
    assert!(!snapshot.notes.contains_key("b"));
    assert_eq!(
        snapshot.notes.keys().cloned().collect::<Vec<_>>(),
        ["a", "c", "d"]
    );
    // The link to c comes after the refused link to b in the pass, and it is still made.
    assert_eq!(
        snapshot.links.iter().cloned().collect::<Vec<_>>(),
        [("a".to_string(), "c".to_string())]
    );
    assert_eq!(synced.logged, 4);
}
