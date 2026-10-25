use std::sync::Arc;

use super::super::fake::FakeGraph;
use super::super::log::tests::MemoryLog;
use super::*;

fn encode(op: Op) -> Vec<u8> {
    serde_json::to_vec(&DataRecord {
        op,
        at_ms: 0,
        by: super::super::records::By::Call,
    })
    .unwrap()
}

fn note(name: &str) -> Op {
    Op::PutVaultNote {
        name: name.into(),
        title: name.into(),
        kind: "lesson".into(),
    }
}

async fn fresh() -> (Arc<FakeGraph>, MemoryLog, MemoryLog) {
    let graph = Arc::new(FakeGraph::default());
    graph.create_database().await.unwrap();
    (graph, MemoryLog::default(), MemoryLog::default())
}

#[tokio::test]
async fn catch_up_applies_every_record_after_the_position_and_none_twice_when_sequences_jump() {
    let (graph, schema, data) = fresh().await;
    data.append(encode(note("a"))).await.unwrap();
    data.jump_to(40);
    data.append(encode(note("b"))).await.unwrap();
    data.jump_to(90);
    data.append(encode(note("c"))).await.unwrap();

    let first = catch_up(&*graph, &schema, &data).await.unwrap();
    assert_eq!(first.records, 3);
    assert_eq!(graph.applied(), [0, 40, 90]);
    assert_eq!(graph.position().await.unwrap().data, Some(90));

    // Another record far above the position: only it is applied.
    data.jump_to(500);
    data.append(encode(note("d"))).await.unwrap();
    let second = catch_up(&*graph, &schema, &data).await.unwrap();
    assert_eq!(second.records, 1);
    assert_eq!(graph.applied(), [0, 40, 90, 500]);
}

#[tokio::test]
async fn applying_a_record_twice_changes_nothing_and_removing_a_missing_one_is_fine() {
    let (graph, _, _) = fresh().await;
    graph.apply(&note("a"), 0).await.unwrap();
    let once = graph.snapshot().await.unwrap();
    graph.apply(&note("a"), 0).await.unwrap();
    assert_eq!(graph.snapshot().await.unwrap(), once);
    let gone = Op::RemoveRelation {
        from: "a".into(),
        relation: "part_of".into(),
        to: "zz".into(),
    };
    graph.apply(&gone, 1).await.unwrap();
    graph
        .apply(
            &Op::RemoveLink {
                from: "a".into(),
                to: "q".into(),
            },
            2,
        )
        .await
        .unwrap();
    assert_eq!(graph.snapshot().await.unwrap(), once);
}

#[tokio::test]
async fn a_refused_record_stops_the_catch_up_at_its_sequence() {
    let (graph, schema, data) = fresh().await;
    data.append(encode(note("a"))).await.unwrap();
    data.append(encode(note("b"))).await.unwrap();
    data.append(encode(note("c"))).await.unwrap();
    graph.refuse(1);
    let error = catch_up(&*graph, &schema, &data).await.unwrap_err();
    assert_eq!(
        error,
        CatchUpError::Refused(Refusal {
            sequence: 1,
            reason: "refused".into()
        })
    );
    assert_eq!(graph.applied(), [0]);
    assert_eq!(graph.position().await.unwrap().data, Some(0));
}

#[tokio::test]
async fn a_voided_record_is_passed_and_the_rest_apply() {
    let (graph, schema, data) = fresh().await;
    data.append(encode(note("a"))).await.unwrap();
    data.append(encode(note("b"))).await.unwrap();
    data.append(encode(note("c"))).await.unwrap();
    data.append(encode(Op::VoidRecord {
        sequence: 1,
        reason: "typo".into(),
    }))
    .await
    .unwrap();
    graph.refuse(1);
    let applied = catch_up(&*graph, &schema, &data).await.unwrap();
    assert_eq!((applied.records, applied.voided), (2, 1));
    let names: Vec<_> = graph.snapshot().await.unwrap().notes.into_keys().collect();
    assert_eq!(names, ["a", "c"]);
    assert_eq!(graph.position().await.unwrap().data, Some(3));
}

#[tokio::test]
async fn a_record_that_cannot_be_decoded_stops_the_catch_up() {
    let (graph, schema, data) = fresh().await;
    data.append(b"not json".to_vec()).await.unwrap();
    let CatchUpError::Refused(refusal) = catch_up(&*graph, &schema, &data).await.unwrap_err()
    else {
        panic!("expected a refusal");
    };
    assert_eq!(
        (refusal.sequence, refusal.reason.as_str()),
        (0, "undecodable")
    );
}
