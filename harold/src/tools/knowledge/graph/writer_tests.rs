use std::sync::Arc;

use super::super::fake::FakeGraph;
use super::super::log::tests::MemoryLog;
use super::super::schema::STEPS;
use super::*;

struct Rig {
    graph: Arc<FakeGraph>,
    schema: Arc<MemoryLog>,
    data: Arc<MemoryLog>,
}

impl Rig {
    fn new() -> Self {
        Self {
            graph: Arc::default(),
            schema: Arc::default(),
            data: Arc::default(),
        }
    }

    async fn start(&self) -> Result<GraphWriter, GraphError> {
        GraphWriter::start(self.graph.clone(), self.schema.clone(), self.data.clone()).await
    }
}

fn note(name: &str) -> Op {
    Op::PutVaultNote {
        name: name.into(),
        title: format!("Title of {name}"),
        kind: "lesson".into(),
    }
}

fn relation(from: &str, relation: &str, to: &str) -> Op {
    Op::PutRelation {
        from: from.into(),
        relation: relation.into(),
        to: to.into(),
    }
}

async fn with_notes(writer: &GraphWriter, names: &[&str]) {
    for name in names {
        writer.submit(note(name), By::Index).await.unwrap();
    }
}

#[tokio::test]
async fn a_new_graph_is_made_from_the_schema_log_and_the_step_is_logged_once() {
    let rig = Rig::new();
    rig.start().await.unwrap();
    assert_eq!(rig.schema.len(), STEPS.len());
    assert_eq!(rig.graph.schema_steps(), [1]);
    // A second start finds the step logged and the database there.
    rig.start().await.unwrap();
    assert_eq!(rig.schema.len(), STEPS.len());
}

#[tokio::test]
async fn check_refuses_an_unknown_type_and_a_missing_or_unknown_vault_note_without_logging() {
    let rig = Rig::new();
    let writer = rig.start().await.unwrap();
    with_notes(&writer, &["a", "b"]).await;
    let logged = rig.data.len();
    let refused = |result: Result<Option<u64>, GraphError>| result.unwrap_err();
    assert_eq!(
        refused(writer.submit(relation("a", "likes", "b"), By::Call).await),
        GraphError::InvalidRelation
    );
    assert_eq!(
        refused(
            writer
                .submit(relation("a", "part_of", "nope"), By::Call)
                .await
        ),
        GraphError::UnknownVaultNote
    );
    writer
        .submit(Op::MarkVaultNoteMissing { name: "b".into() }, By::Index)
        .await
        .unwrap();
    let logged = logged + 1;
    assert_eq!(
        refused(writer.submit(relation("a", "part_of", "b"), By::Call).await),
        GraphError::UnknownVaultNote
    );
    assert_eq!(rig.data.len(), logged);
}

#[tokio::test]
async fn a_change_is_logged_then_applied_with_the_position_in_one_step() {
    let rig = Rig::new();
    let writer = rig.start().await.unwrap();
    with_notes(&writer, &["a", "b"]).await;
    let sequence = writer
        .submit(relation("a", "inspired_by", "b"), By::Call)
        .await
        .unwrap()
        .unwrap();
    let status = writer.status().await.unwrap();
    assert_eq!(status.data_position, Some(sequence));
    assert_eq!(status.data_end, Some(sequence));
    assert_eq!(status.waiting, 0);
}

#[tokio::test]
async fn a_log_that_cannot_be_written_refuses_the_change_and_leaves_the_graph_alone() {
    let rig = Rig::new();
    let writer = rig.start().await.unwrap();
    with_notes(&writer, &["a"]).await;
    let before = rig.graph.snapshot().await.unwrap();
    let applied = rig.graph.applied();
    rig.data.fail_appends(true);
    assert_eq!(
        writer.submit(note("b"), By::Call).await.unwrap_err(),
        GraphError::LogFailed
    );
    assert_eq!(rig.graph.snapshot().await.unwrap(), before);
    assert_eq!(rig.graph.applied(), applied);
}

