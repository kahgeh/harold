//! Page collections: folders of `.html` and `.md` pages that Harold lists and serves at
//! `/c/<name>`. This file holds the checks, the list and the read. It never shows a path.
//!
//! Every check runs at each call, so a symlink or a file changed later cannot lead out of the
//! folder. The file system is read with blocking calls: the caller runs them off the runtime's
//! threads.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use askama::Template;
use axum::extract::rejection::PathRejection;
use axum::extract::{Path as UrlPath, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use time::{OffsetDateTime, UtcOffset};
use tracing::info;

use super::specs::{Listener, encode_segment, render_markdown};
use super::{AppState, WebAppIdentity};
use crate::settings::CollectionSettings;

/// A file larger than this is not listed and not served.
pub(crate) const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// The list shows this many files.
pub(crate) const MAX_ROWS: usize = 200;
/// How much of a file the list reads to find its title.
const TITLE_BYTES: u64 = 16 * 1024;
/// A longer title is cut to this many characters.
const TITLE_CHARS: usize = 200;

/// How a request for a collection page ended, as the log shows it in `result`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    Ok,
    NotFound,
    Refused,
    TooBig,
    Unreadable,
}

impl Outcome {
    pub(crate) fn word(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::NotFound => "not_found",
            Self::Refused => "refused",
            Self::TooBig => "too_big",
            Self::Unreadable => "unreadable",
        }
    }
}

/// A file type that a collection serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ext {
    Html,
    Md,
}

impl Ext {
    fn parse(text: &str) -> Option<Self> {
        match text {
            "html" => Some(Self::Html),
            "md" => Some(Self::Md),
            _ => None,
        }
    }
}

/// One configured collection.
#[derive(Debug, Clone)]
pub(crate) struct Collection {
    pub name: String,
    pub title: String,
    pub folder: PathBuf,
    pub extensions: Vec<Ext>,
    pub listed: bool,
}

impl Collection {
    /// The type of `name` when its extension is one of the collection's. A hidden name has none.
    fn ext_of(&self, name: &str) -> Option<Ext> {
        if name.starts_with('.') {
            return None;
        }
        let (_, ext) = name.rsplit_once('.')?;
        Ext::parse(ext).filter(|ext| self.extensions.contains(ext))
    }
}

/// The configured collections.
pub(crate) struct Collections {
    all: Vec<Collection>,
}

impl Collections {
    /// The collections of the settings, which were validated at start.
    pub(crate) fn new(settings: Vec<CollectionSettings>) -> Self {
        let all = settings
            .into_iter()
            .map(|s| Collection {
                name: s.name,
                title: s.title,
                folder: PathBuf::from(s.folder),
                extensions: s.extensions.iter().filter_map(|e| Ext::parse(e)).collect(),
                listed: s.listed,
            })
            .collect();
        Self { all }
    }

    pub(crate) fn get(&self, name: &str) -> Option<&Collection> {
        self.all.iter().find(|c| c.name == name)
    }

    /// The collections that the Pages screen shows.
    pub(crate) fn listed(&self) -> impl Iterator<Item = &Collection> {
        self.all.iter().filter(|c| c.listed)
    }

    pub(crate) fn names(&self) -> Vec<&str> {
        self.all.iter().map(|c| c.name.as_str()).collect()
    }
}

/// One row of the list. The texts are raw: the template escapes them.
#[derive(Debug)]
pub(crate) struct Row {
    /// The file name; the page does not show it.
    #[cfg_attr(not(test), allow(dead_code))]
    pub file: String,
    pub title: String,
    pub when: String,
    pub href: String,
}

/// The newest rows, and the number of files before the cut.
#[derive(Debug)]
pub(crate) struct Listing {
    pub rows: Vec<Row>,
    pub total: usize,
}

/// A file that passed the checks, read whole.
#[derive(Debug)]
pub(crate) struct LoadedFile {
    pub bytes: Vec<u8>,
    pub ext: Ext,
}

/// `O_NONBLOCK`, so opening a pipe in a regular file's place returns at once.
const O_NONBLOCK: i32 = if cfg!(target_os = "macos") {
    0x0004
} else {
    0o4000
};

