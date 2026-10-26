use std::sync::{Arc, Mutex};
use std::time::Duration;

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

#[tokio::test]
async fn a_change_queued_behind_a_refused_record_does_not_pass_it() {
    let rig = Rig::new();
    let writer = rig.start().await.unwrap();
    rig.graph.refuse(0);
    let (first, second) = tokio::join!(
        writer.submit(note("a"), By::Call),
        writer.submit(note("b"), By::Call)
    );
    assert_eq!(first.unwrap_err(), GraphError::Behind);
    assert_eq!(second.unwrap_err(), GraphError::Behind);
    assert_eq!(rig.data.len(), 1, "the second change was never logged");
    assert_eq!(writer.status().await.unwrap().waiting, 1);
}

#[tokio::test]
async fn a_position_that_cannot_be_read_never_makes_the_graph_behind() {
    let rig = Rig::new();
    let writer = rig.start().await.unwrap();
    with_notes(&writer, &["a"]).await;
    drop(writer);
    // A start whose position read fails is unreachable, not a refused record 0.
    rig.graph.fail_position(true);
    let error = rig.start().await.err().unwrap();
    assert_eq!(error, GraphError::TypeDbUnreachable);
    rig.graph.fail_position(false);
    let writer = rig.start().await.unwrap();
    assert!(writer.status().await.unwrap().last_refusal.is_none());
}

#[tokio::test]
async fn a_rebuild_that_cannot_read_a_log_leaves_the_writer_able_to_go_on() {
    let rig = Rig::new();
    let writer = rig.start().await.unwrap();
    with_notes(&writer, &["a"]).await;
    rig.data.fail_reads(true);
    assert_eq!(writer.rebuild().await.unwrap_err(), GraphError::LogFailed);
    rig.data.fail_reads(false);
    assert!(
        writer.rebuild().await.is_ok(),
        "not stuck in graph_rebuilding"
    );
    assert_eq!(writer.snapshot().await.unwrap().notes.len(), 1);
}

// The minute tick: paused time, the fake graph, and the log lines kept in a buffer.

#[derive(Clone, Default)]
struct Lines(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Lines {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Lines {
    type Writer = Lines;

    fn make_writer(&'a self) -> Lines {
        self.clone()
    }
}

impl Lines {
    /// Starts keeping this thread's log lines. The tests run on one thread each.
    fn keep() -> (Self, tracing::subscriber::DefaultGuard) {
        let lines = Self::default();
        let guard = tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_writer(lines.clone())
                .with_ansi(false)
                .finish(),
        );
        (lines, guard)
    }

    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }

    fn behind(&self) -> Vec<String> {
        self.text()
            .lines()
            .filter(|line| line.contains("knowledge graph behind"))
            .map(str::to_string)
            .collect()
    }

    fn too_long(&self) -> usize {
        self.behind()
            .iter()
            .filter(|line| line.contains("waiting_too_long"))
            .count()
    }
}

/// Lets the writer task see `count` minutes go by, one tick at a time.
async fn minutes(count: u32) {
    // The writer task must have run once, so that its first tick is a minute after the start.
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    for _ in 0..count {
        tokio::time::advance(Duration::from_secs(60)).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
    }
}

/// Logs a record the graph does not hold, as a record logged before a crash would wait.
async fn leave_waiting(rig: &Rig, name: &str) -> u64 {
    let value = serde_json::to_vec(&DataRecord {
        op: note(name),
        at_ms: 0,
        by: By::Call,
    })
    .unwrap();
    rig.data.append(value).await.unwrap()
}

#[tokio::test(start_paused = true)]
async fn records_that_wait_ten_minutes_log_once_for_each_spell() {
    let (lines, _guard) = Lines::keep();
    let rig = Rig::new();
    let writer = rig.start().await.unwrap();
    let first = leave_waiting(&rig, "a").await;
    // The writer sees the record at the first minute, and 9 minutes later it logs nothing.
    minutes(10).await;
    assert!(lines.behind().is_empty(), "{}", lines.text());
    minutes(1).await;
    assert_eq!(lines.too_long(), 1, "{}", lines.text());
    let line = lines.behind().remove(0);
    assert!(line.contains("WARN"), "{line}");
    assert!(line.contains("waiting=1"), "{line}");
    assert!(line.contains(&format!("sequence={first}")), "{line}");
    // The same spell does not log again.
    minutes(8).await;
    assert_eq!(lines.too_long(), 1);

    // The count goes back to 0; a new spell of 10 minutes logs again.
    writer.rebuild().await.unwrap();
    minutes(1).await;
    leave_waiting(&rig, "b").await;
    minutes(10).await;
    assert_eq!(lines.too_long(), 1);
    minutes(1).await;
    assert_eq!(lines.too_long(), 2, "{}", lines.text());
}

