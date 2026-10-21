//! The todo page: the question tool's todos, read and answered through its gRPC service
//! on Harold's own address, as the dash does. The web app never calls into the tool's
//! module (ADR 0001).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use askama::Template;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use datastar::prelude::PatchElements;
use harold_api::tools::questions::v1::question_service_client::QuestionServiceClient;
use harold_api::tools::questions::v1::{
    AnswerKind, AnswerQuestionRequest, DeferQuestionRequest, ListQuestionsRequest, ListedQuestion,
    Pick, SavePicksRequest, TickStepRequest,
};
use tonic::transport::{Channel, Endpoint};
use tonic::{Code, Status};
use tracing::{info, warn};

use super::AppState;
use crate::settings::GrpcSettings;

/// How long a write to the question tool may take.
const WRITE_LIMIT: Duration = Duration::from_secs(5);
/// The largest todo form read, in bytes.
const FORM_MAX_BYTES: usize = 16 * 1024;

/// How long one read of the todos may take, as the dash allows.
const READ_LIMIT: Duration = Duration::from_secs(2);

/// Why a read of the todos failed, as `app todos not read` logs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadFailure {
    Unavailable,
    Timeout,
}

impl ReadFailure {
    fn reason(self) -> &'static str {
        match self {
            Self::Unavailable => "unavailable",
            Self::Timeout => "timeout",
        }
    }
}

/// What the web apps' pages share of the todos: the last list read, whether reads are
/// failing, and the notices on cards.
#[derive(Default)]
struct Shared {
    /// The last list a read returned, in the tool's order with the set-aside questions last.
    last: Option<Vec<ListedQuestion>>,
    /// True from the first failed read until the next good one.
    failing: bool,
    /// A refusal's words per todo id, kept until the todo is gone or next answered.
    notices: HashMap<String, String>,
}

/// The question tool's service on Harold's own gRPC address.
#[derive(Clone)]
pub(crate) struct Questions {
    pub(super) client: QuestionServiceClient<Channel>,
    shared: Arc<Mutex<Shared>>,
}

impl Questions {
    /// A client that connects at its first call, so a tool that did not start fails that
    /// call and not the web app's start.
    pub(crate) fn lazy(grpc: &GrpcSettings) -> Self {
        let channel = Endpoint::from_shared(format!("http://{}:{}", grpc.host, grpc.port))
            .expect("host and port make a URI")
            .connect_lazy();
        Self {
            client: QuestionServiceClient::new(channel),
            shared: Arc::default(),
        }
    }

    /// Reads the todos once. A failed read keeps the last list and logs
    /// `app todos not read` the first time in a row; the first good read after a failure
    /// logs `app todos read again`.
    pub(crate) async fn refresh(&self) -> TodosSection {
        let mut client = self.client.clone();
        let read =
            match tokio::time::timeout(READ_LIMIT, client.list_questions(ListQuestionsRequest {}))
                .await
            {
                Ok(Ok(response)) => Ok(response.into_inner().questions),
                Ok(Err(_)) => Err(ReadFailure::Unavailable),
                Err(_) => Err(ReadFailure::Timeout),
            };
        let mut shared = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        match read {
            Ok(mut list) => {
                // The tool's order, the questions set aside last.
                list.sort_by_key(|listed| listed.deferred);
                if shared.failing {
                    shared.failing = false;
                    info!("app todos read again");
                }
                let ids: Vec<&str> = list
                    .iter()
                    .filter_map(|l| l.question.as_ref().map(|q| q.id.as_str()))
                    .collect();
                shared.notices.retain(|id, _| ids.contains(&id.as_str()));
                shared.last = Some(list);
            }
            Err(failure) => {
                if !shared.failing {
                    shared.failing = true;
                    warn!(reason = failure.reason(), "app todos not read");
                }
            }
        }
        Self::section(&shared)
    }

    /// Shows `text` on the card of todo `id` until it is gone or answered.
    pub(super) fn set_notice(&self, id: &str, text: &str) {
        let mut shared = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        shared.notices.insert(id.to_owned(), text.to_owned());
    }

    /// Takes the notice off the card of todo `id`.
    pub(super) fn clear_notice(&self, id: &str) {
        let mut shared = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        shared.notices.remove(id);
    }