fn open_without_blocking(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    File::options()
        .read(true)
        .custom_flags(O_NONBLOCK)
        .open(path)
}

/// One file of the folder that the list shows. No handle is kept: a folder of thousands of
/// files must not run out of file descriptors.
struct Found {
    name: String,
    ext: Ext,
    modified: SystemTime,
    resolved: PathBuf,
}

/// Whether the resolved file's own name passes the collection's rules: the served name counts,
/// so a symlink to a hidden file or to another extension is refused.
fn target_allowed(c: &Collection, resolved: &Path) -> bool {
    resolved
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| c.ext_of(n).is_some())
}

/// Every allowed, visible, plain file of at most 2 MiB that lies inside the folder, newest first.
fn scan(c: &Collection) -> Result<Vec<Found>, Outcome> {
    let folder = std::fs::canonicalize(&c.folder).map_err(|_| Outcome::Unreadable)?;
    let entries = std::fs::read_dir(&folder).map_err(|_| Outcome::Unreadable)?;
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let Some(ext) = c.ext_of(&name) else {
            continue;
        };
        // The same checks as a read: a symlink out of the folder is not listed.
        let Ok(resolved) = std::fs::canonicalize(entry.path()) else {
            continue;
        };
        if !resolved.starts_with(&folder) || !target_allowed(c, &resolved) {
            continue;
        }
        let Ok(metadata) = std::fs::metadata(&resolved) else {
            continue;
        };
        if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
            continue;
        }
        found.push(Found {
            name,
            ext,
            modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            resolved,
        });
    }
    // Newest first; the name breaks a tie so the order is stable.
    found.sort_by(|a, b| b.modified.cmp(&a.modified).then(a.name.cmp(&b.name)));
    Ok(found)
}

/// The newest 200 files with their titles, and how many files there are.
pub(crate) fn list_blocking(c: &Collection) -> Result<Listing, Outcome> {
    let found = scan(c)?;
    let total = found.len();
    let offset = crate::telemetry::local_offset();
    let rows = found
        .into_iter()
        .take(MAX_ROWS)
        .map(|f| {
            // One handle at a time, and only for the rows shown.
            let mut head = Vec::new();
            if let Ok(file) = open_without_blocking(&f.resolved) {
                let _ = file.take(TITLE_BYTES).read_to_end(&mut head);
            }
            let head = String::from_utf8_lossy(&head);
            let title = match f.ext {
                Ext::Html => title_of_html(&head),
                Ext::Md => title_of_md(&head),
            };
            Row {
                href: format!("/c/{}/{}", encode_segment(&c.name), encode_segment(&f.name)),
                when: format_when(f.modified, offset),
                title: title.unwrap_or_else(|| f.name.clone()),
                file: f.name,
            }
        })
        .collect();
    Ok(Listing { rows, total })
}

/// The number of files that the list would count: the badge on the Pages screen.
pub(crate) fn count_files(c: &Collection) -> Result<usize, Outcome> {
    scan(c).map(|found| found.len())
}

/// One file, after the checks. The checks that need no file access run first.
pub(crate) fn load_blocking(c: &Collection, file: &str) -> Result<LoadedFile, Outcome> {
    if file.contains(['/', '\\', '\0']) {
        return Err(Outcome::Refused);
    }
    let ext = c.ext_of(file).ok_or(Outcome::Refused)?;
    let folder = std::fs::canonicalize(&c.folder).map_err(|_| Outcome::Unreadable)?;
    let resolved = std::fs::canonicalize(folder.join(file)).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Outcome::NotFound
        } else {
            Outcome::Unreadable
        }
    })?;
    if !resolved.starts_with(&folder) || !target_allowed(c, &resolved) {
        return Err(Outcome::Refused);
    }
    // Opened before it is judged, and judged by the open handle, so what is read is what was
    // checked. A pipe swapped in cannot block the open, and is not a regular file.
    let handle = open_without_blocking(&resolved).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Outcome::NotFound
        } else {
            Outcome::Unreadable
        }
    })?;
    let metadata = handle.metadata().map_err(|_| Outcome::Unreadable)?;
    if !metadata.is_file() {
        return Err(Outcome::Refused);
    }
    if metadata.len() > MAX_FILE_BYTES {
        return Err(Outcome::TooBig);
    }
    let mut bytes = Vec::new();
    // One byte past the limit, so a file that grew since the stat is caught.
    handle
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Outcome::Unreadable)?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(Outcome::TooBig);
    }
    Ok(LoadedFile { bytes, ext })
}

