//! The Notes page: the vault notes changed most recently, a search box, and a vault note
//! read-only. It reads them through the knowledge tool's gRPC service on Harold's own
//! address, as the todo page reads the question tool's. The web app never calls into the
//! tool's module (ADR 0001).

use std::convert::Infallible;
use std::time::{Duration, Instant};

use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use axum::response::{Html, IntoResponse, Response};
use datastar::prelude::PatchElements;
use harold_api::tools::knowledge::v1::knowledge_service_client::KnowledgeServiceClient;
use harold_api::tools::knowledge::v1::{
    Hit, RecentRequest, SearchRequest, ShowVaultNoteRequest, VaultNote,
};
use pulldown_cmark::{Event as Md, Options, Parser, Tag, TagEnd, html};
use serde::Deserialize;
use tonic::Code;
use tonic::transport::{Channel, Endpoint};
use tracing::{info, warn};

use super::AppState;
use super::todos::client_url;
use crate::settings::GrpcSettings;

/// How many vault notes a list shows.
const ROWS: u32 = 10;
/// How long a read of the recent list or of one vault note may take.
const READ_LIMIT: Duration = Duration::from_secs(5);
/// How long a search may take: the first one after a pause waits for the model to load.
const SEARCH_LIMIT: Duration = Duration::from_secs(60);
/// The shortest query that searches.
const MIN_QUERY_CHARS: usize = 2;
/// The longest query read, in characters.
const MAX_QUERY_CHARS: usize = 200;

/// The knowledge tool's service on Harold's own gRPC address.
#[derive(Clone)]
pub(crate) struct Notes {
    client: KnowledgeServiceClient<Channel>,
}

impl Notes {
    /// A client that connects at its first call, so a tool that did not start fails that
    /// call and not the web app's start.
    pub(crate) fn lazy(grpc: &GrpcSettings) -> Self {
        let channel = Endpoint::from_shared(client_url(grpc))
            .expect("host and port make a URI")
            .connect_lazy();
        Self {
            client: KnowledgeServiceClient::new(channel),
        }
    }
}

/// One row of a list: a vault note's kind and title, linked to the vault note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RowView {
    pub kind: String,
    pub title: String,
    pub href: String,
}

impl From<Hit> for RowView {
    fn from(hit: Hit) -> Self {
        let title = if hit.title.is_empty() {
            hit.name.clone()
        } else {
            hit.title
        };
        Self {
            kind: hit.kind,
            title,
            href: format!("/notes/{}", encode_path(&hit.name)),
        }
    }
}

/// Percent-encodes a vault note's name for a path, keeping its `/`.
fn encode_path(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for byte in name.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(byte as char);
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// What the list under the search box says when it has no rows, or no tool to ask.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Message {
    None,
    NoNotes,
    NoMatches,
    Resting,
    Unavailable,
}

impl Message {
    fn text(self) -> &'static str {
        match self {
            Message::None => "",
            Message::NoNotes => "No notes yet.",
            Message::NoMatches => "No notes match.",
            Message::Resting => "Search is resting: the model didn't load.",
            Message::Unavailable => "Notes are not available right now.",
        }
    }
}

/// The list: a heading, then the rows or a message. The page draws it first and a search
/// replaces it.
#[derive(Template)]
#[template(path = "web_app/notes_list.html")]
pub(crate) struct NotesList {
    pub heading: &'static str,
    pub rows: Vec<RowView>,
    pub message: &'static str,
}

impl NotesList {
    fn recent(rows: Vec<RowView>) -> Self {
        let message = if rows.is_empty() {
            Message::NoNotes
        } else {
            Message::None
        };
        Self {
            heading: "Changed lately",
            rows,
            message: message.text(),
        }
    }

    fn found(rows: Vec<RowView>) -> Self {
        let message = if rows.is_empty() {
            Message::NoMatches
        } else {
            Message::None
        };
        Self {
            heading: "Closest notes",
            rows,
            message: message.text(),
        }
    }

    fn only(heading: &'static str, message: Message) -> Self {
        Self {
            heading,
            rows: Vec::new(),
            message: message.text(),
        }
    }
}

#[derive(Template)]
#[template(path = "web_app/notes.html")]
struct NotesPage {
    app_name: String,
    has_icon: bool,
    list: String,
}

#[derive(Template)]
#[template(path = "web_app/note.html")]
struct NotePage {
    app_name: String,
    has_icon: bool,
    kind: String,
    title: String,
    html: String,
}

/// The vault note's text as safe HTML: Markdown rendered with raw HTML switched off, its
/// `[[links]]` as plain words, links only to a web address, mail or a place in the page, and
/// no image.
pub(crate) fn render_markdown(text: &str) -> String {
    let text = plain_wiki_links(text);
    let parser = Parser::new_ext(
        &text,
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH,
    )
    .filter_map(|event| match event {
        // Raw HTML is shown as the text it is.
        Md::Html(raw) | Md::InlineHtml(raw) => Some(Md::Text(raw)),
        Md::Start(Tag::Image { .. }) | Md::End(TagEnd::Image) => None,
        Md::Start(Tag::Link {
            link_type,
            dest_url,
            title,
            id,
        }) => Some(Md::Start(Tag::Link {
            link_type,
            dest_url: if safe_link(&dest_url) {
                dest_url
            } else {
                "#".into()
            },
            title,
            id,
        })),
        other => Some(other),
    });
    let mut out = String::new();
    html::push_html(&mut out, parser);
    out
}

