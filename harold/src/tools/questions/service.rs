//! The question queue's gRPC service, over the tool's own store.

use std::sync::Arc;

use harold_api::tools::questions::v1 as pb;
use harold_api::tools::questions::v1::question_service_server::QuestionService;
use tokio::sync::Notify;
use tonic::{Request, Response, Status};
use tracing::info;

use super::schema::{self, Kind};
use super::store::{QuestionStore, Refusal, StoreError};
use super::{NAME, now_ms};

pub(super) struct QuestionToolService<S> {
    store: Arc<S>,
    /// Woken after every successful write, so the batch sender looks again.
    wake: Arc<Notify>,
}

impl<S: QuestionStore> QuestionToolService<S> {
    pub(super) fn new(store: Arc<S>, wake: Arc<Notify>) -> Self {
        Self { store, wake }
    }

    fn written(&self) {
        // `notify_one` keeps a permit, so a wake sent while the sender is busy is not lost.
        self.wake.notify_one();
    }
}

fn invalid(field: &str) -> Status {
    Status::invalid_argument(format!("invalid {field}"))
}

fn refused(refusal: Refusal) -> Status {
    Status::failed_precondition(match refusal {
        Refusal::VersionChanged => "version_changed",
        Refusal::NotWaiting => "not_waiting",
        Refusal::NotAQuestion => return invalid("id"),
    })
}

fn unavailable(_: StoreError) -> Status {
    Status::unavailable("the question database is unavailable")
}

fn kind_from_proto(kind: i32) -> Result<Kind, Status> {
    match pb::AnswerKind::try_from(kind) {
        Ok(pb::AnswerKind::Answer) => Ok(Kind::Answer),
        Ok(pb::AnswerKind::Revise) => Ok(Kind::Revise),
        Ok(pb::AnswerKind::Reject) => Ok(Kind::Reject),
        Ok(pb::AnswerKind::FreeText) => Ok(Kind::FreeText),
        Ok(pb::AnswerKind::Done) => Ok(Kind::Done),
        Ok(pb::AnswerKind::Ignore) => Ok(Kind::Ignore),
        Ok(pb::AnswerKind::Unspecified) | Err(_) => Err(invalid("kind")),
    }
}

#[tonic::async_trait]
impl<S: QuestionStore> QuestionService for QuestionToolService<S> {
    async fn put_question(
        &self,
        request: Request<pb::PutQuestionRequest>,
    ) -> Result<Response<pb::PutQuestionResponse>, Status> {
        let question = request
            .into_inner()
            .question
            .ok_or_else(|| invalid("question"))?;
        let question = schema::from_proto(question).map_err(|field| invalid(&field))?;
        let id = question.id.clone();
        let version = self
            .store
            .put(question, now_ms())
            .await
            .map_err(unavailable)?;
        self.written();
        info!(tool = NAME, question_id = %id, version, "question stored");
        Ok(Response::new(pb::PutQuestionResponse { id, version }))
    }

    async fn dismiss_question(
        &self,
        request: Request<pb::DismissQuestionRequest>,
    ) -> Result<Response<pb::DismissQuestionResponse>, Status> {
        let request = request.into_inner();
        let version = self
            .store
            .dismiss(&request.id, &request.reason, now_ms())
            .await
            .map_err(unavailable)?
            .map_err(refused)?;
        self.written();
        info!(tool = NAME, question_id = %request.id, version, "question dismissed");
        Ok(Response::new(pb::DismissQuestionResponse {}))
    }

    async fn reorder_questions(
        &self,
        request: Request<pb::ReorderQuestionsRequest>,
    ) -> Result<Response<pb::ReorderQuestionsResponse>, Status> {
        let ids = request.into_inner().ids;
        self.store
            .reorder(&ids, now_ms())
            .await
            .map_err(unavailable)?;
        self.written();
        Ok(Response::new(pb::ReorderQuestionsResponse {}))
    }

    async fn list_questions(
        &self,
        _request: Request<pb::ListQuestionsRequest>,
    ) -> Result<Response<pb::ListQuestionsResponse>, Status> {
        let listing = self.store.list(now_ms()).await.map_err(unavailable)?;
        let questions: Vec<pb::ListedQuestion> = listing
            .questions
            .iter()
            .map(|listed| pb::ListedQuestion {
                question: Some(schema::to_proto(&listed.question)),
                version: listed.version,
                deferred: listed.deferred,
                ticked: listed.ticked.clone(),
            })
            .collect();
        let waiting = u32::try_from(questions.len()).unwrap_or(u32::MAX);
        let dismissed = listing
            .dismissed
            .into_iter()
            .map(|(id, reason)| pb::Dismissed { id, reason })
            .collect();
        Ok(Response::new(pb::ListQuestionsResponse {
            questions,
            waiting,
            dismissed,
        }))
    }

    async fn answer_question(
        &self,
        request: Request<pb::AnswerQuestionRequest>,
    ) -> Result<Response<pb::AnswerQuestionResponse>, Status> {
        let pb::AnswerQuestionRequest {
            id,
            version,
            kind,
            picks,
            note,
            send_now,
        } = request.into_inner();
        let kind = kind_from_proto(kind)?;
        let resolve =
            move |question: &schema::Question| schema::resolve(question, kind, &picks, &note);
        self.store
            .answer(&id, version, send_now, resolve, now_ms())
            .await
            .map_err(unavailable)?
            .map_err(refused)?
            .map_err(|field| invalid(&field))?;
        self.written();
        info!(tool = NAME, question_id = %id, version, "question answered");
        Ok(Response::new(pb::AnswerQuestionResponse {}))
    }

    async fn defer_question(
        &self,
        request: Request<pb::DeferQuestionRequest>,
    ) -> Result<Response<pb::DeferQuestionResponse>, Status> {
        let id = request.into_inner().id;
        self.store
            .defer(&id, now_ms())
            .await
            .map_err(unavailable)?
            .map_err(refused)?;
        self.written();
        Ok(Response::new(pb::DeferQuestionResponse {}))
    }

    async fn tick_step(
        &self,
        _request: Request<pb::TickStepRequest>,
    ) -> Result<Response<pb::TickStepResponse>, Status> {
        // Task 3 replaces this.
        Err(Status::unimplemented("tick_step"))
    }
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;