/// A title in one line: white space runs become one space; cut to 200 characters.
fn tidy(text: &str) -> Option<String> {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!one_line.is_empty()).then(|| one_line.chars().take(TITLE_CHARS).collect())
}

/// The text of the `<title>` element in the first 16 KiB of an HTML file.
pub(crate) fn title_of_html(first16k: &str) -> Option<String> {
    // ASCII lower-casing keeps every byte offset.
    let lower = first16k.to_ascii_lowercase();
    let start = lower.find("<title>")? + "<title>".len();
    let end = start + lower[start..].find("</title>")?;
    tidy(&first16k[start..end])
}

/// The text of the first `# ` heading in the first 16 KiB of a Markdown file.
pub(crate) fn title_of_md(first16k: &str) -> Option<String> {
    first16k
        .lines()
        .find_map(|line| line.strip_prefix("# "))
        .and_then(tidy)
}

/// The date and time of a row, as `3 Nov 20:40`, at the given offset.
fn format_when(at: SystemTime, offset: UtcOffset) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let secs = match at.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_secs()).ok(),
        Err(before) => i64::try_from(before.duration().as_secs())
            .ok()
            .map(|secs| -secs),
    };
    let local = secs
        .and_then(|secs| OffsetDateTime::from_unix_timestamp(secs).ok())
        .and_then(|utc| utc.checked_to_offset(offset));
    let Some(local) = local else {
        return "-".to_owned();
    };
    format!(
        "{} {} {:02}:{:02}",
        local.day(),
        MONTHS[local.month() as usize - 1],
        local.hour(),
        local.minute()
    )
}

// ---------------------------------------------------------------------------
// Pages
// ---------------------------------------------------------------------------

/// One row of the Pages screen.
struct IndexRow {
    title: String,
    href: String,
    /// The page count; none for Specs and Notes, and for a collection that cannot be counted.
    badge: Option<usize>,
}

#[derive(Template)]
#[template(path = "web_app/collections_index.html")]
struct IndexPage {
    app_name: String,
    web_app: bool,
    has_icon: bool,
    rows: Vec<IndexRow>,
}

#[derive(Template)]
#[template(path = "web_app/collections.html")]
struct ListPage {
    app_name: String,
    web_app: bool,
    has_icon: bool,
    title: String,
    rows: Vec<Row>,
    message: &'static str,
    more: String,
}

#[derive(Template)]
#[template(path = "web_app/collection_page.html")]
struct PagePage {
    app_name: String,
    web_app: bool,
    has_icon: bool,
    title: String,
    collection_title: String,
    collection_path: String,
    file: String,
    markdown: bool,
    frame_src: String,
    html: String,
}

fn page_response(template: &impl Template, status: StatusCode) -> Response {
    match template.render() {
        Ok(html) => (status, Html(html)).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

fn status_of(outcome: Outcome) -> StatusCode {
    match outcome {
        Outcome::Ok => StatusCode::OK,
        Outcome::Unreadable => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::NOT_FOUND,
    }
}

/// A calm page for a list or a page that cannot be shown. It names no path.
fn gone(identity: &WebAppIdentity, listener: Listener, outcome: Outcome) -> Response {
    let message = match outcome {
        Outcome::Unreadable => "Harold can't read this folder right now.",
        _ => "No such page.",
    };
    let page = ListPage {
        app_name: identity.name.clone(),
        web_app: listener == Listener::WebApp,
        has_icon: listener == Listener::WebApp && identity.icon.is_some(),
        title: "Pages".to_owned(),
        rows: Vec::new(),
        message,
        more: String::new(),
    };
    page_response(&page, status_of(outcome))
}

/// Logs `app page shown` and counts the request once. `collection` is a configured name or
/// `unknown`, so no text from the URL reaches the log or the counter.
fn finish(
    started: Instant,
    listener: Listener,
    collection: &str,
    outcome: Outcome,
    response: Response,
) -> Response {
    let ms = started.elapsed().as_millis() as u64;
    let listener = listener.word();
    let result = outcome.word();
    info!(listener, collection, result, ms, "app page shown");
    crate::telemetry::count_collection_page(collection, result);
    response
}

/// The collection for a request, or the outcome that answers it: `refused` when the name did
/// not decode, `not_found` when no collection has it.
fn find<'a>(collections: &'a Collections, name: Option<&str>) -> Result<&'a Collection, Outcome> {
    let name = name.ok_or(Outcome::Refused)?;
    collections.get(name).ok_or(Outcome::NotFound)
}

