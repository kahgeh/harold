//! The question queue's gRPC service, over the tool's own store.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use harold_api::tools::questions::v1 as pb;
use harold_api::tools::questions::v1::question_service_server::QuestionService;
use tokio::sync::Notify;
use tonic::{Request, Response, Status};
use tracing::{info, warn};

use super::schema::{self, Kind};
use super::spec::{Reason, SpecChecker, State};
use super::store::{QuestionStore, Refusal, SavedPick, StoreError, TickRefusal};
use super::{NAME, now_ms};
use crate::tools::Products;

/// The refusal an answer or a pick gets while the todo's spec has changed.
const SPEC_CHANGED: &str = "spec_changed";

pub(super) struct QuestionToolService<S> {
    store: Arc<S>,
    /// Woken after every successful write, so the batch sender looks again.
    wake: Arc<Notify>,
    checker: Arc<SpecChecker>,
    /// The question versions whose changed or unknown spec has been logged: a line is logged
    /// once per version, however often the todos are listed.
    logged: Mutex<HashSet<(String, u32, bool)>>,
}

impl<S: QuestionStore> QuestionToolService<S> {
    pub(super) fn new(store: Arc<S>, wake: Arc<Notify>, products: Arc<dyn Products>) -> Self {
        Self {
            store,
            wake,
            checker: Arc::new(SpecChecker::new(products)),
            logged: Mutex::new(HashSet::new()),
        }
    }

    /// The spec state of each `(spec_file, sha)`. The file system is read off the runtime's
    /// threads.
    async fn spec_states(&self, asked: Vec<(String, String)>) -> Vec<State> {
        let checker = Arc::clone(&self.checker);
        let count = asked.len();
        tokio::task::spawn_blocking(move || {
            asked
                .iter()
                .map(|(file, sha)| checker.state(file, sha))
                .collect()
        })
        .await
        .unwrap_or_else(|_| vec![State::Unknown(None); count])
    }

    /// Logs a changed or unknown spec once per question version, and forgets the versions
    /// no longer listed. `listed` is every version the caller read; `seen` holds those
    /// whose state it checked, with the state.
    fn note_states(&self, seen: &[(String, u32, State)], listed: Option<&[(String, u32)]>) {
        let mut logged = self.logged.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(listed) = listed {
            logged.retain(|(id, version, _)| listed.iter().any(|(i, v)| i == id && v == version));
        }
        for (id, version, state) in seen {
            match state {
                State::Changed => {
                    if logged.insert((id.clone(), *version, true)) {
                        warn!(
                            tool = NAME,
                            question_id = %id,
                            version = *version,
                            "todo changed spec"
                        );
                    }
                }
                State::Unknown(Some(reason)) => {
                    if logged.insert((id.clone(), *version, false)) {
                        info!(
                            tool = NAME,
                            question_id = %id,
                            version = *version,
                            reason = reason.word(),
                            "todo spec unknown"
                        );
                    }
                }
                State::Current | State::Unknown(None) => {}
            }
        }
    }

