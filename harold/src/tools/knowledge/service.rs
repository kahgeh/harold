//! The knowledge tool's gRPC service, over the index.

use std::sync::Arc;

use harold_api::tools::knowledge::v1 as pb;
use harold_api::tools::knowledge::v1::knowledge_service_server::KnowledgeService;
use tonic::{Request, Response, Status};

use super::graph::records::{By, Op, well_formed_name};
use super::graph::runtime::GraphRuntime;
use super::graph::writer::{GraphError, GraphWriter};
use super::index::Index;
use super::search::{Hit, Knowledge, SearchError, clamp};
use super::store::NoteRow;

/// The index, once the database has opened. Until then, and while the bucket cannot be
/// reached at start-up, every call answers `Unavailable` with the reason it is not open.
#[derive(Default)]
pub(super) struct Opening {
    index: std::sync::OnceLock<Arc<Index>>,
    /// The knowledge graph: off, or started on its first call.
    pub(super) graph: std::sync::OnceLock<Arc<GraphRuntime>>,
    reason: std::sync::Mutex<&'static str>,
}

impl Opening {
    pub(super) fn opened(&self, index: Arc<Index>) {
        let _ = self.index.set(index);
    }

    pub(super) fn failed(&self, reason: &'static str) {
        *self.reason.lock().unwrap_or_else(|e| e.into_inner()) = reason;
    }
}

pub(super) struct KnowledgeToolService {
    opening: Arc<Opening>,
}

impl KnowledgeToolService {
    pub(super) fn new(opening: Arc<Opening>) -> Self {
        Self { opening }
    }

    fn index(&self) -> Result<Arc<Index>, Status> {
        self.opening.index.get().cloned().ok_or_else(|| {
            let reason = *self
                .opening
                .reason
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let reason = if reason.is_empty() {
                "store_failed"
            } else {
                reason
            };
            Status::unavailable(format!("store_failed: {reason}"))
        })
    }

    async fn graph(&self) -> Result<GraphWriter, Status> {
        let Some(runtime) = self.opening.graph.get() else {
            return Err(graph_status(GraphError::Off));
        };
        runtime.writer().await.map_err(graph_status)
    }

    fn knowledge(&self) -> Result<Knowledge, Status> {
        self.index().map(Knowledge::new)
    }
}

fn status_of(error: SearchError) -> Status {
    match error {
        SearchError::NotFound => Status::not_found("unknown_vault_note"),
        SearchError::Resting(reason) => Status::unavailable(format!("search_resting: {reason}")),
        SearchError::Store(error) => {
            Status::unavailable(format!("store_failed: {}", error.reason()))
        }
    }
}

/// A graph refusal as a gRPC status: the message is the reason word.
fn graph_status(error: GraphError) -> Status {
    let reason = error.reason();
    match error {
        GraphError::InvalidRelation | GraphError::InvalidVaultNote | GraphError::InvalidReason => {
            Status::invalid_argument(reason)
        }
        GraphError::UnknownVaultNote => Status::not_found(reason),
        GraphError::NotRefused => Status::failed_precondition(reason),
        _ => Status::unavailable(reason),
    }
}

fn hit(hit: Hit) -> pb::Hit {
    pb::Hit {
        name: hit.note.name,
        title: hit.note.title,
        kind: hit.note.kind,
        description: hit.note.description,
        passage: hit.passage,
        score: hit.score,
        linked: hit.linked,
        changed_at_ms: hit.note.changed_at_ms,
    }
}

fn recent_hit(note: NoteRow) -> pb::Hit {
    pb::Hit {
        name: note.name,
        title: note.title,
        kind: note.kind,
        description: note.description,
        passage: String::new(),
        score: 0.0,
        linked: false,
        changed_at_ms: note.changed_at_ms,
    }
}

fn invalid(field: &str) -> Status {
    Status::invalid_argument(format!("invalid {field}"))
}

fn source(source: &str) -> &str {
    match source {
        "chief" | "app" | "dash" => source,
        _ => "chief",
    }
}

#[tonic::async_trait]
impl KnowledgeService for KnowledgeToolService {
    async fn search(
        &self,
        request: Request<pb::SearchRequest>,
    ) -> Result<Response<pb::SearchResponse>, Status> {
        let request = request.into_inner();
        if request.query.len() > 2_000 {
            return Err(invalid("query"));
        }
        let hits = self
            .knowledge()?
            .search(
                &request.query,
                clamp(request.limit),
                source(&request.source),
            )
            .await
            .map_err(status_of)?;
        Ok(Response::new(pb::SearchResponse {
            hits: hits.into_iter().map(hit).collect(),
        }))
    }