#[tokio::test]
async fn a_change_the_graph_already_holds_is_not_logged() {
    let rig = Rig::new();
    let writer = rig.start().await.unwrap();
    with_notes(&writer, &["a", "b"]).await;
    writer
        .submit(relation("a", "part_of", "b"), By::Call)
        .await
        .unwrap();
    let logged = rig.data.len();
    assert_eq!(writer.submit(note("a"), By::Index).await.unwrap(), None);
    assert_eq!(
        writer
            .submit(relation("a", "part_of", "b"), By::Call)
            .await
            .unwrap(),
        None
    );
    let gone = Op::RemoveRelation {
        from: "a".into(),
        relation: "supersedes".into(),
        to: "b".into(),
    };
    assert_eq!(writer.submit(gone, By::Call).await.unwrap(), None);
    assert_eq!(rig.data.len(), logged);
}

#[tokio::test]
async fn unrelate_is_allowed_when_a_vault_note_is_no_longer_in_the_graph() {
    let rig = Rig::new();
    let writer = rig.start().await.unwrap();
    with_notes(&writer, &["a", "b"]).await;
    writer
        .submit(relation("a", "supersedes", "b"), By::Call)
        .await
        .unwrap();
    writer
        .submit(Op::MarkVaultNoteMissing { name: "b".into() }, By::Index)
        .await
        .unwrap();
    let unrelate = |to: &str| Op::RemoveRelation {
        from: "a".into(),
        relation: "supersedes".into(),
        to: to.into(),
    };
    // b is marked missing: the relation goes.
    assert!(
        writer
            .submit(unrelate("b"), By::Call)
            .await
            .unwrap()
            .is_some()
    );
    // c was never there at all: nothing to remove, and no error.
    assert_eq!(writer.submit(unrelate("c"), By::Call).await.unwrap(), None);
}

#[tokio::test]
async fn a_vault_note_a_typed_relation_names_cannot_be_removed_only_marked_missing() {
    let rig = Rig::new();
    let writer = rig.start().await.unwrap();
    with_notes(&writer, &["a", "b"]).await;
    writer
        .submit(relation("a", "relates_to", "b"), By::Call)
        .await
        .unwrap();
    assert_eq!(
        writer
            .submit(Op::RemoveVaultNote { name: "b".into() }, By::Index)
            .await
            .unwrap_err(),
        GraphError::InvalidVaultNote
    );
    writer
        .submit(Op::MarkVaultNoteMissing { name: "b".into() }, By::Index)
        .await
        .unwrap();
    let rows = writer.relations_of("a").await.unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].other_missing);
    // The file comes back.
    writer.submit(note("b"), By::Index).await.unwrap();
    assert!(!writer.relations_of("a").await.unwrap()[0].other_missing);
}

