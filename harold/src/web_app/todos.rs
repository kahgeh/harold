//! The todo page: the question tool's todos, read and answered through its gRPC service
//! on Harold's own address, as the dash does. The web app never calls into the tool's
//! module (ADR 0001).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use askama::Template;
use harold_api::tools::questions::v1::question_service_client::QuestionServiceClient;
use harold_api::tools::questions::v1::{ListQuestionsRequest, ListedQuestion};
use tonic::transport::{Channel, Endpoint};
use tracing::{info, warn};

use crate::settings::GrpcSettings;

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

    /// The todos as last read, without reading.
    pub(crate) fn current(&self) -> TodosSection {
        Self::section(&self.shared.lock().unwrap_or_else(|e| e.into_inner()))
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