    fn section(shared: &Shared) -> TodosSection {
        match &shared.last {
            None => TodosSection {
                unreadable: true,
                cards: Vec::new(),
                waiting: 0,
            },
            Some(list) => TodosSection {
                unreadable: false,
                cards: list
                    .iter()
                    .filter_map(|listed| Card::from_listed(listed, &shared.notices))
                    .collect(),
                waiting: list.iter().filter(|listed| !listed.deferred).count(),
            },
        }
    }
}

/// `#todos`: the todo page, as the stream and the page draw it.
#[derive(Template)]
#[template(path = "web_app/todos.html")]
pub(crate) struct TodosSection {
    /// No list has ever been read.
    unreadable: bool,
    cards: Vec<Card>,
    /// The todo count: the waiting questions and the instructions.
    waiting: usize,
}

impl TodosSection {
    pub(crate) fn count(&self) -> TodoCount {
        TodoCount {
            count: if self.unreadable { 0 } else { self.waiting },
        }
    }
}

/// `#todo-count`, the number on the thread page's **Todos** button; empty with none.
#[derive(Template)]
#[template(
    source = r#"<span id="todo-count" class="todo-count">{% if count > 0 %}{{ count }}{% endif %}</span>"#,
    ext = "html"
)]
pub(crate) struct TodoCount {
    count: usize,
}

struct Card {
    id: String,
    version: u32,
    is_question: bool,
    kind: &'static str,
    product: String,
    later: bool,
    heading: String,
    under: Option<String>,
    link: Option<CardLink>,
    decisions: Vec<DecisionView>,
    steps: Vec<StepView>,
    notice: Option<String>,
    can_send: bool,
}

struct CardLink {
    /// Set only for an `http://` or `https://` address; anything else shows as text.
    href: Option<String>,
    text: String,
}

struct DecisionView {
    index: usize,
    ask: String,
    options: Vec<OptionView>,
}

struct OptionView {
    key: String,
    label: String,
    impact: String,
    suggested: bool,
    free: bool,
    picked: bool,
    text: String,
}

struct StepView {
    n: usize,
    text: String,
    detail: String,
    ticked: bool,
}

impl Card {
    fn from_listed(listed: &ListedQuestion, notices: &HashMap<String, String>) -> Option<Self> {
        let question = listed.question.as_ref()?;
        let is_question = question.steps.is_empty();
        let link = (!question.link.is_empty()).then(|| {
            let text = if question.spec.is_empty() {
                "Open the link".to_owned()
            } else {
                format!("Read the spec: {}", question.spec)
            };
            let safe =
                question.link.starts_with("https://") || question.link.starts_with("http://");
            CardLink {
                href: safe.then(|| question.link.clone()),
                text,
            }
        });
        let decisions: Vec<DecisionView> = question
            .decisions
            .iter()
            .enumerate()
            .map(|(index, decision)| {
                let saved = listed
                    .picks
                    .iter()
                    .find(|pick| usize::try_from(pick.decision).is_ok_and(|d| d == index));
                let picked_key = saved.map(|pick| pick.key.as_str()).or_else(|| {
                    decision
                        .options
                        .iter()
                        .find(|option| option.suggested)
                        .map(|option| option.key.as_str())
                });
                DecisionView {
                    index,
                    ask: decision.ask.clone(),
                    options: decision
                        .options
                        .iter()
                        .map(|option| {
                            let picked = picked_key == Some(option.key.as_str());
                            OptionView {
                                key: option.key.clone(),
                                label: option.label.clone(),
                                impact: option.impact.clone(),
                                suggested: option.suggested,
                                free: option.free,
                                picked,
                                text: saved
                                    .filter(|_| picked)
                                    .map(|pick| pick.text.clone())
                                    .unwrap_or_default(),
                            }
                        })
                        .collect(),
                }
            })
            .collect();
        let can_send = decisions.iter().all(|decision| {
            decision
                .options
                .iter()
                .any(|option| option.picked && (!option.free || !option.text.trim().is_empty()))
        });
        let steps = question
            .steps
            .iter()
            .enumerate()
            .map(|(i, step)| StepView {
                n: i + 1,
                text: step.text.clone(),
                detail: step.detail.clone(),
                ticked: u32::try_from(i + 1).is_ok_and(|n| listed.ticked.contains(&n)),
            })
            .collect();
        let (heading, under) = if is_question {
            (question.intro.clone(), None)
        } else {
            (
                question.title.clone(),
                (!question.intro.is_empty()).then(|| question.intro.clone()),
            )
        };
        Some(Self {
            id: question.id.clone(),
            version: listed.version,
            is_question,
            kind: if is_question {
                "question"
            } else {
                "instructions"
            },
            product: question.product.clone(),
            later: listed.deferred,
            heading,
            under,
            link,
            decisions,
            steps,
            notice: notices.get(&question.id).cloned(),
            can_send,
        })
    }
}