    /// Refuses with `spec_changed` when the question is open at `version` and its spec file
    /// no longer has the checksum it was asked with.
    async fn refuse_changed(&self, id: &str, version: u32) -> Result<(), Status> {
        let Some(shown) = self.store.show(id).await.map_err(unavailable)? else {
            return Ok(());
        };
        if shown.version != version || !matches!(shown.state.as_str(), "waiting" | "deferred") {
            return Ok(());
        }
        let asked = vec![(shown.question.spec_file.clone(), shown.question.sha.clone())];
        let state = self.spec_states(asked).await[0];
        self.note_states(&[(id.to_owned(), version, state)], None);
        match state {
            State::Changed => Err(Status::failed_precondition(SPEC_CHANGED)),
            _ => Ok(()),
        }
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

fn spec_state_to_proto(state: State) -> pb::SpecState {
    match state {
        State::Current => pb::SpecState::Current,
        State::Changed => pb::SpecState::Changed,
        State::Unknown(_) => pb::SpecState::Unknown,
    }
}

fn pick_to_proto(pick: &SavedPick) -> pb::Pick {
    pb::Pick {
        decision: pick.decision,
        key: pick.key.clone(),
        text: pick.text.clone(),
    }
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
        if !question.spec_file.is_empty() {
            let checker = Arc::clone(&self.checker);
            let file = question.spec_file.clone();
            // A registry that cannot be read does not stop the todo: its spec stays
            // `unknown` until the check passes.
            let allowed = tokio::task::spawn_blocking(move || checker.may_put(&file))
                .await
                .unwrap_or(Err(Reason::OutsideSpecs));
            if matches!(allowed, Err(Reason::OutsideSpecs)) {
                return Err(invalid("spec_file"));
            }
        }
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
        let asked = listing
            .questions
            .iter()
            .map(|listed| {
                (
                    listed.question.spec_file.clone(),
                    listed.question.sha.clone(),
                )
            })
            .collect();
        let states = self.spec_states(asked).await;
        let seen: Vec<(String, u32, State)> = listing
            .questions
            .iter()
            .zip(&states)
            .map(|(listed, state)| (listed.question.id.clone(), listed.version, *state))
            .collect();
        let listed_versions: Vec<(String, u32)> = seen
            .iter()
            .map(|(id, version, _)| (id.clone(), *version))
            .collect();
        self.note_states(&seen, Some(&listed_versions));
        let questions: Vec<pb::ListedQuestion> = listing
            .questions
            .iter()
            .zip(&states)
            .map(|(listed, state)| pb::ListedQuestion {
                question: Some(schema::to_proto(&listed.question)),
                version: listed.version,
                deferred: listed.deferred,
                ticked: listed.ticked.clone(),
                picks: listed.picks.iter().map(pick_to_proto).collect(),
                spec_state: spec_state_to_proto(*state) as i32,
            })
            .collect();
        let waiting = listing
            .questions
            .iter()
            .filter(|listed| !listed.question.is_instructions())
            .count();
        let waiting = u32::try_from(waiting).unwrap_or(u32::MAX);
        let dismissed = listing
            .dismissed
            .into_iter()
            .map(|(id, reason)| pb::Dismissed { id, reason })
            .collect();
        Ok(Response::new(pb::ListQuestionsResponse {
            questions,
            waiting,
            dismissed,
            checked_at_ms: listing.checked_at_ms,
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
            trace_parent,
        } = request.into_inner();
        let kind = kind_from_proto(kind)?;
        self.refuse_changed(&id, version).await?;
        // Kept only when it is a well-formed `traceparent`: a stored value is parsed again
        // when the answers are typed, and says nothing else.
        let trace_parent = crate::telemetry::context_from_traceparent(&trace_parent)
            .and_then(|cx| crate::telemetry::traceparent_of(&cx));
        let resolve =
            move |question: &schema::Question| schema::resolve(question, kind, &picks, &note);
        self.store
            .answer_traced(
                &id,
                version,
                send_now,
                trace_parent.as_deref(),
                resolve,
                now_ms(),
            )
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
        request: Request<pb::TickStepRequest>,
    ) -> Result<Response<pb::TickStepResponse>, Status> {
        let pb::TickStepRequest {
            id,
            version,
            step,
            ticked,
        } = request.into_inner();
        self.store
            .tick(&id, version, step, ticked, now_ms())
            .await
            .map_err(unavailable)?
            .map_err(|refusal| match refusal {
                TickRefusal::VersionChanged => Status::failed_precondition("version_changed"),
                TickRefusal::NotWaiting => Status::failed_precondition("not_waiting"),
                TickRefusal::InvalidStep => invalid("step"),
            })?;
        self.written();
        let reason = if ticked { "ticked" } else { "unticked" };
        info!(tool = NAME, question_id = %id, version, reason, "step ticked");
        Ok(Response::new(pb::TickStepResponse {}))
    }

    async fn save_picks(
        &self,
        request: Request<pb::SavePicksRequest>,
    ) -> Result<Response<pb::SavePicksResponse>, Status> {
        let pb::SavePicksRequest { id, version, picks } = request.into_inner();
        self.refuse_changed(&id, version).await?;
        self.store
            .save_picks(&id, version, &picks)
            .await
            .map_err(|error| {
                warn!(
                    tool = NAME,
                    question_id = %id,
                    version,
                    reason = "store_failed",
                    kind = error.kind(),
                    error = %error,
                    "picks not saved"
                );
                unavailable(error)
            })?
            .map_err(refused)?
            .map_err(|field| invalid(&field))?;
        // A pick is for the dash and the chief to read: it wakes nothing.
        info!(tool = NAME, question_id = %id, version, "picks saved");
        Ok(Response::new(pb::SavePicksResponse {}))
    }

    async fn show_question(
        &self,
        request: Request<pb::ShowQuestionRequest>,
    ) -> Result<Response<pb::ShowQuestionResponse>, Status> {
        let id = request.into_inner().id;
        let shown = self
            .store
            .show(&id)
            .await
            .map_err(unavailable)?
            .ok_or_else(|| Status::not_found("unknown_question"))?;
        let state = self
            .spec_states(vec![(
                shown.question.spec_file.clone(),
                shown.question.sha.clone(),
            )])
            .await[0];
        Ok(Response::new(pb::ShowQuestionResponse {
            spec_state: spec_state_to_proto(state) as i32,
            question: Some(schema::to_proto(&shown.question)),
            version: shown.version,
            state: shown.state,
            reason: shown.reason,
            picks: shown.picks.iter().map(pick_to_proto).collect(),
        }))
    }

    async fn mark_checked(
        &self,
        _request: Request<pb::MarkCheckedRequest>,
    ) -> Result<Response<pb::MarkCheckedResponse>, Status> {
        let now = now_ms();
        self.store.mark_checked(now).await.map_err(unavailable)?;
        let waiting = self
            .store
            .list(now)
            .await
            .map_err(unavailable)?
            .questions
            .len();
        info!(tool = NAME, count = waiting, "todos checked");
        Ok(Response::new(pb::MarkCheckedResponse {
            checked_at_ms: now,
        }))
    }
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;
