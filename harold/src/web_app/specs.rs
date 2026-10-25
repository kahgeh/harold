//! The spec pages: each registered product's spec files, shown as pages in the web app and on
//! the specs listener, and the **Share** button that asks the chief to publish one spec.
//!
//! Harold finds the specs through the `Products` port, as the question tool does (ADR 0015):
//! the files `<repo>/.superpowers/pod/specs/<id>.md`. It resolves symlinks, refuses a file
//! outside that folder or over 1 MiB, and never shows a path. The page renders the Markdown with
//! raw HTML switched off. Mermaid is drawn by the page's own script (`specs.js`).

use std::convert::Infallible;
use std::fs::File;
use std::io::Read;
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use askama::Template;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::sse::{Event, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Router, middleware};
use datastar::prelude::PatchElements;
use pulldown_cmark::{CodeBlockKind, Event as Md, Options, Parser, Tag, TagEnd, html};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use tracing::info;

use super::{AppState, WebAppIdentity, origin, pages};
use crate::store::append_app_reply_with_photo;
use crate::telemetry;
use crate::tools::Products;

/// The folder inside a product's repo that holds its specs.
const SPECS_FOLDER: [&str; 3] = [".superpowers", "pod", "specs"];
/// A spec file larger than this is not shown.
pub(crate) const MAX_SPEC_BYTES: u64 = 1024 * 1024;
/// How much of a spec file the list reads to find its title.
const TITLE_BYTES: u64 = 4096;
/// The longest id taken.
const MAX_ID_CHARS: usize = 100;
/// How many hex digits of the checksum the page and the share reply show.
const SHA_SHOWN: usize = 12;

/// The registry file, read on every call so an edit shows at once. With no registry set
/// there are no products.
struct RegistryFile {
    path: Option<PathBuf>,
}

impl Products for RegistryFile {
    fn products(&self) -> Result<Vec<(String, PathBuf)>, &'static str> {
        let Some(path) = &self.path else {
            return Ok(Vec::new());
        };
        let products = crate::pod_stats::read_registry(path).map_err(|_| "registry_unreadable")?;
        Ok(products
            .into_iter()
            .map(|product| (product.name, product.repo))
            .collect())
    }
}

/// Which listener served a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Listener {
    WebApp,
    Specs,
}

impl Listener {
    fn word(self) -> &'static str {
        match self {
            Self::WebApp => "web_app",
            Self::Specs => "specs",
        }
    }
}

/// How a request for a spec page ended, as `app spec shown` logs it in `result`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    Ok,
    NotFound,
    Refused,
    Unreadable,
}

impl Outcome {
    fn word(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::NotFound => "not_found",
            Self::Refused => "refused",
            Self::Unreadable => "unreadable",
        }
    }
}

/// A spec that could not be shown, and the product and id to log: set only when the product is
/// in the registry and the id passes the id pattern, so no text from the URL is logged
/// otherwise.
#[derive(Debug)]
pub(crate) struct Failure {
    pub outcome: Outcome,
    pub named: Option<(String, String)>,
}

impl Failure {
    fn plain(outcome: Outcome) -> Self {
        Self {
            outcome,
            named: None,
        }
    }
}

/// One spec file, read.
#[derive(Debug)]
pub(crate) struct SpecFile {
    pub product: String,
    pub id: String,
    /// The file's SHA-256, in hex.
    pub sha: String,
    pub text: String,
}

impl SpecFile {
    /// The first 12 hex digits of the checksum, as the SPEC line shows them.
    pub(crate) fn sha_short(&self) -> &str {
        &self.sha[..SHA_SHOWN]
    }
}

/// One row of the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ListedSpec {
    pub product: String,
    pub id: String,
    pub title: String,
    modified: Option<SystemTime>,
}

/// The registry's specs.
pub(crate) struct Specs {
    products: Arc<dyn Products>,
}

impl Specs {
    /// The specs of the products in the registry at `registry`; none when it is not set.
    pub(crate) fn new(registry: Option<PathBuf>) -> Self {
        Self::with_products(Arc::new(RegistryFile { path: registry }))
    }

