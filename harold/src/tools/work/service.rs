//! The work tool's gRPC service. Every write is guarded first, then checked,
//! then stored through `Rotator::current`, the one writable handle. The tool
//! trusts local callers (ADR 0002): only the chief's pane writes.

use std::collections::{BTreeSet, HashMap};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use harold_api::tools::questions::v1 as qp;
use harold_api::tools::questions::v1::question_service_client::QuestionServiceClient;
use harold_api::tools::work::v1 as pb;
use harold_api::tools::work::v1::work_service_server::WorkService;
use tonic::transport::Channel;
use tonic::{Code, Request, Response, Status};
use tracing::info;

use super::NAME;
use super::check::{self, Facts, QuestionFact, SpecFacts};
use super::files::{WorkFiles, day_of};
use super::rotation::{Clock, Rotator};
use super::schema::{self, Decision, Mark, Priority, Record, valid_link, valid_name};
use super::store::{Patch, PutOutcome, Refusal, StoreError};
use crate::tools::BoxFuture;
use crate::tools::backlog::guard::{Guard, Refused};

/// The longest wait for the question tool: for the connection, and for each call.
const QUESTION_TIMEOUT: Duration = Duration::from_secs(2);

/// The longest wait for all the question states of one check, so a check never
/// hangs. The ids that are not read by then are `Unreadable`.
const QUESTIONS_TIMEOUT: Duration = Duration::from_secs(5);

/// The question tool's answer about each question id.
pub(super) trait QuestionStates: Send + Sync {
    fn states<'a>(&'a self, ids: &'a [String]) -> BoxFuture<'a, HashMap<String, QuestionFact>>;
}

/// Asks the question tool through its `ShowQuestion` call on Harold's own
/// loopback address, as the CLI does. It only reads.
pub(super) struct LoopbackQuestions {
    address: Option<SocketAddr>,
    /// The limit for the connection and for each call.
    per_call: Duration,
    /// The limit for all the calls of one lookup.
    overall: Duration,
}

impl LoopbackQuestions {
    pub(super) fn new(address: Option<SocketAddr>) -> Self {
        Self::with_limits(address, QUESTION_TIMEOUT, QUESTIONS_TIMEOUT)
    }

    pub(super) fn with_limits(
        address: Option<SocketAddr>,
        per_call: Duration,
        overall: Duration,
    ) -> Self {
        Self {
            address,
            per_call,
            overall,
        }
    }

    /// The wait for the next step: the per-call limit, but never past `deadline`.
    fn limit(&self, deadline: tokio::time::Instant) -> Duration {
        self.per_call
            .min(deadline.saturating_duration_since(tokio::time::Instant::now()))
    }
}

impl QuestionStates for LoopbackQuestions {
    fn states<'a>(&'a self, ids: &'a [String]) -> BoxFuture<'a, HashMap<String, QuestionFact>> {
        Box::pin(async move {
            let deadline = tokio::time::Instant::now() + self.overall;
            let client = match self.address {
                Some(address) if !ids.is_empty() => connect(address, self.limit(deadline)).await,
                _ => None,
            };
            let Some(mut client) = client else {
                return ids
                    .iter()
                    .map(|id| (id.clone(), QuestionFact::Unreadable))
                    .collect();
            };
            let mut states = HashMap::new();
            for id in ids {
                let limit = self.limit(deadline);
                let fact = match limit.is_zero() {
                    true => QuestionFact::Unreadable,
                    false => show(&mut client, id, limit).await,
                };
                states.insert(id.clone(), fact);
            }
            states
        })
    }
}

async fn connect(address: SocketAddr, limit: Duration) -> Option<QuestionServiceClient<Channel>> {
    let endpoint = tonic::transport::Endpoint::from_shared(format!("http://{address}"))
        .ok()?
        .connect_timeout(limit)
        .timeout(QUESTION_TIMEOUT);
    let channel = tokio::time::timeout(limit, endpoint.connect())
        .await
        .ok()?
        .ok()?;
    Some(QuestionServiceClient::new(channel))
}

