use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use harold_api::tools::knowledge::v1 as pb;
use harold_api::tools::knowledge::v1::knowledge_service_server::{
    KnowledgeService, KnowledgeServiceServer,
};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tonic::{Request, Response, Status};

use super::notes::{Notes, render_markdown};
use super::origin::loopback_hosts;
use super::{AppState, router};
use crate::settings::GrpcSettings;
use crate::store::HaroldStore;

const LOGIN: &str = "chief@example.com";

fn hit(name: &str, kind: &str, title: &str) -> pb::Hit {
    pb::Hit {
        name: name.into(),
        title: title.into(),
        kind: kind.into(),
        description: format!("about {name}"),
        ..Default::default()
    }
}

#[derive(Default)]
struct Script {
    recent: Vec<pb::Hit>,
    found: Vec<pb::Hit>,
    search_refusal: Option<Status>,
    recent_fails: bool,
    note: Option<pb::VaultNote>,
    searches: Vec<pb::SearchRequest>,
}

#[derive(Clone, Default)]
struct Fake(Arc<Mutex<Script>>);

#[tonic::async_trait]
impl KnowledgeService for Fake {
    async fn search(
        &self,
        request: Request<pb::SearchRequest>,
    ) -> Result<Response<pb::SearchResponse>, Status> {
        let mut script = self.0.lock().unwrap();
        script.searches.push(request.into_inner());
        if let Some(refusal) = &script.search_refusal {
            return Err(Status::new(refusal.code(), refusal.message()));
        }
        Ok(Response::new(pb::SearchResponse {
            hits: script.found.clone(),
        }))
    }
    async fn recent(
        &self,
        request: Request<pb::RecentRequest>,
    ) -> Result<Response<pb::RecentResponse>, Status> {
        let script = self.0.lock().unwrap();
        assert_eq!(request.into_inner().limit, 10);
        if script.recent_fails {
            return Err(Status::unavailable("down"));
        }
        Ok(Response::new(pb::RecentResponse {
            hits: script.recent.clone(),
        }))
    }
    async fn related(
        &self,
        _: Request<pb::RelatedRequest>,
    ) -> Result<Response<pb::RelatedResponse>, Status> {
        Err(Status::unimplemented("related"))
    }
    async fn suggest_links(
        &self,
        _: Request<pb::SuggestLinksRequest>,
    ) -> Result<Response<pb::SuggestLinksResponse>, Status> {
        Err(Status::unimplemented("links"))
    }
    async fn show_vault_note(
        &self,
        request: Request<pb::ShowVaultNoteRequest>,
    ) -> Result<Response<pb::VaultNote>, Status> {
        let script = self.0.lock().unwrap();
        match &script.note {
            Some(note) if note.name == request.into_inner().name => Ok(Response::new(note.clone())),
            _ => Err(Status::not_found("unknown_vault_note")),
        }
    }
    async fn status(
        &self,
        _: Request<pb::StatusRequest>,
    ) -> Result<Response<pb::StatusResponse>, Status> {
        Err(Status::unimplemented("status"))
    }
    async fn rebuild(
        &self,
        _: Request<pb::RebuildRequest>,
    ) -> Result<Response<pb::RebuildResponse>, Status> {
        Err(Status::unimplemented("rebuild"))
    }
}

struct Served {
    fake: Fake,
    addr: SocketAddr,
    _shutdown: watch::Sender<()>,
    handles: Vec<JoinHandle<()>>,
    _folder: std::path::PathBuf,
}

impl Drop for Served {
    fn drop(&mut self) {
        for handle in &self.handles {
            handle.abort();
        }
        let _ = std::fs::remove_dir_all(&self._folder);
    }
}