    pub(crate) fn with_products(products: Arc<dyn Products>) -> Self {
        Self { products }
    }

    /// Every product's specs, newest first within a product. The file system is read off
    /// the runtime's threads.
    pub(crate) async fn list(self: &Arc<Self>) -> Result<Vec<ListedSpec>, Outcome> {
        let specs = Arc::clone(self);
        tokio::task::spawn_blocking(move || list_blocking(&*specs.products))
            .await
            .unwrap_or(Err(Outcome::Unreadable))
    }

    /// One spec file, after the checks.
    pub(crate) async fn load(
        self: &Arc<Self>,
        product: &str,
        id: &str,
    ) -> Result<SpecFile, Failure> {
        let specs = Arc::clone(self);
        let (product, id) = (product.to_owned(), id.to_owned());
        tokio::task::spawn_blocking(move || load_blocking(&*specs.products, &product, &id))
            .await
            .unwrap_or(Err(Failure::plain(Outcome::Unreadable)))
    }
}

/// Whether `id` is a spec id: lowercase letters, digits and `-`.
pub(crate) fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.chars().count() <= MAX_ID_CHARS
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// The resolved specs folder of the product whose repo is `repo`.
fn specs_folder(repo: &FsPath) -> Option<PathBuf> {
    if !repo.is_absolute() {
        return None;
    }
    std::fs::canonicalize(
        SPECS_FOLDER
            .iter()
            .fold(repo.to_path_buf(), |p, part| p.join(part)),
    )
    .ok()
}

/// `O_NONBLOCK`, so opening a pipe in a regular file's place returns at once.
const O_NONBLOCK: i32 = if cfg!(target_os = "macos") {
    0x0004
} else {
    0o4000
};

fn open_without_blocking(path: &FsPath) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    File::options()
        .read(true)
        .custom_flags(O_NONBLOCK)
        .open(path)
}

fn load_blocking(products: &dyn Products, product: &str, id: &str) -> Result<SpecFile, Failure> {
    let listed = products
        .products()
        .map_err(|_| Failure::plain(Outcome::Unreadable))?;
    let Some((name, repo)) = listed.into_iter().find(|(name, _)| name == product) else {
        return Err(Failure::plain(Outcome::NotFound));
    };
    if !valid_id(id) {
        return Err(Failure::plain(Outcome::NotFound));
    }
    let named = Some((name.clone(), id.to_owned()));
    let fail = |outcome| Failure {
        outcome,
        named: named.clone(),
    };
    let folder = specs_folder(&repo).ok_or_else(|| fail(Outcome::NotFound))?;
    // Resolved at every call, so a symlink changed later cannot lead out.
    let resolved = std::fs::canonicalize(folder.join(format!("{id}.md")))
        .map_err(|_| fail(Outcome::NotFound))?;
    if !resolved.starts_with(&folder) {
        return Err(fail(Outcome::Refused));
    }
    // Opened before it is judged, and judged by the open handle, so what is read is what was
    // checked. A pipe swapped in cannot block the open, and is not a regular file.
    let file = open_without_blocking(&resolved).map_err(|_| fail(Outcome::NotFound))?;
    let metadata = file.metadata().map_err(|_| fail(Outcome::Unreadable))?;
    if !metadata.is_file() || metadata.len() > MAX_SPEC_BYTES {
        return Err(fail(Outcome::Refused));
    }
    let mut bytes = Vec::new();
    // One byte past the limit, so a file that grew since the stat is caught.
    file.take(MAX_SPEC_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| fail(Outcome::Unreadable))?;
    if bytes.len() as u64 > MAX_SPEC_BYTES {
        return Err(fail(Outcome::Refused));
    }
    Ok(SpecFile {
        product: name,
        id: id.to_owned(),
        sha: hex::encode(Sha256::digest(&bytes)),
        text: String::from_utf8_lossy(&bytes).into_owned(),
    })
}