/// `waiting` is `Waiting`. Any other state is `Answered`. `NotFound` with
/// `unknown_question` is `Missing`. Any other error is `Unreadable`.
async fn show(
    client: &mut QuestionServiceClient<Channel>,
    id: &str,
    limit: Duration,
) -> QuestionFact {
    let call = client.show_question(qp::ShowQuestionRequest { id: id.into() });
    match tokio::time::timeout(limit, call).await {
        Ok(Ok(response)) if response.get_ref().state == "waiting" => QuestionFact::Waiting,
        Ok(Ok(_)) => QuestionFact::Answered,
        Ok(Err(status))
            if status.code() == Code::NotFound && status.message() == "unknown_question" =>
        {
            QuestionFact::Missing
        }
        _ => QuestionFact::Unreadable,
    }
}

/// The facts of one check, read before the check runs: the registry, and the
/// state of each linked question.
struct GatheredFacts {
    products: Result<Vec<(String, PathBuf)>, Refused>,
    questions: HashMap<String, QuestionFact>,
}

impl Facts for GatheredFacts {
    fn spec(&self, product: &str, id: &str) -> SpecFacts {
        let Ok(products) = &self.products else {
            return SpecFacts::default();
        };
        // A part that could leave the pod folder names no spec.
        if !safe_part(product) || !safe_part(id) {
            return SpecFacts {
                registry_ok: true,
                ..SpecFacts::default()
            };
        }
        match products.iter().find(|(name, _)| name == product) {
            Some((_, repo)) => spec_facts(repo, id),
            // A product that is not in the registry has no spec.
            None => SpecFacts {
                registry_ok: true,
                ..SpecFacts::default()
            },
        }
    }

    fn question(&self, id: &str) -> QuestionFact {
        self.questions
            .get(id)
            .copied()
            .unwrap_or(QuestionFact::Unreadable)
    }
}

/// A product name or a spec id that is safe in a path: not empty, not `.` or
/// `..`, and with no `/`, `\` or NUL.
fn safe_part(part: &str) -> bool {
    !part.is_empty() && part != "." && part != ".." && !part.contains(['/', '\\', '\0'])
}

/// What the pod files `.superpowers/pod/{specs,merged,withdrawn,approvals}/<id>.md`
/// of the product in `repo` say. A repo folder that is not there cannot be read,
/// and an unsafe `id` names no spec.
fn spec_facts(repo: &Path, id: &str) -> SpecFacts {
    if !repo.is_dir() {
        return SpecFacts::default();
    }
    if !safe_part(id) {
        return SpecFacts {
            registry_ok: true,
            ..SpecFacts::default()
        };
    }
    let pod = repo.join(".superpowers").join("pod");
    let has = |folder: &str| pod.join(folder).join(format!("{id}.md")).is_file();
    SpecFacts {
        exists: has("specs"),
        merged: has("merged"),
        withdrawn: has("withdrawn"),
        approved: has("approvals"),
        registry_ok: true,
    }
}

pub(super) struct WorkToolService {
    rotator: Arc<Rotator>,
    files: WorkFiles,
    guard: Guard,
    questions: Arc<dyn QuestionStates>,
    clock: Arc<dyn Clock>,
}

impl WorkToolService {
    pub(super) fn new(
        rotator: Arc<Rotator>,
        files: WorkFiles,
        guard: Guard,
        questions: Arc<dyn QuestionStates>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            rotator,
            files,
            guard,
            questions,
            clock,
        }
    }

    /// Only the chief writes. A refusal is logged.
    fn may_write(&self, caller_pane: &str) -> Result<(), Status> {
        self.guard.may_write_queue(caller_pane).map_err(|refused| {
            info!(tool = NAME, reason = refused.reason(), "work write refused");
            denied(refused)
        })
    }

    fn now_ms(&self) -> i64 {
        self.clock.now_ms()
    }
}

fn invalid(field: &str) -> Status {
    Status::invalid_argument(format!("invalid {field}"))
}