    async fn recent(
        &self,
        request: Request<pb::RecentRequest>,
    ) -> Result<Response<pb::RecentResponse>, Status> {
        let notes = self
            .knowledge()?
            .recent(clamp(request.into_inner().limit))
            .await
            .map_err(status_of)?;
        Ok(Response::new(pb::RecentResponse {
            hits: notes.into_iter().map(recent_hit).collect(),
        }))
    }

    async fn related(
        &self,
        request: Request<pb::RelatedRequest>,
    ) -> Result<Response<pb::RelatedResponse>, Status> {
        let request = request.into_inner();
        if request.name.is_empty() {
            return Err(invalid("name"));
        }
        let hits = self
            .knowledge()?
            .related(&request.name, clamp(request.limit))
            .await
            .map_err(status_of)?;
        Ok(Response::new(pb::RelatedResponse {
            hits: hits.into_iter().map(hit).collect(),
        }))
    }

    async fn suggest_links(
        &self,
        request: Request<pb::SuggestLinksRequest>,
    ) -> Result<Response<pb::SuggestLinksResponse>, Status> {
        let request = request.into_inner();
        if request.name.is_empty() {
            return Err(invalid("name"));
        }
        let hits = self
            .knowledge()?
            .suggest_links(&request.name)
            .await
            .map_err(status_of)?;
        Ok(Response::new(pb::SuggestLinksResponse {
            hits: hits.into_iter().map(hit).collect(),
        }))
    }

    async fn show_vault_note(
        &self,
        request: Request<pb::ShowVaultNoteRequest>,
    ) -> Result<Response<pb::VaultNote>, Status> {
        let request = request.into_inner();
        if request.name.is_empty() {
            return Err(invalid("name"));
        }
        let note = self
            .knowledge()?
            .show(&request.name)
            .await
            .map_err(status_of)?;
        Ok(Response::new(pb::VaultNote {
            name: note.name,
            title: note.title,
            kind: note.kind,
            description: note.description,
            created: note.created,
            links: note.links,
            text: note.body,
            file: self
                .index()?
                .vault()
                .join(&note.path)
                .to_string_lossy()
                .into_owned(),
            path: note.path,
            changed_at_ms: note.changed_at_ms,
        }))
    }

    async fn status(
        &self,
        _: Request<pb::StatusRequest>,
    ) -> Result<Response<pb::StatusResponse>, Status> {
        let index = &self.index()?;
        let store = Arc::clone(&index.store);
        let (notes, passages) = super::index::blocking(&store, |s| s.counts())
            .await
            .map_err(|failure| status_of(failure.into()))?;
        let info = super::index::blocking(&store, |s| s.index_info())
            .await
            .map_err(|failure| status_of(failure.into()))?;
        let pass = index.status();
        Ok(Response::new(pb::StatusResponse {
            vault_notes: notes as u32,
            passages: passages as u32,
            last_ok_ms: if pass.last_ok.is_some() {
                pass.last_ok_ms
            } else {
                0
            },
            pending: pass.pending as u32,
            runtime: info.as_ref().map(|i| i.runtime.clone()).unwrap_or_default(),
            model: info.as_ref().map(|i| i.model.clone()).unwrap_or_default(),
            dimensions: info.as_ref().map_or(0, |i| i.dimensions as u32),
            worker: index.embedder.state().to_string(),
            last_failure: pass
                .last_failure
                .map(|f| f.reason().to_string())
                .unwrap_or_default(),
            vault_unreadable: pass.last_failure == Some(super::index::Failure::VaultUnreadable),
        }))
    }

    async fn rebuild(
        &self,
        _: Request<pb::RebuildRequest>,
    ) -> Result<Response<pb::RebuildResponse>, Status> {
        let counts = self
            .index()?
            .pass(true)
            .await
            .map_err(|failure| match failure {
                super::index::Failure::WorkerFailed => Status::unavailable("search_resting"),
                other => Status::unavailable(format!("store_failed: {}", other.reason())),
            })?;
        let store = Arc::clone(&self.index()?.store);
        let (notes, passages) = super::index::blocking(&store, |s| s.counts())
            .await
            .map_err(|failure| status_of(failure.into()))?;
        let _ = counts;
        Ok(Response::new(pb::RebuildResponse {
            vault_notes: notes as u32,
            passages: passages as u32,
        }))
    }