// ---------------------------------------------------------------------------
// The posts
// ---------------------------------------------------------------------------

/// What a post did, as `app todo answered` logs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Answer,
    FreeText,
    Later,
    Picks,
    Tick,
    Done,
    Ignore,
}

impl Kind {
    fn word(self) -> &'static str {
        match self {
            Self::Answer => "answer",
            Self::FreeText => "free_text",
            Self::Later => "later",
            Self::Picks => "picks",
            Self::Tick => "tick",
            Self::Done => "done",
            Self::Ignore => "ignore",
        }
    }
}

/// How the question tool answered a write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Sent,
    VersionChanged,
    NotWaiting,
    Unavailable,
}

impl Outcome {
    fn word(self) -> &'static str {
        match self {
            Self::Sent => "sent",
            Self::VersionChanged => "version_changed",
            Self::NotWaiting => "not_waiting",
            Self::Unavailable => "unavailable",
        }
    }

    fn of(result: Result<Result<(), Status>, tokio::time::error::Elapsed>) -> Self {
        match result {
            Ok(Ok(())) => Self::Sent,
            Ok(Err(status)) if status.code() == Code::FailedPrecondition => {
                match status.message() {
                    "version_changed" => Self::VersionChanged,
                    "not_waiting" => Self::NotWaiting,
                    _ => Self::Unavailable,
                }
            }
            Ok(Err(_)) | Err(_) => Self::Unavailable,
        }
    }

    /// The words on the card for a refusal.
    fn notice(self, id: &str, kind: Kind) -> Option<String> {
        match self {
            Self::Sent => None,
            Self::VersionChanged => Some(format!("{id} changed. Look again before you send.")),
            Self::NotWaiting => Some(format!("{id} was already answered or withdrawn.")),
            Self::Unavailable if kind == Kind::Picks => {
                Some("Harold can't save your picks right now".to_owned())
            }
            Self::Unavailable => Some("Harold can't send this right now. Try again.".to_owned()),
        }
    }
}

/// A todo form as posted: its fields in order, repeats kept.
struct Form(Vec<(String, String)>);

impl Form {
    /// None when the body is not `application/x-www-form-urlencoded` text.
    fn parse(body: &[u8]) -> Option<Self> {
        if body.len() > FORM_MAX_BYTES {
            return None;
        }
        let text = std::str::from_utf8(body).ok()?;
        let mut fields = Vec::new();
        for pair in text.split('&').filter(|pair| !pair.is_empty()) {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            fields.push((decode(name)?, decode(value)?));
        }
        Some(Self(fields))
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(field, _)| field == name)
            .map(|(_, value)| value.as_str())
    }

    /// The todo's id: present and not blank.
    fn id(&self) -> Option<String> {
        let id = self.get("id")?.trim();
        (!id.is_empty()).then(|| id.to_owned())
    }

    fn version(&self) -> Option<u32> {
        self.get("version")?.parse().ok()
    }
}

/// Decodes one `application/x-www-form-urlencoded` name or value.
fn decode(text: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(text.len());
    let mut rest = text.bytes();
    while let Some(byte) = rest.next() {
        match byte {
            b'+' => bytes.push(b' '),
            b'%' => {
                let high = char::from(rest.next()?).to_digit(16)?;
                let low = char::from(rest.next()?).to_digit(16)?;
                bytes.push(u8::try_from(high * 16 + low).ok()?);
            }
            other => bytes.push(other),
        }
    }
    String::from_utf8(bytes).ok()
}

impl Questions {
    /// The listed todo `id`, as last read.
    fn find(&self, id: &str) -> Option<ListedQuestion> {
        let shared = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        shared
            .last
            .as_ref()?
            .iter()
            .find(|listed| listed.question.as_ref().is_some_and(|q| q.id == id))
            .cloned()
    }