async fn load(c: &Collection, file: &str) -> Result<LoadedFile, Outcome> {
    let (c, file) = (c.clone(), file.to_owned());
    tokio::task::spawn_blocking(move || load_blocking(&c, &file))
        .await
        .unwrap_or(Err(Outcome::Unreadable))
}

/// The Pages screen: each listed collection with its page count, then Specs, then Notes when
/// they are on. It logs and counts nothing.
pub(crate) async fn index_response(
    collections: &Arc<Collections>,
    identity: &WebAppIdentity,
    listener: Listener,
    notes_on: bool,
) -> Response {
    let all = Arc::clone(collections);
    let counted = tokio::task::spawn_blocking(move || {
        all.listed()
            .map(|c| (c.title.clone(), c.name.clone(), count_files(c).ok()))
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    let mut rows: Vec<IndexRow> = counted
        .into_iter()
        .map(|(title, name, badge)| IndexRow {
            title,
            href: format!("/c/{}", encode_segment(&name)),
            badge,
        })
        .collect();
    rows.push(IndexRow {
        title: "Specs".to_owned(),
        href: "/specs".to_owned(),
        badge: None,
    });
    rows.push(IndexRow {
        title: "Factory flow".to_owned(),
        href: "/flow".to_owned(),
        badge: None,
    });
    if notes_on {
        rows.push(IndexRow {
            title: "Notes".to_owned(),
            href: "/notes".to_owned(),
            badge: None,
        });
    }
    let page = IndexPage {
        app_name: identity.name.clone(),
        web_app: listener == Listener::WebApp,
        has_icon: listener == Listener::WebApp && identity.icon.is_some(),
        rows,
    };
    page_response(&page, StatusCode::OK)
}

/// One collection's list, newest first. `name` is none when the request's name did not decode.
pub(crate) async fn list_response(
    collections: &Arc<Collections>,
    identity: &WebAppIdentity,
    listener: Listener,
    name: Option<&str>,
) -> Response {
    let started = Instant::now();
    let c = match find(collections, name) {
        Ok(c) => c,
        Err(outcome) => {
            let response = gone(identity, listener, outcome);
            return finish(started, listener, "unknown", outcome, response);
        }
    };
    let folder = c.clone();
    let listed = tokio::task::spawn_blocking(move || list_blocking(&folder))
        .await
        .unwrap_or(Err(Outcome::Unreadable));
    let (outcome, response) = match listed {
        Err(outcome) => (outcome, gone(identity, listener, outcome)),
        Ok(listing) => {
            let more = if listing.total > listing.rows.len() {
                format!("{} newest of {}", listing.rows.len(), listing.total)
            } else {
                String::new()
            };
            let page = ListPage {
                app_name: identity.name.clone(),
                web_app: listener == Listener::WebApp,
                has_icon: listener == Listener::WebApp && identity.icon.is_some(),
                title: c.title.clone(),
                message: if listing.rows.is_empty() {
                    "No pages in this collection"
                } else {
                    ""
                },
                rows: listing.rows,
                more,
            };
            (Outcome::Ok, page_response(&page, StatusCode::OK))
        }
    };
    finish(started, listener, &c.name, outcome, response)
}

/// One collection page at `/c/{collection}/{file}`: the shell with a sandboxed frame for an
/// `.html` file, the spec page's template for an `.md` file.
pub(crate) async fn page_response_for(
    collections: &Arc<Collections>,
    identity: &WebAppIdentity,
    listener: Listener,
    request: Option<(&str, &str)>,
) -> Response {
    let started = Instant::now();
    let found = find(collections, request.map(|r| r.0));
    let (c, file) = match (found, request) {
        (Ok(c), Some((_, file))) => (c, file),
        (found, _) => {
            let outcome = found.err().unwrap_or(Outcome::Refused);
            let response = gone(identity, listener, outcome);
            return finish(started, listener, "unknown", outcome, response);
        }
    };
    let (outcome, response) = match load(c, file).await {
        Err(outcome) => (outcome, gone(identity, listener, outcome)),
        Ok(loaded) => {
            let text = String::from_utf8_lossy(&loaded.bytes);
            let markdown = loaded.ext == Ext::Md;
            let head = &text[..text.floor_char_boundary(TITLE_BYTES as usize)];
            let title = if markdown {
                title_of_md(&text)
            } else {
                title_of_html(head)
            };
            let page = PagePage {
                app_name: identity.name.clone(),
                web_app: listener == Listener::WebApp,
                has_icon: listener == Listener::WebApp && identity.icon.is_some(),
                title: title.unwrap_or_else(|| file.to_owned()),
                collection_title: c.title.clone(),
                collection_path: encode_segment(&c.name),
                file: file.to_owned(),
                markdown,
                frame_src: format!(
                    "/c/{}/{}/raw",
                    encode_segment(&c.name),
                    encode_segment(file)
                ),
                html: if markdown {
                    render_markdown(&text)
                } else {
                    String::new()
                },
            };
            (Outcome::Ok, page_response(&page, StatusCode::OK))
        }
    };
    finish(started, listener, &c.name, outcome, response)
}

/// The file of an `.html` page at `/c/{collection}/{file}/raw`, as it is. An `.md` file has no
/// raw form.
pub(crate) async fn raw_response(
    collections: &Arc<Collections>,
    listener: Listener,
    request: Option<(&str, &str)>,
) -> Response {
    let started = Instant::now();
    let found = find(collections, request.map(|r| r.0));
    let (c, file) = match (found, request) {
        (Ok(c), Some((_, file))) => (c, file),
        (found, _) => {
            let outcome = found.err().unwrap_or(Outcome::Refused);
            let response = status_of(outcome).into_response();
            return finish(started, listener, "unknown", outcome, response);
        }
    };
    let (outcome, response) = match load(c, file).await {
        Err(outcome) => (outcome, status_of(outcome).into_response()),
        Ok(loaded) if loaded.ext == Ext::Md => {
            (Outcome::NotFound, StatusCode::NOT_FOUND.into_response())
        }
        Ok(loaded) => (
            Outcome::Ok,
            (
                [
                    (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                    (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
                ],
                loaded.bytes,
            )
                .into_response(),
        ),
    };
    finish(started, listener, &c.name, outcome, response)
}

/// `GET /c` on the web app.
pub(crate) async fn get_index(State(state): State<AppState>) -> Response {
    index_response(
        &state.collections,
        &state.identity,
        Listener::WebApp,
        state.notes.is_some(),
    )
    .await
}

/// `GET /c/{collection}` on the web app.
pub(crate) async fn get_list(
    State(state): State<AppState>,
    name: Result<UrlPath<String>, PathRejection>,
) -> Response {
    let name = name.ok().map(|UrlPath(name)| name);
    list_response(
        &state.collections,
        &state.identity,
        Listener::WebApp,
        name.as_deref(),
    )
    .await
}

/// `GET /c/{collection}/{file}` on the web app.
pub(crate) async fn get_page(
    State(state): State<AppState>,
    request: Result<UrlPath<(String, String)>, PathRejection>,
) -> Response {
    let request = request.ok().map(|UrlPath(request)| request);
    page_response_for(
        &state.collections,
        &state.identity,
        Listener::WebApp,
        request.as_ref().map(|(c, f)| (c.as_str(), f.as_str())),
    )
    .await
}

/// `GET /c/{collection}/{file}/raw` on the web app.
pub(crate) async fn get_raw(
    State(state): State<AppState>,
    request: Result<UrlPath<(String, String)>, PathRejection>,
) -> Response {
    let request = request.ok().map(|UrlPath(request)| request);
    raw_response(
        &state.collections,
        Listener::WebApp,
        request.as_ref().map(|(c, f)| (c.as_str(), f.as_str())),
    )
    .await
}

#[cfg(test)]
#[path = "collections_tests.rs"]
mod tests;