    async fn relate(
        &self,
        request: Request<pb::RelateRequest>,
    ) -> Result<Response<pb::RelateResponse>, Status> {
        let request = request.into_inner();
        let graph = self.graph().await?;
        let op = Op::PutRelation {
            from: request.from,
            relation: request.relation,
            to: request.to,
        };
        let changed = graph.submit(op, By::Call).await.map_err(graph_status)?;
        Ok(Response::new(pb::RelateResponse {
            changed: changed.is_some(),
        }))
    }

    async fn unrelate(
        &self,
        request: Request<pb::RelateRequest>,
    ) -> Result<Response<pb::RelateResponse>, Status> {
        let request = request.into_inner();
        let graph = self.graph().await?;
        let op = Op::RemoveRelation {
            from: request.from,
            relation: request.relation,
            to: request.to,
        };
        let changed = graph.submit(op, By::Call).await.map_err(graph_status)?;
        Ok(Response::new(pb::RelateResponse {
            changed: changed.is_some(),
        }))
    }

    async fn relations_of(
        &self,
        request: Request<pb::RelationsOfRequest>,
    ) -> Result<Response<pb::RelationsOfResponse>, Status> {
        let request = request.into_inner();
        if !well_formed_name(&request.vault_note) {
            return Err(invalid("vault_note"));
        }
        let rows = self
            .graph()
            .await?
            .relations_of(&request.vault_note)
            .await
            .map_err(graph_status)?;
        Ok(Response::new(pb::RelationsOfResponse {
            relations: rows
                .into_iter()
                .map(|row| pb::GraphRelation {
                    name: row.other,
                    title: row.other_title,
                    relation: row.relation,
                    outgoing: row.outgoing,
                    missing: row.other_missing,
                })
                .collect(),
        }))
    }

    async fn graph_status(
        &self,
        _: Request<pb::GraphStatusRequest>,
    ) -> Result<Response<pb::GraphStatusResponse>, Status> {
        let Some(runtime) = self.opening.graph.get().filter(|r| r.enabled()) else {
            return Ok(Response::new(pb::GraphStatusResponse::default()));
        };
        let graph = runtime.writer().await.map_err(graph_status)?;
        let status = graph.status().await.map_err(graph_status)?;
        let refusal = status.last_refusal;
        Ok(Response::new(pb::GraphStatusResponse {
            enabled: true,
            has_schema_position: status.schema_position.is_some(),
            schema_position: status.schema_position.unwrap_or(0),
            has_data_position: status.data_position.is_some(),
            data_position: status.data_position.unwrap_or(0),
            has_schema_end: status.schema_end.is_some(),
            schema_end: status.schema_end.unwrap_or(0),
            has_data_end: status.data_end.is_some(),
            data_end: status.data_end.unwrap_or(0),
            waiting: status.waiting as u64,
            has_last_refusal: refusal.is_some(),
            last_refusal_sequence: refusal.as_ref().map_or(0, |r| r.sequence),
            last_refusal_reason: refusal.map(|r| r.reason).unwrap_or_default(),
        }))
    }

    async fn rebuild_graph(
        &self,
        _: Request<pb::RebuildGraphRequest>,
    ) -> Result<Response<pb::RebuildGraphResponse>, Status> {
        let rebuilt = self.graph().await?.rebuild().await.map_err(graph_status)?;
        Ok(Response::new(pb::RebuildGraphResponse {
            schema_steps: rebuilt.applied.schema_steps as u64,
            records: rebuilt.applied.records as u64,
            voided: rebuilt.applied.voided as u64,
            vault_notes: rebuilt.vault_notes as u64,
            links: rebuilt.links as u64,
            typed_relations: rebuilt.typed_relations as u64,
            ms: rebuilt.ms,
        }))
    }

    async fn void_graph_record(
        &self,
        request: Request<pb::VoidGraphRecordRequest>,
    ) -> Result<Response<pb::VoidGraphRecordResponse>, Status> {
        let request = request.into_inner();
        let void_sequence = self
            .graph()
            .await?
            .void(request.sequence, request.reason)
            .await
            .map_err(graph_status)?;
        Ok(Response::new(pb::VoidGraphRecordResponse { void_sequence }))
    }
}