    /// The picks a form holds, as the tool takes them, and whether any is free. None when
    /// the form is malformed: a `pick.<n>` that is not a number or is repeated, an empty
    /// key, a decision the todo does not have, a free pick without words, or a todo whose
    /// decisions are not all picked.
    fn picks(&self, id: &str, form: &Form) -> Option<(Vec<Pick>, bool)> {
        self.read_picks(id, form, false)
    }

    /// The picks a form holds so far, as the tool saves them: as `picks`, but a decision may
    /// be missing, a free pick without words yet is left out, and no pick at all is an empty
    /// list.
    fn partial_picks(&self, id: &str, form: &Form) -> Option<Vec<Pick>> {
        self.read_picks(id, form, true).map(|(picks, _)| picks)
    }

    fn read_picks(&self, id: &str, form: &Form, partial: bool) -> Option<(Vec<Pick>, bool)> {
        let known = self.find(id).and_then(|listed| listed.question);
        let mut picks = BTreeMap::new();
        let mut seen = BTreeSet::new();
        let mut any_free = false;
        for (name, key) in &form.0 {
            let Some(number) = name.strip_prefix("pick.") else {
                continue;
            };
            let decision: u32 = number.parse().ok()?;
            if key.is_empty() || !seen.insert(decision) {
                return None;
            }
            let text = form.get(&format!("text.{decision}")).unwrap_or("").trim();
            let free = match &known {
                Some(question) => {
                    let options = &question.decisions.get(decision as usize)?.options;
                    options.iter().find(|option| &option.key == key)?.free
                }
                None => !text.is_empty(),
            };
            if free && text.is_empty() {
                if partial {
                    continue;
                }
                return None;
            }
            any_free |= free;
            picks.insert(
                decision,
                Pick {
                    decision,
                    key: key.clone(),
                    text: if free { text.to_owned() } else { String::new() },
                },
            );
        }
        if !partial
            && let Some(question) = &known
            && picks.len() != question.decisions.len()
        {
            return None;
        }
        (partial || !picks.is_empty()).then(|| (picks.into_values().collect(), any_free))
    }

    async fn answer(&self, request: AnswerQuestionRequest) -> Outcome {
        let mut client = self.client.clone();
        let result = tokio::time::timeout(WRITE_LIMIT, client.answer_question(request)).await;
        Outcome::of(result.map(|r| r.map(|_| ())))
    }

    async fn defer(&self, id: String) -> Outcome {
        let mut client = self.client.clone();
        let result = tokio::time::timeout(
            WRITE_LIMIT,
            client.defer_question(DeferQuestionRequest { id }),
        )
        .await;
        Outcome::of(result.map(|r| r.map(|_| ())))
    }

    async fn save_picks(&self, request: SavePicksRequest) -> Outcome {
        let mut client = self.client.clone();
        let result = tokio::time::timeout(WRITE_LIMIT, client.save_picks(request)).await;
        Outcome::of(result.map(|r| r.map(|_| ())))
    }

    async fn tick(&self, request: TickStepRequest) -> Outcome {
        let mut client = self.client.clone();
        let result = tokio::time::timeout(WRITE_LIMIT, client.tick_step(request)).await;
        Outcome::of(result.map(|r| r.map(|_| ())))
    }
}

/// Logs the post, sets or clears the card's notice, reads the todos again, wakes every open
/// page, and answers with the redrawn page.
async fn finish(state: &AppState, kind: Kind, id: &str, outcome: Outcome) -> Response {
    log_post(kind, outcome);
    match outcome.notice(id, kind) {
        Some(notice) => state.questions.set_notice(id, &notice),
        None => state.questions.clear_notice(id),
    }
    let section = state.questions.refresh().await;
    state.todos_changed.send_modify(|posts| *posts += 1);
    let page = section.render().unwrap_or_default();
    let count = section.count().render().unwrap_or_default();
    let patch: Event = PatchElements::new(format!("{page}{count}")).into();
    Sse::new(tokio_stream::once(Ok::<Event, Infallible>(patch))).into_response()
}

fn log_post(kind: Kind, outcome: Outcome) {
    info!(
        kind = kind.word(),
        result = outcome.word(),
        "app todo answered"
    );
}