fn list_blocking(products: &dyn Products) -> Result<Vec<ListedSpec>, Outcome> {
    let listed = products.products().map_err(|_| Outcome::Unreadable)?;
    let mut rows = Vec::new();
    for (product, repo) in listed {
        let Some(folder) = specs_folder(&repo) else {
            continue;
        };
        let Ok(entries) = std::fs::read_dir(&folder) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(id) = name
                .to_str()
                .and_then(|name| name.strip_suffix(".md"))
                .filter(|id| valid_id(id))
            else {
                continue;
            };
            // The same checks as a page: a symlink out of the folder is not listed.
            let Ok(resolved) = std::fs::canonicalize(entry.path()) else {
                continue;
            };
            if !resolved.starts_with(&folder) {
                continue;
            }
            let Ok(file) = open_without_blocking(&resolved) else {
                continue;
            };
            let Ok(metadata) = file.metadata() else {
                continue;
            };
            if !metadata.is_file() || metadata.len() > MAX_SPEC_BYTES {
                continue;
            }
            let mut head = Vec::new();
            let _ = file.take(TITLE_BYTES).read_to_end(&mut head);
            rows.push(ListedSpec {
                product: product.clone(),
                id: id.to_owned(),
                title: title_of(&String::from_utf8_lossy(&head)).unwrap_or_else(|| id.to_owned()),
                modified: metadata.modified().ok(),
            });
        }
    }
    // Newest first; the id breaks a tie so the order is stable.
    rows.sort_by(|a, b| {
        a.product
            .cmp(&b.product)
            .then(b.modified.cmp(&a.modified))
            .then(a.id.cmp(&b.id))
    });
    Ok(rows)
}

/// The text of the first level-1 heading.
fn title_of(text: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.strip_prefix("# "))
        .map(|title| title.trim().to_owned())
        .filter(|title| !title.is_empty())
}

/// Percent-encodes one path segment.
fn encode_segment(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char);
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// The path of a spec page.
pub(crate) fn spec_path(product: &str, id: &str) -> String {
    format!("/specs/{}/{}", encode_segment(product), encode_segment(id))
}

// ---------------------------------------------------------------------------
// Markdown
// ---------------------------------------------------------------------------

fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// A diagram block: its text escaped, which the page's script reads back as text.
fn mermaid_block(source: &str) -> String {
    format!("<pre class=\"mermaid\">{}</pre>\n", escape_html(source))
}

/// The text between `<pre class="mermaid">` and `</pre>` when `block` is only that.
fn raw_mermaid(block: &str) -> Option<&str> {
    const OPEN: &str = "<pre class=\"mermaid\">";
    const CLOSE: &str = "</pre>";
    let inner = block.trim().strip_prefix(OPEN)?.strip_suffix(CLOSE)?;
    (!inner.to_ascii_lowercase().contains("</pre")).then_some(inner)
}

/// Whether a Markdown link may keep its `href`: `http:`, `https:` or relative. Any other
/// scheme, a control character or a start of `//` or `\` does not.
fn link_allowed(dest: &str) -> bool {
    let dest = dest.trim();
    if dest.chars().any(char::is_control) || dest.starts_with("//") || dest.starts_with('\\') {
        return false;
    }
    let lower = dest.to_ascii_lowercase();
    match lower.find([':', '/', '?', '#']) {
        Some(at) if lower.as_bytes()[at] == b':' => {
            lower.starts_with("http:") || lower.starts_with("https:")
        }
        _ => true,
    }
}