/// The router with the Notes page on or off, over a fake knowledge service.
async fn serve(notes_on: bool) -> Served {
    let fake = Fake::default();
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);
    let service = KnowledgeServiceServer::new(fake.clone());
    let grpc_addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let grpc = tokio::spawn(async move {
        let _ = tonic::transport::Server::builder()
            .add_service(service)
            .serve(grpc_addr)
            .await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let folder = std::env::temp_dir().join(format!("harold-notes-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&folder).unwrap();
    let store = Arc::new(HaroldStore::open(folder.join("store")).await.unwrap());
    let (shutdown, shutdown_rx) = watch::channel(());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut state = AppState::for_tests(
        store,
        LOGIN,
        "BPublicKeyForTheTestPage",
        Arc::from(loopback_hosts("127.0.0.1:0", addr)),
        shutdown_rx,
        None,
        Default::default(),
    );
    if notes_on {
        state.notes = Some(Notes::lazy(&GrpcSettings {
            host: "127.0.0.1".into(),
            port,
        }));
    }
    let web = tokio::spawn(async move {
        let _ = axum::serve(listener, router(state)).await;
    });
    Served {
        fake,
        addr,
        _shutdown: shutdown,
        handles: vec![grpc, web],
        _folder: folder,
    }
}

impl Served {
    async fn get(&self, path: &str) -> reqwest::Response {
        reqwest::Client::new()
            .get(format!("http://{}{path}", self.addr))
            .header("Tailscale-User-Login", LOGIN)
            .send()
            .await
            .unwrap()
    }

    fn script(&self) -> std::sync::MutexGuard<'_, Script> {
        self.fake.0.lock().unwrap()
    }
}

#[tokio::test]
async fn the_page_lists_the_recent_vault_notes_by_kind_and_title_and_nothing_more() {
    let app = serve(true).await;
    app.script().recent = vec![
        hit(
            "a-knowledge-index-on-buckwal",
            "idea",
            "a-knowledge-index-on-buckwal",
        ),
        hit("references/hyperdx", "reference", "HyperDX"),
    ];
    let response = app.get("/notes").await;
    assert_eq!(response.status(), 200);
    let body = response.text().await.unwrap();
    assert!(
        body.contains(r#"<span class="note-kind">idea</span>"#),
        "{body}"
    );
    assert!(body.contains(r#"<span class="note-title">HyperDX</span>"#));
    assert!(body.contains(r#"href="/notes/references/hyperdx""#));
    assert!(body.contains("Changed lately"));
    assert!(body.contains(r#"type="search""#));
    assert!(body.contains("@get('/notes/search?q='"));
    assert!(
        body.contains("filterSignals: {include: /^none$/}"),
        "no signal rides along"
    );
    assert!(body.contains("input__debounce.300ms"));
    assert!(
        !body.contains("about references/hyperdx"),
        "no description on the page"
    );
}

#[tokio::test]
async fn the_page_says_when_there_are_no_notes_or_the_tool_is_down() {
    let app = serve(true).await;
    let empty = app.get("/notes").await.text().await.unwrap();
    assert!(empty.contains("No notes yet."));
    app.script().recent_fails = true;
    let down = app.get("/notes").await;
    assert_eq!(down.status(), 200);
    assert!(
        down.text()
            .await
            .unwrap()
            .contains("Notes are not available right now.")
    );
}

#[tokio::test]
async fn a_search_is_a_patch_of_the_list_with_at_most_ten_closest_notes() {
    let app = serve(true).await;
    app.script().found = vec![hit("a", "idea", "A note")];
    let response = app.get("/notes/search?q=keeping%20the%20index").await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    let body = response.text().await.unwrap();
    assert!(body.contains("datastar-patch-elements"), "{body}");
    assert!(body.contains("notes-list"));
    assert!(body.contains("Closest notes"));
    assert!(body.contains("A note"));
    let searches = app.script().searches.clone();
    assert_eq!(searches.len(), 1);
    assert_eq!(searches[0].query, "keeping the index");
    assert_eq!(searches[0].limit, 10);
    assert_eq!(searches[0].source, "app");
}

#[tokio::test]
async fn a_query_under_two_characters_keeps_the_recent_list_and_does_not_search() {
    let app = serve(true).await;
    app.script().recent = vec![hit("a", "idea", "Recent note")];
    for path in [
        "/notes/search?q=a",
        "/notes/search?q=",
        "/notes/search?q=%20%20",
        "/notes/search",
    ] {
        let body = app.get(path).await.text().await.unwrap();
        assert!(
            body.contains("Changed lately") && body.contains("Recent note"),
            "{path}: {body}"
        );
    }
    assert!(app.script().searches.is_empty());
}

#[tokio::test]
async fn a_search_that_finds_nothing_or_cannot_load_says_so() {
    let app = serve(true).await;
    let none = app.get("/notes/search?q=zzz").await.text().await.unwrap();
    assert!(none.contains("No notes match."));
    app.script().search_refusal = Some(Status::unavailable("search_resting: failed_to_load"));
    let resting = app.get("/notes/search?q=zzz").await.text().await.unwrap();
    assert!(
        resting.contains("Search is resting: the model didn&#"),
        "{resting}"
    );
    app.script().search_refusal = Some(Status::unavailable("store_failed"));
    let down = app.get("/notes/search?q=zzz").await.text().await.unwrap();
    assert!(down.contains("Notes are not available right now."));
}

#[tokio::test]
async fn a_vault_note_opens_read_only_with_markdown_rendered_and_raw_html_off() {
    let app = serve(true).await;
    app.script().note = Some(pb::VaultNote {
        name: "references/hyperdx".into(),
        title: "HyperDX".into(),
        kind: "reference".into(),
        text: "# Heading\n\nSee [[a-note]] and [[b-note|the label]].\n\n<script>alert(1)</script>\n\n[bad](javascript:alert(1)) [good](https://example.com)\n\n- one\n- two\n".into(),
        ..Default::default()
    });
    let response = app.get("/notes/references/hyperdx").await;
    assert_eq!(response.status(), 200);
    let body = response.text().await.unwrap();
    assert!(body.contains("<h3>HyperDX</h3>"));
    assert!(body.contains(r#"<p class="note-kind">reference</p>"#));
    assert!(body.contains("<h1>Heading</h1>"));
    assert!(body.contains("See a-note and the label."), "{body}");
    assert!(!body.contains("[["));
    assert!(!body.contains("<script>alert"), "{body}");
    assert!(body.contains("&lt;script&gt;"));
    assert!(!body.contains("javascript:"));
    assert!(body.contains(r#"href="https://example.com""#));
    assert!(body.contains("<li>one</li>"));
    assert!(body.contains(r#"href="/notes""#), "the back link");
    assert!(
        !body.contains("<form") && !body.contains("<textarea"),
        "read-only"
    );
}

#[tokio::test]
async fn an_unknown_vault_note_is_not_found() {
    let app = serve(true).await;
    assert_eq!(app.get("/notes/no-such-note").await.status(), 404);
}

#[tokio::test]
async fn without_the_knowledge_tool_there_is_no_notes_link_and_no_notes_route() {
    let off = serve(false).await;
    for path in ["/notes", "/notes/search?q=abc", "/notes/anything"] {
        assert_eq!(off.get(path).await.status(), 404, "{path}");
    }
    let thread = off.get("/").await.text().await.unwrap();
    assert!(!thread.contains(r#"id="to-notes""#));
    let on = serve(true).await;
    let thread = on.get("/").await.text().await.unwrap();
    assert!(
        thread.contains(r#"<a href="/notes" id="to-notes" class="todos-button">Notes</a>"#),
        "{thread}"
    );
}

#[test]
fn markdown_shows_links_between_vault_notes_as_plain_words_and_drops_images() {
    let html = render_markdown(
        "A [[one]], [[two|2]], [[three#h]] and ![alt text](https://x/y.png) and [[broken",
    );
    assert!(
        html.contains("A one, 2, three and alt text and [[broken"),
        "{html}"
    );
    assert!(!html.contains("<img"));
}

#[test]
fn markdown_tables_and_code_render() {
    let html = render_markdown("| a | b |\n|---|---|\n| 1 | 2 |\n\n```\nlet x = <b>;\n```\n");
    assert!(html.contains("<table>"));
    assert!(html.contains("&lt;b&gt;"));
}