fn denied(refused: Refused) -> Status {
    let reason = refused.reason();
    match refused {
        Refused::NoPane | Refused::NotChief | Refused::WrongProduct => {
            Status::permission_denied(reason)
        }
        Refused::UnknownProduct => Status::not_found(reason),
        Refused::RegistryUnreadable => Status::unavailable(reason),
    }
}

fn refused(refusal: Refusal) -> Status {
    let reason = refusal.reason();
    match refusal {
        Refusal::NotFound => Status::not_found(reason),
        Refusal::FocusFull | Refusal::NameInUse | Refusal::ReadOnly => {
            Status::failed_precondition(reason)
        }
        Refusal::BadName => invalid("name"),
    }
}

fn unavailable(_: StoreError) -> Status {
    Status::unavailable("the work database is unavailable")
}

/// A field of one line: no line break.
fn one_line(field: &str, text: &str) -> Result<(), Status> {
    match text.contains(['\n', '\r']) {
        true => Err(invalid(field)),
        false => Ok(()),
    }
}

/// A field of one line that is not empty.
fn filled_line(field: &str, text: &str) -> Result<(), Status> {
    match text.trim().is_empty() {
        true => Err(invalid(field)),
        false => one_line(field, text),
    }
}

fn priority_of(word: &str) -> Result<Priority, Status> {
    Priority::parse(word).ok_or_else(|| invalid("priority"))
}

fn status_of(words: &str) -> Result<schema::Status, Status> {
    schema::Status::parse(words).ok_or_else(|| invalid("status"))
}

fn mark_of(word: &str) -> Result<Mark, Status> {
    Mark::parse(word).ok_or_else(|| invalid("mark"))
}

fn link_of(link: String) -> Result<String, Status> {
    match valid_link(&link) {
        true => Ok(link),
        false => Err(invalid("link")),
    }
}

/// A decision with no date is dated `today`.
fn decision_from_proto(decision: pb::Decision, today: &str) -> Result<Decision, Status> {
    filled_line("who", &decision.who)?;
    filled_line("text", &decision.text)?;
    one_line("at", &decision.at)?;
    Ok(Decision {
        at: match decision.at.is_empty() {
            true => today.to_string(),
            false => decision.at,
        },
        who: decision.who,
        mark: mark_of(&decision.mark)?,
        text: decision.text,
    })
}

fn record_from_proto(record: pb::WorkRecord, today: &str) -> Result<Record, Status> {
    if !valid_name(&record.name) {
        return Err(invalid("name"));
    }
    one_line("title", &record.title)?;
    one_line("waits_for", &record.waits_for)?;
    one_line("next_step", &record.next_step)?;
    Ok(Record {
        priority: priority_of(&record.priority)?,
        status: status_of(&record.status)?,
        decisions: record
            .decisions
            .into_iter()
            .map(|decision| decision_from_proto(decision, today))
            .collect::<Result<_, _>>()?,
        links: record
            .links
            .into_iter()
            .map(link_of)
            .collect::<Result<_, _>>()?,
        name: record.name,
        title: record.title,
        waits_for: record.waits_for,
        next_step: record.next_step,
        // The store sets these.
        created_in: String::new(),
        created_at_ms: 0,
        updated_at_ms: 0,
        status_since_ms: 0,
    })
}

fn record_to_proto(record: Record) -> pb::WorkRecord {
    pb::WorkRecord {
        name: record.name,
        title: record.title,
        priority: record.priority.as_str().into(),
        status: record.status.as_str().into(),
        waits_for: record.waits_for,
        next_step: record.next_step,
        created_in: record.created_in,
        created_at_ms: record.created_at_ms,
        updated_at_ms: record.updated_at_ms,
        status_since_ms: record.status_since_ms,
        decisions: record
            .decisions
            .into_iter()
            .map(|decision| pb::Decision {
                at: decision.at,
                who: decision.who,
                mark: decision.mark.as_str().into(),
                text: decision.text,
            })
            .collect(),
        links: record.links,
    }
}