#[tokio::test(start_paused = true)]
async fn a_refused_record_logs_once_and_the_ten_minute_trigger_adds_no_second_line() {
    let (lines, _guard) = Lines::keep();
    let rig = Rig::new();
    let writer = rig.start().await.unwrap();
    rig.graph.refuse(0);
    assert_eq!(
        writer.submit(note("a"), By::Call).await.unwrap_err(),
        GraphError::Behind
    );
    assert_eq!(lines.behind().len(), 1);
    assert_eq!(lines.too_long(), 0);
    minutes(15).await;
    assert_eq!(lines.behind().len(), 1, "{}", lines.text());
}

#[tokio::test(start_paused = true)]
async fn nothing_is_judged_while_the_graph_is_stopped_and_the_spell_starts_again_after() {
    let (lines, _guard) = Lines::keep();
    let rig = Rig::new();
    rig.start().await.unwrap();
    leave_waiting(&rig, "a").await;
    rig.graph.set_stopped(true);
    crate::tools::knowledge::graph::metrics::PUBLISHED.with(|p| p.borrow_mut().clear());
    minutes(15).await;
    assert!(lines.behind().is_empty());
    assert!(
        crate::tools::knowledge::graph::metrics::PUBLISHED.with(|p| p.borrow().is_empty()),
        "a stopped graph publishes nothing"
    );
    // TypeDB starts again: the count is judged from now, not from before the stop.
    rig.graph.set_stopped(false);
    minutes(10).await;
    assert!(lines.behind().is_empty(), "{}", lines.text());
    minutes(1).await;
    assert_eq!(lines.too_long(), 1);
}

#[tokio::test(start_paused = true)]
async fn both_gauges_are_published_each_minute_without_a_change() {
    let rig = Rig::new();
    let writer = rig.start().await.unwrap();
    with_notes(&writer, &["a", "b"]).await;
    writer
        .submit(relation("a", "relates_to", "b"), By::Call)
        .await
        .unwrap();
    let published = &crate::tools::knowledge::graph::metrics::PUBLISHED;
    published.with(|p| p.borrow_mut().clear());
    minutes(2).await;
    assert_eq!(
        published.with(|p| p.borrow().clone()),
        [(0, Some(1)), (0, Some(1))]
    );
}

#[tokio::test(start_paused = true)]
async fn the_waiting_gauge_shows_the_catch_up_that_clears_the_records_that_waited() {
    let rig = Rig::new();
    let writer = rig.start().await.unwrap();
    with_notes(&writer, &["a"]).await;
    // TypeDB is out of reach for a rebuild, which leaves the writer with records to apply.
    rig.graph.set_unreachable(true);
    assert_eq!(
        writer.rebuild().await.unwrap_err(),
        GraphError::TypeDbUnreachable
    );
    leave_waiting(&rig, "b").await;
    rig.graph.set_unreachable(false);
    let published = &crate::tools::knowledge::graph::metrics::PUBLISHED;
    published.with(|p| p.borrow_mut().clear());
    // The note is held already, so this change logs nothing; the catch-up before it does the work.
    assert_eq!(writer.submit(note("a"), By::Index).await.unwrap(), None);
    assert!(rig.graph.snapshot().await.unwrap().notes.contains_key("b"));
    assert_eq!(published.with(|p| p.borrow().clone()), [(0, None)]);
}

#[tokio::test(start_paused = true)]
async fn a_spell_that_logged_a_refusal_stays_through_a_stop_and_a_start() {
    let (lines, _guard) = Lines::keep();
    let rig = Rig::new();
    let writer = rig.start().await.unwrap();
    rig.graph.refuse(0);
    writer.submit(note("a"), By::Call).await.unwrap_err();
    minutes(2).await;
    rig.graph.set_stopped(true);
    minutes(15).await;
    rig.graph.set_stopped(false);
    minutes(15).await;
    assert_eq!(lines.behind().len(), 1, "{}", lines.text());
}

/// The data log is lost and made again, and `names` are logged without TypeDB getting them.
async fn lose_data_log_leaving_waiting(rig: &Rig, names: &[&str]) {
    rig.data.start_again();
    for name in names {
        leave_waiting(rig, name).await;
    }
}

impl Lines {
    fn restarted(&self) -> Vec<String> {
        self.text()
            .lines()
            .filter(|line| line.contains("knowledge graph log restarted"))
            .map(str::to_string)
            .collect()
    }
}

#[tokio::test]
async fn a_data_log_that_started_again_is_applied_from_its_start_at_the_next_start() {
    let (lines, _guard) = Lines::keep();
    let rig = Rig::new();
    let writer = rig.start().await.unwrap();
    rig.data.jump_to(500);
    writer.submit(note("a"), By::Index).await.unwrap();
    drop(writer);
    lose_data_log_leaving_waiting(&rig, &["b", "c"]).await;
    let writer = rig.start().await.unwrap();
    assert_eq!(rig.graph.applied(), [500, 0, 1]);
    let snapshot = rig.graph.snapshot().await.unwrap();
    assert!(snapshot.notes.contains_key("b") && snapshot.notes.contains_key("c"));
    let status = writer.status().await.unwrap();
    assert_eq!((status.data_position, status.waiting), (Some(1), 0));
    let found = lines.restarted();
    assert_eq!(found.len(), 1, "{}", lines.text());
    assert!(found[0].contains("WARN"), "{}", found[0]);
    assert!(found[0].contains("kind=\"data\""), "{}", found[0]);
    assert!(found[0].contains("sequence=500"), "{}", found[0]);
    assert!(found[0].contains("records=2"), "{}", found[0]);
}

