use std::sync::Arc;

use harold_api::tools::knowledge::v1 as pb;
use harold_api::tools::knowledge::v1::knowledge_service_server::KnowledgeService;
use tonic::{Code, Request};

use super::graph::fake::FakeGraph;
use super::graph::log::tests::MemoryLog;
use super::graph::records::{By, Op};
use super::graph::runtime::GraphRuntime;
use super::graph::writer::GraphWriter;
use super::service::{KnowledgeToolService, Opening};

async fn service_with_graph() -> (KnowledgeToolService, GraphWriter) {
    let writer = GraphWriter::start(
        Arc::new(FakeGraph::default()),
        Arc::new(MemoryLog::default()),
        Arc::new(MemoryLog::default()),
    )
    .await
    .unwrap();
    for name in ["a", "b"] {
        let op = Op::PutVaultNote {
            name: name.into(),
            title: name.into(),
            kind: "lesson".into(),
        };
        writer.submit(op, By::Index).await.unwrap();
    }
    let opening = Arc::new(Opening::default());
    let _ = opening
        .graph
        .set(Arc::new(GraphRuntime::with_writer(writer.clone())));
    (KnowledgeToolService::new(opening), writer)
}

fn relate(from: &str, relation: &str, to: &str) -> Request<pb::RelateRequest> {
    Request::new(pb::RelateRequest {
        from: from.into(),
        relation: relation.into(),
        to: to.into(),
    })
}

#[tokio::test]
async fn graph_calls_answer_graph_off_when_the_graph_is_off() {
    let opening = Arc::new(Opening::default());
    let _ = opening.graph.set(Arc::new(GraphRuntime::off()));
    let service = KnowledgeToolService::new(opening);
    let status = service
        .relate(relate("a", "part_of", "b"))
        .await
        .unwrap_err();
    assert_eq!(
        (status.code(), status.message()),
        (Code::Unavailable, "graph_off")
    );
    let status = service
        .graph_status(Request::new(pb::GraphStatusRequest {}))
        .await
        .unwrap()
        .into_inner();
    assert!(!status.enabled);
}

#[tokio::test]
async fn relate_then_relations_then_unrelate() {
    let (service, _) = service_with_graph().await;
    let done = service
        .relate(relate("a", "inspired_by", "b"))
        .await
        .unwrap()
        .into_inner();
    assert!(done.changed);
    let again = service
        .relate(relate("a", "inspired_by", "b"))
        .await
        .unwrap()
        .into_inner();
    assert!(!again.changed);
    let found = service
        .relations_of(Request::new(pb::RelationsOfRequest {
            vault_note: "b".into(),
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(found.relations.len(), 1);
    assert_eq!(found.relations[0].name, "a");
    assert!(!found.relations[0].outgoing);
    let removed = service
        .unrelate(relate("a", "inspired_by", "b"))
        .await
        .unwrap()
        .into_inner();
    assert!(removed.changed);
}

#[tokio::test]
async fn refusals_carry_their_codes_and_reason_words() {
    let (service, _) = service_with_graph().await;
    let invalid = service.relate(relate("a", "likes", "b")).await.unwrap_err();
    assert_eq!(
        (invalid.code(), invalid.message()),
        (Code::InvalidArgument, "invalid relation")
    );
    let unknown = service
        .relate(relate("a", "part_of", "nope"))
        .await
        .unwrap_err();
    assert_eq!(
        (unknown.code(), unknown.message()),
        (Code::NotFound, "unknown_vault_note")
    );
    let void = service
        .void_graph_record(Request::new(pb::VoidGraphRecordRequest {
            sequence: 1,
            reason: "typo".into(),
        }))
        .await
        .unwrap_err();
    assert_eq!(
        (void.code(), void.message()),
        (Code::FailedPrecondition, "not_refused")
    );
}

#[tokio::test]
async fn status_and_rebuild_report_the_graph() {
    let (service, _) = service_with_graph().await;
    service.relate(relate("a", "part_of", "b")).await.unwrap();
    let status = service
        .graph_status(Request::new(pb::GraphStatusRequest {}))
        .await
        .unwrap()
        .into_inner();
    assert!(status.enabled && status.has_data_position);
    assert_eq!(status.waiting, 0);
    assert!(!status.has_last_refusal);
    let rebuilt = service
        .rebuild_graph(Request::new(pb::RebuildGraphRequest {}))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        (
            rebuilt.schema_steps,
            rebuilt.records,
            rebuilt.vault_notes,
            rebuilt.typed_relations
        ),
        (1, 3, 2, 1)
    );
}