/// The ids of the questions that the records link, each once.
fn question_ids(records: &[Record]) -> Vec<String> {
    let ids: BTreeSet<&str> = records
        .iter()
        .flat_map(|record| &record.links)
        .filter_map(|link| link.strip_prefix("question:"))
        .collect();
    ids.into_iter().map(String::from).collect()
}

#[tonic::async_trait]
impl WorkService for WorkToolService {
    async fn list(
        &self,
        request: Request<pb::ListRequest>,
    ) -> Result<Response<pb::ListResponse>, Status> {
        let all = request.into_inner().all;
        let file = self.rotator.current().await.map_err(unavailable)?;
        let records = file.list(all).await.map_err(unavailable)?;
        Ok(Response::new(pb::ListResponse {
            records: records.into_iter().map(record_to_proto).collect(),
            file: file.file_name().to_string(),
        }))
    }

    async fn show(
        &self,
        request: Request<pb::ShowRequest>,
    ) -> Result<Response<pb::ShowResponse>, Status> {
        let name = request.into_inner().name;
        let file = self.rotator.current().await.map_err(unavailable)?;
        let record = file
            .get(&name)
            .await
            .map_err(unavailable)?
            .ok_or_else(|| refused(Refusal::NotFound))?;
        Ok(Response::new(pb::ShowResponse {
            record: Some(record_to_proto(record)),
        }))
    }

    async fn put(
        &self,
        request: Request<pb::PutRequest>,
    ) -> Result<Response<pb::PutResponse>, Status> {
        let pb::PutRequest {
            record,
            caller_pane,
        } = request.into_inner();
        self.may_write(&caller_pane)?;
        let now = self.now_ms();
        let record = record_from_proto(record.ok_or_else(|| invalid("record"))?, &day_of(now))?;
        let file = self.rotator.current().await.map_err(unavailable)?;
        let outcome = file
            .put(record, now)
            .await
            .map_err(unavailable)?
            .map_err(refused)?;
        Ok(Response::new(pb::PutResponse {
            created: outcome == PutOutcome::Created,
        }))
    }

    async fn set(
        &self,
        request: Request<pb::SetRequest>,
    ) -> Result<Response<pb::SetResponse>, Status> {
        let pb::SetRequest {
            name,
            title,
            priority,
            status,
            waits_for,
            next_step,
            caller_pane,
        } = request.into_inner();
        self.may_write(&caller_pane)?;
        for (field, text) in [
            ("title", &title),
            ("waits_for", &waits_for),
            ("next_step", &next_step),
        ] {
            if let Some(text) = text {
                one_line(field, text)?;
            }
        }
        let patch = Patch {
            title,
            priority: priority.as_deref().map(priority_of).transpose()?,
            status: status.as_deref().map(status_of).transpose()?,
            waits_for,
            next_step,
        };
        let file = self.rotator.current().await.map_err(unavailable)?;
        file.set(&name, patch, self.now_ms())
            .await
            .map_err(unavailable)?
            .map_err(refused)?;
        Ok(Response::new(pb::SetResponse {}))
    }

    async fn decide(
        &self,
        request: Request<pb::DecideRequest>,
    ) -> Result<Response<pb::DecideResponse>, Status> {
        let pb::DecideRequest {
            name,
            mark,
            who,
            text,
            caller_pane,
        } = request.into_inner();
        self.may_write(&caller_pane)?;
        let now = self.now_ms();
        let decision = decision_from_proto(
            pb::Decision {
                at: String::new(),
                who,
                mark,
                text,
            },
            &day_of(now),
        )?;
        let file = self.rotator.current().await.map_err(unavailable)?;
        let number = file
            .decide(&name, decision, now)
            .await
            .map_err(unavailable)?
            .map_err(refused)?;
        Ok(Response::new(pb::DecideResponse { number }))
    }

    async fn mark(
        &self,
        request: Request<pb::MarkRequest>,
    ) -> Result<Response<pb::MarkResponse>, Status> {
        let pb::MarkRequest {
            name,
            number,
            mark,
            caller_pane,
        } = request.into_inner();
        self.may_write(&caller_pane)?;
        let mark = mark_of(&mark)?;
        let file = self.rotator.current().await.map_err(unavailable)?;
        file.mark(&name, number, mark, self.now_ms())
            .await
            .map_err(unavailable)?
            .map_err(refused)?;
        Ok(Response::new(pb::MarkResponse {}))
    }

