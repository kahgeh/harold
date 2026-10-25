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
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use datastar::prelude::{PatchElements, PatchSignals};
use harold_api::tools::questions::v1::question_service_client::QuestionServiceClient;
use harold_api::tools::questions::v1::{
    AnswerKind, AnswerQuestionRequest, ListQuestionsRequest, ListedQuestion, Pick,
    SavePicksRequest, SpecState, TickStepRequest,
};
use tonic::transport::{Channel, Endpoint};
use tonic::{Code, Status};
use tracing::{info, warn};

use super::AppState;
use crate::settings::GrpcSettings;

/// How long a write to the question tool may take.
const WRITE_LIMIT: Duration = Duration::from_secs(5);
/// The largest todo form read, in bytes.
const FORM_MAX_BYTES: usize = 32 * 1024;
/// The longest note for a revise or reject, in characters, as in the dash.
const NOTE_MAX_CHARS: usize = 1999;

/// How long one read of the todos may take, as the dash allows.
const READ_LIMIT: Duration = Duration::from_secs(2);

/// The checked line turns red this long after the last check, when a todo is waiting.
const STALE_AFTER_MS: i64 = 2 * 60 * 60 * 1000;

/// The notice on a card whose spec has changed since it was asked.
const CHANGED_SPEC_NOTICE: &str =
    "Changed spec: the spec changed after this was asked. The chief will put it again.";

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Wall-clock milliseconds since the Unix epoch.
type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