#[tokio::test]
async fn a_status_counts_the_records_of_a_log_that_started_again_as_waiting() {
    let rig = Rig::new();
    let writer = rig.start().await.unwrap();
    rig.data.jump_to(500);
    writer.submit(note("a"), By::Index).await.unwrap();
    lose_data_log_leaving_waiting(&rig, &["b", "c", "d"]).await;
    assert_eq!(writer.status().await.unwrap().waiting, 3);
}

#[tokio::test]
async fn a_live_change_after_the_log_started_again_applies_every_record_of_the_new_log() {
    let (lines, _guard) = Lines::keep();
    let rig = Rig::new();
    let writer = rig.start().await.unwrap();
    rig.data.jump_to(500);
    writer.submit(note("a"), By::Index).await.unwrap();
    lose_data_log_leaving_waiting(&rig, &["b", "c", "d"]).await;
    // The live change gets sequence 3, below the position, while TypeDB answers.
    let sequence = writer.submit(note("e"), By::Index).await.unwrap();
    assert_eq!(sequence, Some(3));
    assert_eq!(rig.graph.applied(), [500, 0, 1, 2, 3]);
    let snapshot = rig.graph.snapshot().await.unwrap();
    for name in ["a", "b", "c", "d", "e"] {
        assert!(snapshot.notes.contains_key(name), "{name}");
    }
    let status = writer.status().await.unwrap();
    assert_eq!((status.data_position, status.waiting), (Some(3), 0));
    let found = lines.restarted();
    assert_eq!(found.len(), 1, "{}", lines.text());
    assert!(found[0].contains("sequence=500"), "{}", found[0]);
    assert!(found[0].contains("records=4"), "{}", found[0]);
    // The next change is an ordinary one.
    assert_eq!(writer.submit(note("f"), By::Index).await.unwrap(), Some(4));
    assert_eq!(lines.restarted().len(), 1);
}

#[tokio::test]
async fn a_void_whose_sequence_is_not_above_the_last_seen_takes_the_same_path() {
    let (lines, _guard) = Lines::keep();
    let rig = Rig::new();
    let writer = rig.start().await.unwrap();
    rig.data.jump_to(500);
    writer.submit(note("a"), By::Index).await.unwrap();
    rig.graph.refuse(501);
    assert_eq!(
        writer.submit(note("b"), By::Index).await.unwrap_err(),
        GraphError::Behind
    );
    rig.data.start_again();
    leave_waiting(&rig, "c").await;
    let void_sequence = writer.void(501, "typo".into()).await.unwrap();
    assert_eq!(void_sequence, 1);
    assert!(rig.graph.snapshot().await.unwrap().notes.contains_key("c"));
    assert_eq!(lines.restarted().len(), 1, "{}", lines.text());
    assert_eq!(writer.submit(note("d"), By::Index).await.unwrap(), Some(2));
}

#[tokio::test]
async fn a_catch_up_that_stops_part_way_through_a_new_log_still_logs_the_find_once() {
    let (lines, _guard) = Lines::keep();
    let rig = Rig::new();
    let writer = rig.start().await.unwrap();
    rig.data.jump_to(500);
    writer.submit(note("a"), By::Index).await.unwrap();
    drop(writer);
    lose_data_log_leaving_waiting(&rig, &["b", "c", "d"]).await;
    rig.graph.refuse(1);
    let writer = rig.start().await.unwrap();
    assert_eq!(lines.restarted().len(), 1, "{}", lines.text());
    assert_eq!(
        writer.submit(note("e"), By::Index).await.unwrap_err(),
        GraphError::Behind
    );
    // The void runs a catch-up that applies the rest: the find is not logged a second time.
    writer.void(1, "typo".into()).await.unwrap();
    let snapshot = rig.graph.snapshot().await.unwrap();
    // Record 1 (`c`) is voided; record 2 (`d`) applies.
    assert!(!snapshot.notes.contains_key("c") && snapshot.notes.contains_key("d"));
    assert_eq!(lines.restarted().len(), 1, "{}", lines.text());
}

#[tokio::test(start_paused = true)]
async fn a_minute_tick_with_the_graph_caught_up_reads_at_most_two_records() {
    let rig = Rig::new();
    let writer = rig.start().await.unwrap();
    with_notes(&writer, &["a", "b", "c", "d"]).await;
    // The writer task must run once before the count is taken.
    minutes(1).await;
    let before = rig.schema.records_read() + rig.data.records_read();
    minutes(1).await;
    let read = rig.schema.records_read() + rig.data.records_read() - before;
    assert!((1..=2).contains(&read), "{read}");
}