    async fn link(
        &self,
        request: Request<pb::LinkRequest>,
    ) -> Result<Response<pb::LinkResponse>, Status> {
        let pb::LinkRequest {
            name,
            link,
            caller_pane,
        } = request.into_inner();
        self.may_write(&caller_pane)?;
        let link = link_of(link)?;
        let file = self.rotator.current().await.map_err(unavailable)?;
        file.link(&name, &link, self.now_ms())
            .await
            .map_err(unavailable)?
            .map_err(refused)?;
        Ok(Response::new(pb::LinkResponse {}))
    }

    async fn unlink(
        &self,
        request: Request<pb::UnlinkRequest>,
    ) -> Result<Response<pb::UnlinkResponse>, Status> {
        let pb::UnlinkRequest {
            name,
            link,
            caller_pane,
        } = request.into_inner();
        self.may_write(&caller_pane)?;
        let file = self.rotator.current().await.map_err(unavailable)?;
        file.unlink(&name, &link, self.now_ms())
            .await
            .map_err(unavailable)?
            .map_err(refused)?;
        Ok(Response::new(pb::UnlinkResponse {}))
    }

    async fn check(
        &self,
        _request: Request<pb::CheckRequest>,
    ) -> Result<Response<pb::CheckResponse>, Status> {
        // The guard is let go before the question calls, so a slow question
        // tool does not hold back a rotation.
        let records = {
            let file = self.rotator.current().await.map_err(unavailable)?;
            file.list(true).await.map_err(unavailable)?
        };
        let facts = GatheredFacts {
            products: self.guard.products(),
            questions: self.questions.states(&question_ids(&records)).await,
        };
        let mismatches = check::check(&records, &facts)
            .into_iter()
            .map(|mismatch| pb::Mismatch {
                record: mismatch.record,
                kind: mismatch.kind.into(),
                line: mismatch.line,
            })
            .collect();
        Ok(Response::new(pb::CheckResponse { mismatches }))
    }

    async fn import(
        &self,
        _request: Request<pb::ImportRequest>,
    ) -> Result<Response<pb::ImportResponse>, Status> {
        // Task 7 builds the import. Until then the call is not served.
        Err(Status::unimplemented("import is not built yet"))
    }

    async fn rotate(
        &self,
        request: Request<pb::RotateRequest>,
    ) -> Result<Response<pb::RotateResponse>, Status> {
        if !request.into_inner().dry_run {
            return Err(Status::failed_precondition("rotation_is_harolds"));
        }
        let dry = self.rotator.dry_run().await.map_err(unavailable)?;
        Ok(Response::new(pb::RotateResponse {
            open_records: dry.counts.open_records,
            decisions: dry.counts.decisions,
            links: dry.counts.links,
            records: dry.records,
            target: dry.target,
        }))
    }

    async fn history(
        &self,
        request: Request<pb::HistoryRequest>,
    ) -> Result<Response<pb::HistoryResponse>, Status> {
        let name = request.into_inner().name;
        let (file, record) = self
            .files
            .history(&name)
            .await
            .map_err(unavailable)?
            .ok_or_else(|| refused(Refusal::NotFound))?;
        Ok(Response::new(pb::HistoryResponse {
            file,
            record: Some(record_to_proto(record)),
        }))
    }

    async fn search(
        &self,
        request: Request<pb::SearchRequest>,
    ) -> Result<Response<pb::SearchResponse>, Status> {
        let word = request.into_inner().word;
        if word.trim().is_empty() {
            return Err(invalid("word"));
        }
        let found = self
            .files
            .search(&word)
            .await
            .map_err(unavailable)?
            .into_iter()
            .map(|(file, record)| pb::Found {
                file,
                record: Some(record_to_proto(record)),
            })
            .collect();
        Ok(Response::new(pb::SearchResponse { found }))
    }
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;