/// The spec as safe HTML: raw HTML shown as text, a fenced `mermaid` block or a raw
/// `<pre class="mermaid">` block as a diagram, and a link kept only to `http:`, `https:` or a
/// relative address.
pub(crate) fn render_markdown(text: &str) -> String {
    let parser = Parser::new_ext(text, Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH);
    let mut events: Vec<Md> = Vec::new();
    let mut diagram: Option<String> = None;
    let mut raw: Option<String> = None;
    // For each open link: whether it lost its `href`, and so its tags.
    let mut links: Vec<bool> = Vec::new();
    for event in parser {
        if let Some(source) = diagram.as_mut() {
            match event {
                Md::Text(more) => source.push_str(&more),
                Md::End(TagEnd::CodeBlock) => {
                    events.push(Md::Html(mermaid_block(source).into()));
                    diagram = None;
                }
                _ => {}
            }
            continue;
        }
        if let Some(block) = raw.as_mut() {
            match event {
                Md::Html(more) | Md::Text(more) => block.push_str(&more),
                Md::End(TagEnd::HtmlBlock) => {
                    match raw_mermaid(block) {
                        Some(source) => events.push(Md::Html(mermaid_block(source).into())),
                        None => {
                            events.push(Md::Start(Tag::CodeBlock(CodeBlockKind::Indented)));
                            events.push(Md::Text(std::mem::take(block).into()));
                            events.push(Md::End(TagEnd::CodeBlock));
                        }
                    }
                    raw = None;
                }
                _ => {}
            }
            continue;
        }
        match event {
            Md::Start(Tag::CodeBlock(CodeBlockKind::Fenced(info)))
                if info.split_whitespace().next() == Some("mermaid") =>
            {
                diagram = Some(String::new());
            }
            Md::Start(Tag::HtmlBlock) => raw = Some(String::new()),
            // Inline raw HTML is shown as the text it is.
            Md::Html(raw_text) | Md::InlineHtml(raw_text) => events.push(Md::Text(raw_text)),
            Md::Start(Tag::Image { .. }) | Md::End(TagEnd::Image) => {}
            Md::Start(Tag::Link {
                link_type,
                dest_url,
                title,
                id,
            }) => {
                let keep = link_allowed(&dest_url);
                links.push(!keep);
                if keep {
                    events.push(Md::Start(Tag::Link {
                        link_type,
                        dest_url,
                        title,
                        id,
                    }));
                }
            }
            Md::End(TagEnd::Link) => {
                if links.pop() == Some(false) {
                    events.push(Md::End(TagEnd::Link));
                }
            }
            other => events.push(other),
        }
    }
    let mut out = String::new();
    html::push_html(&mut out, events.into_iter());
    out
}

// ---------------------------------------------------------------------------
// Pages
// ---------------------------------------------------------------------------

struct Group {
    product: String,
    rows: Vec<Row>,
}

struct Row {
    title: String,
    id: String,
    href: String,
}

#[derive(Template)]
#[template(path = "web_app/specs_list.html")]
struct SpecsListPage {
    app_name: String,
    /// Whether this is the web app, which has the manifest, the icon and the Datastar script.
    web_app: bool,
    has_icon: bool,
    groups: Vec<Group>,
    message: &'static str,
}

#[derive(Template)]
#[template(path = "web_app/spec.html")]
struct SpecPage {
    app_name: String,
    web_app: bool,
    has_icon: bool,
    product: String,
    id: String,
    sha: String,
    title: String,
    html: String,
    /// The **Share** button shows: the web app only.
    share: bool,
    share_path: String,
}

#[derive(Template)]
#[template(path = "web_app/spec_gone.html")]
struct GonePage {
    app_name: String,
    web_app: bool,
    message: &'static str,
}

fn gone(identity: &WebAppIdentity, listener: Listener, status: StatusCode) -> Response {
    let message = match status {
        StatusCode::NOT_FOUND => "No such spec.",
        _ => "Harold can't read the specs right now.",
    };
    let page = GonePage {
        app_name: identity.name.clone(),
        web_app: listener == Listener::WebApp,
        message,
    };
    match page.render() {
        Ok(html) => (status, Html(html)).into_response(),
        Err(_) => status.into_response(),
    }
}