/// Whether a Markdown link may be followed: a web or mail address, or a place in the page.
fn safe_link(dest: &str) -> bool {
    let lower = dest.trim().to_ascii_lowercase();
    lower.starts_with("https://")
        || lower.starts_with("http://")
        || lower.starts_with("mailto:")
        || lower.starts_with('#')
}

/// `[[name]]`, `[[name|label]]` and `[[name#heading]]` as the words a reader would see.
fn plain_wiki_links(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find("[[") {
        let after = &rest[open + 2..];
        match after.find("]]") {
            Some(close) if !after[..close].contains('\n') => {
                out.push_str(&rest[..open]);
                let inner = &after[..close];
                let words = match inner.split_once('|') {
                    Some((_, label)) => label,
                    None => inner.split('#').next().unwrap_or(inner),
                };
                out.push_str(words.trim());
                rest = &after[close + 2..];
            }
            _ => {
                out.push_str(&rest[..open + 2]);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn not_found() -> Response {
    StatusCode::NOT_FOUND.into_response()
}

/// `GET /notes`: the page, with the 10 vault notes changed most recently.
pub(crate) async fn get_notes(State(state): State<AppState>) -> Response {
    let Some(notes) = state.notes.clone() else {
        return not_found();
    };
    let started = Instant::now();
    let list = match recent(&notes).await {
        Ok(rows) => NotesList::recent(rows),
        Err(_) => NotesList::only("Changed lately", Message::Unavailable),
    };
    let Ok(list) = list.render() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let page = NotesPage {
        app_name: state.identity.name.clone(),
        has_icon: state.identity.icon.is_some(),
        list,
    };
    let Ok(html) = page.render() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    info!(
        bytes = html.len(),
        ms = started.elapsed().as_millis() as u64,
        "app notes served"
    );
    Html(html).into_response()
}

async fn recent(notes: &Notes) -> Result<Vec<RowView>, Code> {
    let mut client = notes.client.clone();
    match tokio::time::timeout(READ_LIMIT, client.recent(RecentRequest { limit: ROWS })).await {
        Ok(Ok(response)) => Ok(response
            .into_inner()
            .hits
            .into_iter()
            .map(RowView::from)
            .collect()),
        Ok(Err(status)) => Err(status.code()),
        Err(_) => Err(Code::DeadlineExceeded),
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct SearchQuery {
    #[serde(default)]
    q: String,
}

/// `GET /notes/search?q=`: a Datastar patch of the list. A query under two characters gives
/// the recent list again; a search gives the closest vault notes, at most 10.
pub(crate) async fn get_search(
    State(state): State<AppState>,
    Query(query): Query<SearchQuery>,
) -> Response {
    let Some(notes) = state.notes.clone() else {
        return not_found();
    };
    let words: String = query.q.trim().chars().take(MAX_QUERY_CHARS).collect();
    let list = if words.chars().count() < MIN_QUERY_CHARS {
        match recent(&notes).await {
            Ok(rows) => NotesList::recent(rows),
            Err(_) => NotesList::only("Changed lately", Message::Unavailable),
        }
    } else {
        search(&notes, &words).await
    };
    let Ok(html) = list.render() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let patch: Event = PatchElements::new(html).into();
    Sse::new(tokio_stream::iter([Ok::<Event, Infallible>(patch)])).into_response()
}

async fn search(notes: &Notes, words: &str) -> NotesList {
    let mut client = notes.client.clone();
    let request = SearchRequest {
        query: words.to_owned(),
        limit: ROWS,
        source: "app".into(),
    };
    match tokio::time::timeout(SEARCH_LIMIT, client.search(request)).await {
        Ok(Ok(response)) => NotesList::found(
            response
                .into_inner()
                .hits
                .into_iter()
                .map(RowView::from)
                .collect(),
        ),
        Ok(Err(status)) if status.code() == Code::Unavailable => {
            if status.message().starts_with("search_resting") {
                NotesList::only("Closest notes", Message::Resting)
            } else {
                NotesList::only("Closest notes", Message::Unavailable)
            }
        }
        Ok(Err(_)) => NotesList::only("Closest notes", Message::Unavailable),
        Err(_) => {
            warn!("app notes search timed out");
            NotesList::only("Closest notes", Message::Unavailable)
        }
    }
}

/// `GET /notes/{name}`: one vault note, read-only.
pub(crate) async fn get_note(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    let Some(notes) = state.notes.clone() else {
        return not_found();
    };
    let mut client = notes.client.clone();
    let shown = tokio::time::timeout(
        READ_LIMIT,
        client.show_vault_note(ShowVaultNoteRequest { name }),
    )
    .await;
    let note: VaultNote = match shown {
        Ok(Ok(response)) => response.into_inner(),
        Ok(Err(status)) if status.code() == Code::NotFound => return not_found(),
        _ => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let title = if note.title.is_empty() {
        note.name.clone()
    } else {
        note.title.clone()
    };
    let page = NotePage {
        app_name: state.identity.name.clone(),
        has_icon: state.identity.icon.is_some(),
        kind: note.kind.clone(),
        title,
        html: render_markdown(&note.text),
    };
    match page.render() {
        Ok(html) => Html(html).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}