fn wall_clock() -> i64 {
    (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
}

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
    /// The last list a read returned, in the tool's order.
    last: Option<Vec<ListedQuestion>>,
    /// When the chief last checked the todos, as the last good read said; 0 before the first check.
    checked_at_ms: i64,
    /// True from the first failed read until the next good one.
    failing: bool,
    /// A refusal's words per todo id, kept until the todo is gone or next answered.
    notices: HashMap<String, String>,
    /// The version of each todo whose answer was refused for a changed spec, until the todo is
    /// gone or put again.
    changed_refusals: HashMap<String, u32>,
    /// The note box a refusal leaves open per todo id (`revise` or `reject`) and the note
    /// posted, kept until the todo is gone or next posted.
    open_notes: HashMap<String, (&'static str, String)>,
}

/// The URL of Harold's own gRPC address; an IPv6 host gets its brackets.
pub(super) fn client_url(grpc: &GrpcSettings) -> String {
    let addr = grpc.addr().expect("grpc host is an address");
    format!("http://{addr}")
}

/// The question tool's service on Harold's own gRPC address.
#[derive(Clone)]
pub(crate) struct Questions {
    pub(super) client: QuestionServiceClient<Channel>,
    shared: Arc<Mutex<Shared>>,
    /// The time the checked line is measured against; a test swaps it.
    clock: Clock,
}

impl Questions {
    /// A client that connects at its first call, so a tool that did not start fails that
    /// call and not the web app's start.
    pub(crate) fn lazy(grpc: &GrpcSettings) -> Self {
        let channel = Endpoint::from_shared(client_url(grpc))
            .expect("host and port make a URI")
            .connect_lazy();
        Self {
            client: QuestionServiceClient::new(channel),
            shared: Arc::default(),
            clock: Arc::new(wall_clock),
        }
    }

    /// The same todos, with the checked line measured against `clock`.
    #[cfg(test)]
    pub(super) fn with_clock(mut self, clock: impl Fn() -> i64 + Send + Sync + 'static) -> Self {
        self.clock = Arc::new(clock);
        self
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
                Ok(Ok(response)) => {
                    let response = response.into_inner();
                    Ok((response.questions, response.checked_at_ms))
                }
                Ok(Err(_)) => Err(ReadFailure::Unavailable),
                Err(_) => Err(ReadFailure::Timeout),
            };
        let mut shared = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        match read {
            Ok((list, checked_at_ms)) => {
                shared.checked_at_ms = checked_at_ms;
                if shared.failing {
                    shared.failing = false;
                    info!("app todos read again");
                }
                let ids: Vec<&str> = list
                    .iter()
                    .filter_map(|l| l.question.as_ref().map(|q| q.id.as_str()))
                    .collect();
                shared.notices.retain(|id, _| ids.contains(&id.as_str()));
                // A changed-spec refusal holds only for the version it was made at: the chief
                // putting the todo again takes it off.
                shared.changed_refusals.retain(|id, version| {
                    list.iter().any(|listed| {
                        listed.version == *version
                            && listed.question.as_ref().is_some_and(|q| &q.id == id)
                    })
                });
                shared.open_notes.retain(|id, _| ids.contains(&id.as_str()));
                shared.last = Some(list);
            }
            Err(failure) => {
                if !shared.failing {
                    shared.failing = true;
                    warn!(reason = failure.reason(), "app todos not read");
                }
            }
        }
        self.section(&shared)
    }

    /// Shows `text` on the card of todo `id` until it is gone or answered.
    pub(super) fn set_notice(&self, id: &str, text: &str) {
        let mut shared = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        shared.notices.insert(id.to_owned(), text.to_owned());
    }

    /// Keeps the changed-spec notice on todo `id` at the version last read, even if the list
    /// has not marked the spec as changed yet.
    fn refused_for_changed_spec(&self, id: &str) {
        let Some(version) = self.find(id).map(|listed| listed.version) else {
            return;
        };
        let mut shared = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        shared.changed_refusals.insert(id.to_owned(), version);
    }

    /// Leaves the note box of todo `id` open on `mode` (`revise` or `reject`), holding `note`.
    fn open_note(&self, id: &str, mode: &'static str, note: &str) {
        let mut shared = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        shared
            .open_notes
            .insert(id.to_owned(), (mode, note.to_owned()));
    }

    /// Closes the note box of todo `id`.
    fn close_note(&self, id: &str) {
        let mut shared = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        shared.open_notes.remove(id);
    }

    /// Takes the notice off the card of todo `id`.
    pub(super) fn clear_notice(&self, id: &str) {
        let mut shared = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        shared.notices.remove(id);
    }

    fn section(&self, shared: &Shared) -> TodosSection {
        match &shared.last {
            None => TodosSection {
                unreadable: true,
                checked: None,
                cards: Vec::new(),
                waiting: 0,
            },
            Some(list) => TodosSection {
                unreadable: false,
                checked: (!list.is_empty()).then(|| {
                    CheckedLine::of(
                        shared.checked_at_ms,
                        (self.clock)(),
                        crate::telemetry::local_offset(),
                    )
                }),
                cards: list
                    .iter()
                    .filter_map(|listed| {
                        Card::from_listed(
                            listed,
                            &shared.notices,
                            &shared.changed_refusals,
                            &shared.open_notes,
                        )
                    })
                    .collect(),
                waiting: list.len(),
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
    /// The checked line; none when no todo is listed.
    checked: Option<CheckedLine>,
    cards: Vec<Card>,
    /// The todo count: the waiting questions and the instructions.
    waiting: usize,
}

/// The line at the top of the page saying when the chief last checked the todos.
pub(super) struct CheckedLine {
    pub(super) text: String,
    /// Over 2 hours since the check, or none yet: the line is red.
    pub(super) stale: bool,
}

impl CheckedLine {
    /// The line for a check at `checked_at_ms` (0 for none), seen at `now_ms`, in the Mac's
    /// `offset`. It is only drawn while a todo is listed, so a todo is waiting.
    pub(super) fn of(checked_at_ms: i64, now_ms: i64, offset: time::UtcOffset) -> Self {
        if checked_at_ms <= 0 {
            return Self {
                text: "Not checked yet".to_owned(),
                stale: true,
            };
        }
        let local = |ms: i64| {
            time::OffsetDateTime::from_unix_timestamp(ms.div_euclid(1000))
                .unwrap_or(time::OffsetDateTime::UNIX_EPOCH)
                .to_offset(offset)
        };
        let (checked, now) = (local(checked_at_ms), local(now_ms));
        let clock = format!("{:02}:{:02}", checked.hour(), checked.minute());
        let day = if checked.date() == now.date() || checked.date() > now.date() {
            Day::Today
        } else if now.date().previous_day() == Some(checked.date()) {
            Day::Yesterday
        } else {
            Day::On(format!(
                "{} {}",
                checked.day(),
                MONTHS[usize::from(u8::from(checked.month())) - 1]
            ))
        };
        let stale = now_ms.saturating_sub(checked_at_ms) > STALE_AFTER_MS;
        let text = match (stale, day) {
            (false, Day::Today) => format!("Checked by the chief at {clock}"),
            (false, Day::Yesterday) => format!("Checked by the chief yesterday at {clock}"),
            (false, Day::On(date)) => format!("Checked by the chief on {date} at {clock}"),
            (true, Day::Today) => format!("Not checked since {clock}"),
            (true, Day::Yesterday) => format!("Not checked since yesterday at {clock}"),
            (true, Day::On(date)) => format!("Not checked since {date} at {clock}"),
        };
        Self { text, stale }
    }
}

enum Day {
    Today,
    Yesterday,
    On(String),
}

/// Whether the todo's spec file no longer matches the checksum it was asked with.
fn has_changed_spec(listed: &ListedQuestion) -> bool {
    SpecState::try_from(listed.spec_state) == Ok(SpecState::Changed)
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
    heading: String,
    under: Option<String>,
    link: Option<CardLink>,
    decisions: Vec<DecisionView>,
    steps: Vec<StepView>,
    notice: Option<String>,
    /// The spec changed after the todo was asked: Send, Done and Ignore are off, and Revise,
    /// Reject and Relegate to idea are left out.
    changed: bool,
    /// Send is off: with a note open, until it has words; otherwise until every decision has a pick.
    send_disabled: bool,
    /// The card's name in the page's signals: the id's letters and digits.
    sig: String,
    /// `revise`, `reject` or `relegate` while a refusal leaves the note box open, else empty.
    open_mode: &'static str,
    /// The note a refusal left in the box.
    note: String,
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
    fn from_listed(
        listed: &ListedQuestion,
        notices: &HashMap<String, String>,
        changed_refusals: &HashMap<String, u32>,
        open_notes: &HashMap<String, (&'static str, String)>,
    ) -> Option<Self> {
        let question = listed.question.as_ref()?;
        let changed = has_changed_spec(listed);
        let refused_for_change = changed_refusals.get(&question.id) == Some(&listed.version);
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
            version: listed.version,
            is_question,
            kind: if is_question {
                "question"
            } else {
                "instructions"
            },
            product: question.product.clone(),
            heading,
            under,
            link,
            decisions,
            steps,
            notice: if changed || refused_for_change {
                Some(CHANGED_SPEC_NOTICE.to_owned())
            } else {
                notices.get(&question.id).cloned()
            },
            changed,
            send_disabled: changed
                || match open_notes.get(&question.id) {
                    Some((_, note)) => note.trim().is_empty(),
                    None => !can_send,
                },
            sig: signal_name(&question.id),
            open_mode: open_notes
                .get(&question.id)
                .filter(|_| !changed)
                .map_or("", |(mode, _)| mode),
            note: open_notes
                .get(&question.id)
                .filter(|_| !changed)
                .map(|(_, note)| note.clone())
                .unwrap_or_default(),
            id: question.id.clone(),
        })
    }
}

/// A todo id as a signal name: its ASCII letters and digits.
fn signal_name(id: &str) -> String {
    id.chars().filter(char::is_ascii_alphanumeric).collect()
}

// ---------------------------------------------------------------------------
// The posts
// ---------------------------------------------------------------------------

/// What a post did, as `app todo answered` logs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Answer,
    FreeText,
    Picks,
    Tick,
    Done,
    Ignore,
    Revise,
    Reject,
    Relegate,
}

impl Kind {
    fn word(self) -> &'static str {
        match self {
            Self::Answer => "answer",
            Self::FreeText => "free_text",
            Self::Picks => "picks",
            Self::Tick => "tick",
            Self::Done => "done",
            Self::Ignore => "ignore",
            Self::Revise => "revise",
            Self::Reject => "reject",
            Self::Relegate => "relegate",
        }
    }

    /// The word in the sent line, for a kind that answers a todo.
    fn sent_word(self) -> Option<&'static str> {
        match self {
            Self::Answer | Self::FreeText => Some("answer"),
            Self::Done | Self::Ignore | Self::Revise | Self::Reject | Self::Relegate => {
                Some(self.word())
            }
            Self::Picks | Self::Tick => None,
        }
    }
}