fn malformed() -> Response {
    StatusCode::BAD_REQUEST.into_response()
}

/// `POST /todos/answer`: the picks of a question, sent to the chief at once.
pub(crate) async fn post_answer(State(state): State<AppState>, body: Bytes) -> Response {
    let Some(form) = Form::parse(&body) else {
        return malformed();
    };
    let (Some(id), Some(version)) = (form.id(), form.version()) else {
        return malformed();
    };
    let Some((picks, any_free)) = state.questions.picks(&id, &form) else {
        return malformed();
    };
    let kind = if any_free {
        Kind::FreeText
    } else {
        Kind::Answer
    };
    let outcome = state
        .questions
        .answer(AnswerQuestionRequest {
            id: id.clone(),
            version,
            kind: if any_free {
                AnswerKind::FreeText
            } else {
                AnswerKind::Answer
            } as i32,
            picks,
            note: String::new(),
            send_now: true,
        })
        .await;
    finish(&state, kind, &id, outcome).await
}

/// `POST /todos/picks`: saves the picks a card holds so far. A save the tool took answers
/// `204` and no patch, so the page that made it is not redrawn under the user; every other
/// page reads the todos again. A refusal redraws the page with the card's notice.
pub(crate) async fn post_picks(State(state): State<AppState>, body: Bytes) -> Response {
    let Some(form) = Form::parse(&body) else {
        return malformed();
    };
    let (Some(id), Some(version)) = (form.id(), form.version()) else {
        return malformed();
    };
    let Some(picks) = state.questions.partial_picks(&id, &form) else {
        return malformed();
    };
    let outcome = state
        .questions
        .save_picks(SavePicksRequest {
            id: id.clone(),
            version,
            picks,
        })
        .await;
    if outcome != Outcome::Sent {
        return finish(&state, Kind::Picks, &id, outcome).await;
    }
    log_post(Kind::Picks, outcome);
    state.questions.clear_notice(&id);
    state.todos_changed.send_modify(|posts| *posts += 1);
    StatusCode::NO_CONTENT.into_response()
}

/// `POST /todos/later`: sets a question aside.
pub(crate) async fn post_later(State(state): State<AppState>, body: Bytes) -> Response {
    let Some(id) = Form::parse(&body).and_then(|form| form.id()) else {
        return malformed();
    };
    let outcome = state.questions.defer(id.clone()).await;
    finish(&state, Kind::Later, &id, outcome).await
}

/// `POST /todos/tick`: ticks or unticks one step of instructions.
pub(crate) async fn post_tick(State(state): State<AppState>, body: Bytes) -> Response {
    let Some(form) = Form::parse(&body) else {
        return malformed();
    };
    let (Some(id), Some(version)) = (form.id(), form.version()) else {
        return malformed();
    };
    let Some(step) = form
        .get("step")
        .and_then(|step| step.parse::<u32>().ok())
        .filter(|step| *step >= 1)
    else {
        return malformed();
    };
    let ticked = form.get("ticked") == Some("true");
    let outcome = state
        .questions
        .tick(TickStepRequest {
            id: id.clone(),
            version,
            step,
            ticked,
        })
        .await;
    finish(&state, Kind::Tick, &id, outcome).await
}

/// `POST /todos/done` and `POST /todos/ignore`: closes instructions.
async fn close(state: AppState, body: Bytes, kind: Kind, answer: AnswerKind) -> Response {
    let Some(form) = Form::parse(&body) else {
        return malformed();
    };
    let (Some(id), Some(version)) = (form.id(), form.version()) else {
        return malformed();
    };
    let outcome = state
        .questions
        .answer(AnswerQuestionRequest {
            id: id.clone(),
            version,
            kind: answer as i32,
            picks: Vec::new(),
            note: String::new(),
            send_now: true,
        })
        .await;
    finish(&state, kind, &id, outcome).await
}

pub(crate) async fn post_done(State(state): State<AppState>, body: Bytes) -> Response {
    close(state, body, Kind::Done, AnswerKind::Done).await
}

pub(crate) async fn post_ignore(State(state): State<AppState>, body: Bytes) -> Response {
    close(state, body, Kind::Ignore, AnswerKind::Ignore).await
}