#[tokio::test]
async fn the_chiefs_calls_and_a_pass_write_at_once_and_a_rebuild_gives_the_same_graph() {
    let rig = Rig::new();
    let writer = rig.start().await.unwrap();
    with_notes(&writer, &["a", "b", "c"]).await;
    let mut tasks = Vec::new();
    for i in 0..20 {
        let writer = writer.clone();
        tasks.push(tokio::spawn(async move {
            let (op, by) = if i % 2 == 0 {
                (
                    relation("a", "relates_to", if i % 4 == 0 { "b" } else { "c" }),
                    By::Call,
                )
            } else {
                (note(&format!("n{i}")), By::Index)
            };
            writer.submit(op, by).await.unwrap();
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    let live = rig.graph.snapshot().await.unwrap();
    // The position only rises in log order.
    let applied = rig.graph.applied();
    assert!(applied.windows(2).all(|w| w[0] < w[1]));
    let rebuilt = writer.rebuild().await.unwrap();
    assert_eq!(rig.graph.snapshot().await.unwrap(), live);
    assert_eq!(rebuilt.vault_notes, live.notes.len());
    assert_eq!(rebuilt.typed_relations, live.relations.len());
}

#[tokio::test]
async fn a_record_logged_and_not_applied_is_applied_once_on_restart() {
    let rig = Rig::new();
    let writer = rig.start().await.unwrap();
    with_notes(&writer, &["a"]).await;
    drop(writer);
    // Harold stopped after the log and before the transaction.
    rig.data.jump_to(1291);
    let value = serde_json::to_vec(&DataRecord {
        op: note("b"),
        at_ms: 0,
        by: By::Call,
    })
    .unwrap();
    rig.data.append(value).await.unwrap();
    let writer = rig.start().await.unwrap();
    assert_eq!(rig.graph.applied(), [0, 1291]);
    assert_eq!(writer.status().await.unwrap().waiting, 0);
    assert!(rig.graph.snapshot().await.unwrap().notes.contains_key("b"));
}

#[tokio::test]
async fn a_refused_record_makes_the_graph_behind_until_it_is_voided() {
    let rig = Rig::new();
    let writer = rig.start().await.unwrap();
    with_notes(&writer, &["a"]).await;
    drop(writer);
    for name in ["b", "c"] {
        let value = serde_json::to_vec(&DataRecord {
            op: note(name),
            at_ms: 0,
            by: By::Call,
        })
        .unwrap();
        rig.data.append(value).await.unwrap();
    }
    rig.graph.refuse(1);
    let writer = rig.start().await.unwrap();
    assert_eq!(
        writer.submit(note("d"), By::Call).await.unwrap_err(),
        GraphError::Behind
    );
    assert_eq!(
        writer.relations_of("a").await.unwrap_err(),
        GraphError::Behind
    );
    let status = writer.status().await.unwrap();
    assert_eq!(status.last_refusal.unwrap().sequence, 1);
    assert_eq!(status.waiting, 2);

    // Voiding any other sequence is refused.
    assert_eq!(
        writer.void(2, "typo".into()).await.unwrap_err(),
        GraphError::NotRefused
    );
    writer.void(1, "typo".into()).await.unwrap();
    let names: Vec<_> = writer.snapshot().await.unwrap().notes.into_keys().collect();
    assert_eq!(names, ["a", "c"]);
    assert_eq!(writer.status().await.unwrap().waiting, 0);
    writer.submit(note("d"), By::Call).await.unwrap();

    // A rebuild of a log holding the refused record and its void passes it.
    let before = rig.graph.snapshot().await.unwrap();
    let rebuilt = writer.rebuild().await.unwrap();
    assert_eq!(rebuilt.applied.voided, 1);
    assert_eq!(rig.graph.snapshot().await.unwrap(), before);
}

#[tokio::test]
async fn a_void_with_nothing_refused_is_not_refused_by_the_database() {
    let rig = Rig::new();
    let writer = rig.start().await.unwrap();
    assert_eq!(
        writer.void(0, "typo".into()).await.unwrap_err(),
        GraphError::NotRefused
    );
}

#[tokio::test]
async fn a_rebuild_after_the_database_is_lost_gives_the_same_graph_and_a_pass_logs_nothing() {
    let rig = Rig::new();
    let writer = rig.start().await.unwrap();
    with_notes(&writer, &["a", "b"]).await;
    writer
        .submit(
            Op::PutLink {
                from: "a".into(),
                to: "b".into(),
            },
            By::Index,
        )
        .await
        .unwrap();
    writer
        .submit(relation("a", "contradicts", "b"), By::Call)
        .await
        .unwrap();
    let live = rig.graph.snapshot().await.unwrap();
    drop(writer);
    rig.graph.lose_database();
    let writer = rig.start().await.unwrap();
    assert_eq!(rig.graph.snapshot().await.unwrap(), live);
    // The same pass again: nothing differs, so nothing is logged.
    let logged = rig.data.len();
    with_notes(&writer, &["a", "b"]).await;
    writer
        .submit(
            Op::PutLink {
                from: "a".into(),
                to: "b".into(),
            },
            By::Index,
        )
        .await
        .unwrap();
    assert_eq!(rig.data.len(), logged);
}

#[tokio::test]
async fn an_unreachable_server_refuses_a_change_with_typedb_unreachable() {
    let rig = Rig::new();
    let writer = rig.start().await.unwrap();
    with_notes(&writer, &["a"]).await;
    rig.graph.set_unreachable(true);
    assert_eq!(
        writer.submit(note("b"), By::Call).await.unwrap_err(),
        GraphError::TypeDbUnreachable
    );
    rig.graph.set_unreachable(false);
    writer.submit(note("b"), By::Call).await.unwrap();
}