/// How the question tool answered a write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Sent,
    VersionChanged,
    NotWaiting,
    SpecChanged,
    Unavailable,
}

impl Outcome {
    fn word(self) -> &'static str {
        match self {
            Self::Sent => "sent",
            Self::VersionChanged => "version_changed",
            Self::SpecChanged => "spec_changed",
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
                    "spec_changed" => Self::SpecChanged,
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
            // Drawn from the todo's own spec state, and from `changed_refusals`.
            Self::SpecChanged => None,
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
    finish_with_note(state, kind, id, outcome, "").await
}

/// As `finish`, and a refused revise, reject or relegate leaves the note box open holding `note`.
async fn finish_with_note(
    state: &AppState,
    kind: Kind,
    id: &str,
    outcome: Outcome,
    note: &str,
) -> Response {
    log_post(kind, outcome);
    match outcome.notice(id, kind) {
        Some(notice) => state.questions.set_notice(id, &notice),
        None => state.questions.clear_notice(id),
    }
    if outcome == Outcome::SpecChanged {
        state.questions.refused_for_changed_spec(id);
    }
    state.questions.close_note(id);
    let mode = match kind {
        // A changed spec leaves no note box to keep open: Revise, Reject and Relegate are gone
        // with it.
        Kind::Revise | Kind::Reject | Kind::Relegate
            if !matches!(outcome, Outcome::Sent | Outcome::SpecChanged) =>
        {
            Some(kind.word())
        }
        _ => None,
    };
    if let Some(mode) = mode {
        state.questions.open_note(id, mode, note);
    }
    // The sent line lives in the posting page's signals: set by an answer the tool took,
    // emptied by every other post.
    let sent = kind
        .sent_word()
        .filter(|_| outcome == Outcome::Sent)
        .map(|word| format!("{id}: {word} sent to the chief."))
        .unwrap_or_default();
    let section = state.questions.refresh().await;
    state.todos_changed.send_modify(|posts| *posts += 1);
    let page = section.render().unwrap_or_default();
    let count = section.count().render().unwrap_or_default();
    let patch: Event = PatchElements::new(format!("{page}{count}")).into();
    let mut signals = serde_json::json!({ "sent": sent });
    if let Some(mode) = mode {
        // The page that posted keeps its note box open on the words it posted.
        signals["t"] = serde_json::json!({
            signal_name(id): { "mode": mode, "len": note.chars().count() }
        });
    }
    let events = vec![
        Ok::<Event, Infallible>(patch),
        Ok(PatchSignals::new(signals.to_string()).into()),
    ];
    Sse::new(tokio_stream::iter(events)).into_response()
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
pub(crate) async fn post_answer(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
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
            trace_parent: super::phone_traceparent(&headers),
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
            trace_parent: String::new(),
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

/// `POST /todos/revise`, `POST /todos/reject` and `POST /todos/relegate`: the picks so far
/// and a note, sent to the chief at once. A note over `NOTE_MAX_CHARS` gives `400`, and so
/// does a blank one, except for a relegate, whose note is optional.
async fn answer_with_note(
    state: AppState,
    headers: HeaderMap,
    body: Bytes,
    kind: Kind,
    answer: AnswerKind,
) -> Response {
    let Some(form) = Form::parse(&body) else {
        return malformed();
    };
    let (Some(id), Some(version)) = (form.id(), form.version()) else {
        return malformed();
    };
    let note = form.get("note").unwrap_or("").trim();
    if (note.is_empty() && kind != Kind::Relegate) || note.chars().count() > NOTE_MAX_CHARS {
        return malformed();
    }
    let Some(picks) = state.questions.partial_picks(&id, &form) else {
        return malformed();
    };
    let outcome = state
        .questions
        .answer(AnswerQuestionRequest {
            id: id.clone(),
            version,
            kind: answer as i32,
            picks,
            note: note.to_owned(),
            send_now: true,
            trace_parent: super::phone_traceparent(&headers),
        })
        .await;
    finish_with_note(&state, kind, &id, outcome, note).await
}

pub(crate) async fn post_revise(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    answer_with_note(state, headers, body, Kind::Revise, AnswerKind::Revise).await
}

pub(crate) async fn post_reject(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    answer_with_note(state, headers, body, Kind::Reject, AnswerKind::Reject).await
}

pub(crate) async fn post_relegate(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    answer_with_note(state, headers, body, Kind::Relegate, AnswerKind::Relegate).await
}