async fn list_page(specs: &Arc<Specs>, identity: &WebAppIdentity, listener: Listener) -> Response {
    let (groups, message) = match specs.list().await {
        Err(_) => return gone(identity, listener, StatusCode::SERVICE_UNAVAILABLE),
        Ok(rows) if rows.is_empty() => (Vec::new(), "No specs on this machine"),
        Ok(rows) => {
            let mut groups: Vec<Group> = Vec::new();
            for row in rows {
                if groups
                    .last()
                    .is_none_or(|group| group.product != row.product)
                {
                    groups.push(Group {
                        product: row.product.clone(),
                        rows: Vec::new(),
                    });
                }
                if let Some(group) = groups.last_mut() {
                    group.rows.push(Row {
                        href: spec_path(&row.product, &row.id),
                        title: row.title,
                        id: row.id,
                    });
                }
            }
            (groups, "")
        }
    };
    let page = SpecsListPage {
        app_name: identity.name.clone(),
        web_app: listener == Listener::WebApp,
        has_icon: listener == Listener::WebApp && identity.icon.is_some(),
        groups,
        message,
    };
    match page.render() {
        Ok(html) => Html(html).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn spec_page(
    specs: &Arc<Specs>,
    identity: &WebAppIdentity,
    listener: Listener,
    product: &str,
    id: &str,
) -> Response {
    let started = Instant::now();
    let loaded = specs.load(product, id).await;
    let (outcome, named) = match &loaded {
        Ok(spec) => (Outcome::Ok, Some((spec.product.clone(), spec.id.clone()))),
        Err(failure) => (failure.outcome, failure.named.clone()),
    };
    let response = match loaded {
        Err(failure) => {
            let status = match failure.outcome {
                Outcome::Unreadable => StatusCode::SERVICE_UNAVAILABLE,
                _ => StatusCode::NOT_FOUND,
            };
            gone(identity, listener, status)
        }
        Ok(spec) => {
            let page = SpecPage {
                app_name: identity.name.clone(),
                web_app: listener == Listener::WebApp,
                has_icon: listener == Listener::WebApp && identity.icon.is_some(),
                title: title_of(&spec.text).unwrap_or_else(|| spec.id.clone()),
                html: render_markdown(&spec.text),
                sha: spec.sha_short().to_owned(),
                share: listener == Listener::WebApp,
                share_path: format!("{}/share", spec_path(&spec.product, &spec.id)),
                product: spec.product,
                id: spec.id,
            };
            match page.render() {
                Ok(html) => Html(html).into_response(),
                Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            }
        }
    };
    let ms = started.elapsed().as_millis() as u64;
    let listener = listener.word();
    let result = outcome.word();
    match named {
        Some((product, id)) => info!(listener, result, ms, product, id, "app spec shown"),
        None => info!(listener, result, ms, "app spec shown"),
    }
    response
}

/// `GET /specs` on the web app: each product's specs, newest first.
pub(crate) async fn get_list(State(state): State<AppState>) -> Response {
    list_page(&state.specs, &state.identity, Listener::WebApp).await
}

/// `GET /specs/{product}/{id}` on the web app: one spec page, with the **Share** button.
pub(crate) async fn get_spec(
    State(state): State<AppState>,
    Path((product, id)): Path<(String, String)>,
) -> Response {
    spec_page(
        &state.specs,
        &state.identity,
        Listener::WebApp,
        &product,
        &id,
    )
    .await
}

/// `POST /specs/{product}/{id}/share`: puts the reply `Share spec <product>/<id> sha <12>` in
/// the thread, as if you typed it, for the chief. The sha is the file's at this moment; the
/// request holds nothing the reply uses but the product and the id, and both must pass the
/// spec-file checks. Answers `404` and sends nothing for any that does not.
pub(crate) async fn post_share(
    State(state): State<AppState>,
    Path((product, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let spec = match state.specs.load(&product, &id).await {
        Ok(spec) => spec,
        Err(failure) if failure.outcome == Outcome::Unreadable => {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    let text = format!(
        "Share spec {}/{} sha {}",
        spec.product,
        spec.id,
        spec.sha_short()
    );
    // The phone's fetch span names itself in `traceparent`; the reply's trace joins it.
    let parent = super::phone_trace(&headers);
    let stored = telemetry::store_phone_reply_in(
        telemetry::WEB_APP_CHANNEL,
        OffsetDateTime::now_utc(),
        parent.as_ref(),
        async {
            let stored = append_app_reply_with_photo(&state.store, &text, None)
                .await
                .is_ok();
            // Logged inside the reply's span, so the line joins the reply's trace.
            if stored {
                info!(
                    product = %spec.product,
                    id = %spec.id,
                    sha = %spec.sha_short(),
                    "app spec shared"
                );
            }
            stored
        },
    )
    .await;
    if !stored {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let done = PatchElements::new(
        "<span id=\"share-spec\" class=\"share-done\" role=\"status\">Asked the chief to share it</span>",
    );
    Sse::new(tokio_stream::once(Ok::<Event, Infallible>(done.into()))).into_response()
}

// ---------------------------------------------------------------------------
// The specs listener
// ---------------------------------------------------------------------------

/// What the specs listener's routes can reach.
#[derive(Clone)]
pub(crate) struct SpecsState {
    pub specs: Arc<Specs>,
    pub identity: Arc<WebAppIdentity>,
    /// The Hosts answered: the listen address as written, as bound, and `localhost` with the
    /// port. Anything else is refused, which stops DNS rebinding.
    pub hosts: Arc<[String]>,
}

/// The specs listener's routes: `GET /specs`, `GET /specs/{product}/{id}` and three static
/// files. There is no login or pairing check, because a browser on the Mac sends no Tailscale
/// login; the Host check and the absence of any write take their place. Every other path, and
/// every method but `GET` and `HEAD`, gives `404`.
pub(crate) fn specs_router(state: SpecsState) -> Router {
    Router::new()
        .route("/specs", get(listener_list))
        .route("/specs/{product}/{id}", get(listener_spec))
        .route("/static/{name}", get(listener_static))
        .fallback(listener_not_found)
        // The last layer added runs first: the policy, then the Host check, then the method.
        .layer(middleware::from_fn(reads_only))
        .layer(middleware::from_fn_with_state(state.clone(), require_host))
        .layer(middleware::from_fn(pages::add_policy))
        .with_state(state)
}

async fn listener_list(State(state): State<SpecsState>) -> Response {
    list_page(&state.specs, &state.identity, Listener::Specs).await
}

async fn listener_spec(
    State(state): State<SpecsState>,
    Path((product, id)): Path<(String, String)>,
) -> Response {
    spec_page(
        &state.specs,
        &state.identity,
        Listener::Specs,
        &product,
        &id,
    )
    .await
}

async fn listener_static(Path(name): Path<String>) -> Response {
    pages::spec_static(&name).map_or_else(
        || StatusCode::NOT_FOUND.into_response(),
        |file| file.into_response(),
    )
}

async fn listener_not_found() -> StatusCode {
    StatusCode::NOT_FOUND
}

/// Refuses a request whose Host is not one of the listener's loopback names, with `403` and
/// an empty body.
async fn require_host(State(state): State<SpecsState>, request: Request, next: Next) -> Response {
    // HTTP/2 carries the Host in the request's authority instead of the header.
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .or_else(|| {
            request
                .uri()
                .authority()
                .map(|authority| authority.as_str())
        });
    let accepted = host.is_some_and(|host| {
        state
            .hosts
            .iter()
            .any(|accepted| accepted.eq_ignore_ascii_case(host))
    });
    if !accepted {
        return StatusCode::FORBIDDEN.into_response();
    }
    next.run(request).await
}

/// Answers `404` to any method but `GET` and `HEAD`, so no local process can write through
/// the listener.
async fn reads_only(request: Request, next: Next) -> Response {
    if !matches!(*request.method(), Method::GET | Method::HEAD) {
        return StatusCode::NOT_FOUND.into_response();
    }
    next.run(request).await
}

/// The Hosts the specs listener answers to.
pub(crate) fn listener_hosts(listen: &str, bound: std::net::SocketAddr) -> Vec<String> {
    origin::loopback_hosts(listen, bound)
}

#[cfg(test)]
#[path = "specs_tests.rs"]
mod tests;
