use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::{Specs, SpecsState, listener_hosts, render_markdown, specs_router, valid_id};
use crate::outbound::fake::CapturedLogs;
use crate::settings::GrpcSettings;
use crate::store::{EventVersion, HaroldStore};
use crate::tools::Products;
use crate::web_app::notes::Notes;
use crate::web_app::origin::loopback_hosts;
use crate::web_app::pages::{CONTENT_SECURITY_POLICY, SPEC_CONTENT_SECURITY_POLICY};
use crate::web_app::{AppState, WebAppIdentity, router};

const LOGIN: &str = "chief@example.com";

struct FakeProducts(Vec<(String, PathBuf)>);

impl Products for FakeProducts {
    fn products(&self) -> Result<Vec<(String, PathBuf)>, &'static str> {
        Ok(self.0.clone())
    }
}

struct Repo {
    root: PathBuf,
}

impl Repo {
    fn new() -> Self {
        let root = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("harold-specs-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("repo/.superpowers/pod/specs")).unwrap();
        Self { root }
    }

    fn repo(&self) -> PathBuf {
        self.root.join("repo")
    }

    fn folder(&self) -> PathBuf {
        self.repo().join(".superpowers/pod/specs")
    }

    fn write(&self, name: &str, text: &str) {
        std::fs::write(self.folder().join(name), text).unwrap();
    }

    fn specs(&self) -> Arc<Specs> {
        Arc::new(Specs::with_products(Arc::new(FakeProducts(vec![(
            "harold".into(),
            self.repo(),
        )]))))
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

const SPEC: &str = "Waiting for your approval.\n\n# serve-specs: Serve the specs\n\nA paragraph with <b>raw</b> HTML.\n\n<pre class=\"mermaid\">\nflowchart LR\n  A[one<br/>two] --> B\n</pre>\n\n```mermaid\nsequenceDiagram\n  A->>B: hi\n```\n\n[web](https://example.com/a) [plain](http://example.com/b) [rel](other.md) [frag](#top) [bad](javascript:alert(1)) [mixed](JaVa\tScRiPt:alert(1)) [data](data:text/html,x) [proto](//evil.example/x)\n";

fn sha12(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))[..12].to_owned()
}

struct Served {
    addr: SocketAddr,
    handle: JoinHandle<()>,
    _shutdown: watch::Sender<()>,
}

impl Served {
    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn web_app(specs: Arc<Specs>, store: Arc<HaroldStore>) -> Served {
    web_app_with_notes(specs, store, false).await
}

async fn web_app_with_notes(specs: Arc<Specs>, store: Arc<HaroldStore>, notes_on: bool) -> Served {
    let (shutdown, shutdown_rx) = watch::channel(());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut state = AppState::for_tests(
        store,
        LOGIN,
        "BPublicKey",
        Arc::from(loopback_hosts("127.0.0.1:0", addr)),
        shutdown_rx,
        None,
        WebAppIdentity::default(),
    );
    state.specs = specs;
    if notes_on {
        state.notes = Some(Notes::lazy(&GrpcSettings {
            host: "127.0.0.1".into(),
            port: 1,
        }));
    }
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router(state)).await;
    });
    Served {
        addr,
        handle,
        _shutdown: shutdown,
    }
}

async fn specs_listener(specs: Arc<Specs>) -> Served {
    let (shutdown, _) = watch::channel(());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = specs_router(SpecsState {
        specs,
        identity: Arc::new(WebAppIdentity::default()),
        hosts: Arc::from(listener_hosts("127.0.0.1:4331", addr)),
    });
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    Served {
        addr,
        handle,
        _shutdown: shutdown,
    }
}

async fn store(directory: &Path) -> Arc<HaroldStore> {
    Arc::new(HaroldStore::open(directory.join("store")).await.unwrap())
}

async fn get(server: &Served, path: &str) -> reqwest::Response {
    reqwest::Client::new()
        .get(server.url(path))
        .header("Tailscale-User-Login", LOGIN)
        .send()
        .await
        .unwrap()
}

async fn replies(store: &HaroldStore) -> Vec<String> {
    store.project_unhandled_events(100).await.unwrap();
    store
        .app_messages_since(0)
        .await
        .unwrap()
        .into_iter()
        .map(|message| message.text)
        .collect()
}

async fn event_count(store: &HaroldStore) -> usize {
    store
        .stream()
        .load_after_version(EventVersion::start(), 100)
        .await
        .unwrap()
        .len()
}

#[test]
fn an_id_is_lowercase_letters_digits_and_dashes() {
    for good in ["serve-specs", "a1", "x-2-y"] {
        assert!(valid_id(good), "{good}");
    }
    for bad in [
        "",
        "A",
        ".revisions",
        "a.b",
        "a/b",
        "a b",
        "a_b",
        "../x",
        &"a".repeat(101),
    ] {
        assert!(!valid_id(bad), "{bad}");
    }
}

#[test]
fn markdown_shows_raw_html_as_text_and_draws_diagram_blocks() {
    let html = render_markdown(SPEC);
    assert!(html.contains("&lt;b&gt;raw&lt;/b&gt;"), "{html}");
    assert!(!html.contains("<b>"), "{html}");
    // A raw <pre class="mermaid"> block and a fenced block, each with its text escaped.
    assert_eq!(html.matches("<pre class=\"mermaid\">").count(), 2, "{html}");
    assert!(html.contains("A[one&lt;br/&gt;two] --&gt; B"), "{html}");
    assert!(html.contains("A-&gt;&gt;B: hi"), "{html}");
    assert!(!html.contains("<br/>"), "{html}");
}

#[test]
fn markdown_keeps_an_href_only_for_http_https_and_relative_links() {
    let html = render_markdown(SPEC);
    for kept in [
        r#"href="https://example.com/a""#,
        r#"href="http://example.com/b""#,
        r#"href="other.md""#,
        r##"href="#top""##,
    ] {
        assert!(html.contains(kept), "{kept} in {html}");
    }
    assert_eq!(html.matches("<a ").count(), 4, "{html}");
    for text in ["bad", "mixed", "data", "proto"] {
        assert!(html.contains(text), "{text} in {html}");
    }
    for refused in ["javascript", "JaVa", "data:", "evil.example"] {
        assert!(!html.contains(&format!("href=\"{refused}")), "{refused}");
    }
}

#[test]
fn an_other_raw_html_block_shows_as_text() {
    let html = render_markdown("<div onclick=\"x()\">\nhello\n</div>\n");
    assert!(html.contains("&lt;div onclick="), "{html}");
    assert!(!html.contains("<div"), "{html}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_spec_page_shows_its_title_sha_and_diagrams_with_the_share_button() {
    let repo = Repo::new();
    repo.write("serve-specs.md", SPEC);
    let directory = Repo::new();
    let server = web_app(repo.specs(), store(&directory.root).await).await;

    let response = get(&server, "/specs/harold/serve-specs").await;

    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()["content-security-policy"],
        SPEC_CONTENT_SECURITY_POLICY
    );
    let body = response.text().await.unwrap();
    assert!(
        body.contains("<title>serve-specs: Serve the specs</title>"),
        "{body}"
    );
    assert!(
        body.contains(&format!("harold · serve-specs · sha {}", sha12(SPEC))),
        "{body}"
    );
    assert!(body.contains("<pre class=\"mermaid\">"), "{body}");
    assert!(body.contains("id=\"share-spec\""), "{body}");
    assert!(body.contains("/static/specs.js"), "{body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_spec_pages_carry_their_own_policy_and_every_other_page_keeps_the_app_policy() {
    let repo = Repo::new();
    repo.write("a.md", "# A\n");
    let directory = Repo::new();
    let server = web_app(repo.specs(), store(&directory.root).await).await;

    for path in ["/specs/harold/a", "/specs/harold/missing"] {
        let response = get(&server, path).await;
        assert_eq!(
            response.headers()["content-security-policy"],
            SPEC_CONTENT_SECURITY_POLICY,
            "{path}"
        );
    }
    for path in ["/", "/specs", "/static/app.css", "/nope"] {
        let response = get(&server, path).await;
        assert_eq!(
            response.headers()["content-security-policy"],
            CONTENT_SECURITY_POLICY,
            "{path}"
        );
    }
    assert!(SPEC_CONTENT_SECURITY_POLICY.contains("style-src 'self' 'unsafe-inline'"));
    assert!(SPEC_CONTENT_SECURITY_POLICY.contains("script-src 'self' 'unsafe-eval'"));
    assert!(!CONTENT_SECURITY_POLICY.contains("unsafe-inline"));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_list_shows_each_product_newest_first_with_titles_and_leaves_out_the_other_files() {
    let repo = Repo::new();
    repo.write("old.md", "# The old one\n");
    std::thread::sleep(std::time::Duration::from_millis(30));
    repo.write("new.md", "# The new one\n");
    repo.write("untitled.md", "no heading here\n");
    repo.write("old.revisions.md", "# Revisions\n");
    repo.write("old.url", "https://example.com\n");
    repo.write("old.txt", "architect\n");
    std::thread::sleep(std::time::Duration::from_millis(30));
    repo.write("newest.md", "# The newest\n");
    let directory = Repo::new();
    let server = web_app(repo.specs(), store(&directory.root).await).await;

    let body = get(&server, "/specs").await.text().await.unwrap();

    let at = |text: &str| {
        body.find(text)
            .unwrap_or_else(|| panic!("{text} in {body}"))
    };
    assert!(at("The newest") < at("The new one") && at("The new one") < at("The old one"));
    assert!(body.contains("harold"), "{body}");
    assert!(body.contains("href=\"/specs/harold/old\""), "{body}");
    assert!(body.contains(">untitled<"), "{body}");
    for hidden in ["Revisions", "revisions", "old.url", "architect", ".txt"] {
        assert!(!body.contains(hidden), "{hidden} in {body}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_machine_with_no_products_lists_no_specs() {
    let directory = Repo::new();
    let specs = Arc::new(Specs::new(None));
    let server = web_app(specs, store(&directory.root).await).await;

    let body = get(&server, "/specs").await.text().await.unwrap();

    assert!(body.contains("No specs on this machine"), "{body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_or_unknown_spec_answers_404_without_a_path() {
    let repo = Repo::new();
    repo.write("fine.md", "# Fine\n");
    repo.write(".revisions.md", "# Hidden\n");
    repo.write("big.md", &"a".repeat(1024 * 1024 + 1));
    let outside = repo.root.join("outside.md");
    std::fs::write(&outside, "# Outside\n").unwrap();
    std::os::unix::fs::symlink(&outside, repo.folder().join("link.md")).unwrap();
    let inside = repo.folder().join("fine.md");
    std::os::unix::fs::symlink(&inside, repo.folder().join("alias.md")).unwrap();
    let directory = Repo::new();
    let server = web_app(repo.specs(), store(&directory.root).await).await;

    for path in [
        "/specs/harold/.revisions",
        "/specs/harold/a.b",
        "/specs/harold/a%2Fb",
        "/specs/harold/..%2Foutside",
        "/specs/nobody/fine",
        "/specs/harold/missing",
        "/specs/harold/link",
        "/specs/harold/big",
    ] {
        let response = get(&server, path).await;
        assert_eq!(response.status(), 404, "{path}");
        let body = response.text().await.unwrap();
        assert!(
            !body.contains(repo.root.to_str().unwrap()),
            "{path}: {body}"
        );
        assert!(
            !body.contains("Outside") && !body.contains("Hidden"),
            "{path}"
        );
    }
    // A symlink that stays inside the folder is fine.
    assert_eq!(get(&server, "/specs/harold/alias").await.status(), 200);
    assert_eq!(get(&server, "/specs/harold/fine").await.status(), 200);
    // And the list leaves out the file that would be refused.
    let list = get(&server, "/specs").await.text().await.unwrap();
    for hidden in ["/specs/harold/link", "/specs/harold/big"] {
        assert!(!list.contains(hidden), "{hidden} in {list}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_spec_shown_line_names_the_product_and_id_only_when_both_are_valid() {
    crate::telemetry::test_support::install();
    let logs = CapturedLogs::start();
    let repo = Repo::new();
    repo.write("fine.md", "# Fine\n");
    let specs = repo.specs();

    let page = super::spec_page(
        &specs,
        &WebAppIdentity::default(),
        super::Listener::WebApp,
        "harold",
        "fine",
    )
    .await;
    assert_eq!(page.status(), 200);
    let text = logs.text();
    assert!(text.contains("app spec shown"), "{text}");
    assert!(
        text.contains("listener=\"web_app\"") && text.contains("result=\"ok\""),
        "{text}"
    );
    assert!(
        text.contains("product=\"harold\"") && text.contains("id=\"fine\""),
        "{text}"
    );

    let logs = CapturedLogs::start();
    for (product, id) in [("SECRET-PRODUCT", "fine"), ("harold", "SECRET-ID")] {
        let page = super::spec_page(
            &specs,
            &WebAppIdentity::default(),
            super::Listener::Specs,
            product,
            id,
        )
        .await;
        assert_eq!(page.status(), 404);
    }
    let text = logs.text();
    assert_eq!(text.matches("app spec shown").count(), 2, "{text}");
    assert!(
        text.contains("result=\"not_found\"") && text.contains("listener=\"specs\""),
        "{text}"
    );
    assert!(!text.contains("SECRET"), "{text}");
    assert!(
        !text.contains("product=") && !text.contains(" id="),
        "{text}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_specs_listener_serves_without_a_login_and_shows_no_share_button() {
    let repo = Repo::new();
    repo.write("fine.md", SPEC);
    let server = specs_listener(repo.specs()).await;
    let client = reqwest::Client::new();

    let list = client.get(server.url("/specs")).send().await.unwrap();
    assert_eq!(list.status(), 200);
    assert!(
        list.text()
            .await
            .unwrap()
            .contains("href=\"/specs/harold/fine\"")
    );
    let page = client
        .get(server.url("/specs/harold/fine"))
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
    assert_eq!(
        page.headers()["content-security-policy"],
        SPEC_CONTENT_SECURITY_POLICY
    );
    let body = page.text().await.unwrap();
    assert!(body.contains("<pre class=\"mermaid\">"), "{body}");
    assert!(
        !body.contains("share-spec") && !body.contains("Share"),
        "{body}"
    );
    assert!(!body.contains("datastar"), "{body}");
    for file in ["app.css", "specs.js"] {
        let response = client
            .get(server.url(&format!("/static/{file}")))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{file}");
    }
    let mermaid = client
        .get(server.url(&format!(
            "/static/{}",
            crate::web_app::pages::MERMAID_JS_NAME
        )))
        .send()
        .await
        .unwrap();
    assert_eq!(mermaid.status(), 200);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_specs_listener_refuses_a_wrong_host() {
    let repo = Repo::new();
    repo.write("fine.md", "# Fine\n");
    let server = specs_listener(repo.specs()).await;
    let client = reqwest::Client::new();
    let port = server.addr.port();

    for host in [
        "evil.example",
        &format!("evil.example:{port}"),
        "127.0.0.1",
        "localhost",
    ] {
        let response = client
            .get(server.url("/specs"))
            .header("Host", host)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 403, "{host}");
        assert!(response.bytes().await.unwrap().is_empty(), "{host}");
    }
    for host in [
        format!("127.0.0.1:{port}"),
        format!("localhost:{port}"),
        format!("LOCALHOST:{port}"),
    ] {
        let response = client
            .get(server.url("/specs"))
            .header("Host", &host)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{host}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_specs_listener_gives_404_to_every_route_and_method_not_listed() {
    let repo = Repo::new();
    repo.write("fine.md", "# Fine\n");
    let server = specs_listener(repo.specs()).await;
    let client = reqwest::Client::new();

    for path in [
        "/",
        "/notes",
        "/updates",
        "/replies",
        "/specs/",
        "/specs/harold",
        "/specs/harold/fine/share",
        "/static/datastar-1.0.2.js",
        "/static/sw.js",
        "/static/manifest.webmanifest",
        "/static/icon.png",
        "/static/pages.js",
    ] {
        let response = client.get(server.url(path)).send().await.unwrap();
        assert_eq!(response.status(), 404, "GET {path}");
    }
    for (method, path) in [
        ("POST", "/specs/harold/fine/share"),
        ("POST", "/specs"),
        ("POST", "/specs/harold/fine"),
        ("PUT", "/specs/harold/fine"),
        ("DELETE", "/specs/harold/fine"),
        ("POST", "/replies"),
    ] {
        let response = client
            .request(method.parse().unwrap(), server.url(path))
            .header("Origin", format!("http://{}", server.addr))
            .header("Sec-Fetch-Site", "same-origin")
            .header("Datastar-Request", "true")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 404, "{method} {path}");
    }
}

async fn share(
    server: &Served,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> reqwest::Response {
    let mut request = reqwest::Client::new()
        .post(server.url(path))
        .header("Tailscale-User-Login", LOGIN)
        .body(body.to_owned());
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    request.send().await.unwrap()
}

const SAME_ORIGIN: [(&str, &str); 2] = [
    ("Sec-Fetch-Site", "same-origin"),
    ("Datastar-Request", "true"),
];

#[tokio::test(flavor = "current_thread")]
async fn share_puts_one_reply_with_the_product_id_and_the_files_sha_in_the_thread() {
    crate::telemetry::test_support::install();
    let logs = CapturedLogs::start();
    let repo = Repo::new();
    repo.write("fine.md", SPEC);
    let directory = Repo::new();
    let store = store(&directory.root).await;
    let server = web_app(repo.specs(), Arc::clone(&store)).await;

    // The form's own sha is ignored: the reply holds the file's.
    let response = share(
        &server,
        "/specs/harold/fine/share",
        &SAME_ORIGIN,
        r#"{"sha":"000000000000","product":"other","reply":"hijack"}"#,
    )
    .await;

    assert_eq!(response.status(), 200);
    let body = response.text().await.unwrap();
    assert!(body.contains("Asked the chief to share it"), "{body}");
    assert_eq!(
        replies(&store).await,
        vec![format!("Share spec harold/fine sha {}", sha12(SPEC))]
    );
    let text = logs.text();
    assert!(text.contains("app spec shared"), "{text}");
    assert!(
        text.contains("product=harold") && text.contains("id=fine"),
        "{text}"
    );
    assert!(text.contains(&format!("sha={}", sha12(SPEC))), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn share_sends_nothing_without_an_origin_or_for_an_id_that_fails_the_spec_file_checks() {
    let repo = Repo::new();
    repo.write("fine.md", "# Fine\n");
    repo.write(".revisions.md", "# Hidden\n");
    let outside = repo.root.join("outside.md");
    std::fs::write(&outside, "# Outside\n").unwrap();
    std::os::unix::fs::symlink(&outside, repo.folder().join("link.md")).unwrap();
    let directory = Repo::new();
    let store = store(&directory.root).await;
    let server = web_app(repo.specs(), Arc::clone(&store)).await;

    // No Origin (and no Sec-Fetch-Site): the same-origin check refuses it.
    let response = share(
        &server,
        "/specs/harold/fine/share",
        &[("Datastar-Request", "true")],
        "{}",
    )
    .await;
    assert_eq!(response.status(), 403);
    // A cross-site post, and a same-origin post with no non-simple header.
    let response = share(
        &server,
        "/specs/harold/fine/share",
        &[
            ("Sec-Fetch-Site", "cross-site"),
            ("Datastar-Request", "true"),
        ],
        "{}",
    )
    .await;
    assert_eq!(response.status(), 403);
    let response = share(
        &server,
        "/specs/harold/fine/share",
        &[("Sec-Fetch-Site", "same-origin")],
        "{}",
    )
    .await;
    assert_eq!(response.status(), 403);
    // No login at all.
    let response = reqwest::Client::new()
        .post(server.url("/specs/harold/fine/share"))
        .header("Sec-Fetch-Site", "same-origin")
        .header("Datastar-Request", "true")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
    for path in [
        "/specs/harold/.revisions/share",
        "/specs/harold/a.b/share",
        "/specs/harold/missing/share",
        "/specs/harold/link/share",
        "/specs/nobody/fine/share",
    ] {
        let response = share(&server, path, &SAME_ORIGIN, "{}").await;
        assert_eq!(response.status(), 404, "{path}");
    }
    assert_eq!(event_count(&store).await, 0);
    assert!(replies(&store).await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_thread_page_header_has_no_specs_link() {
    for notes_on in [false, true] {
        let repo = Repo::new();
        let directory = Repo::new();
        let server = web_app_with_notes(repo.specs(), store(&directory.root).await, notes_on).await;

        let body = get(&server, "/").await.text().await.unwrap();

        assert!(!body.contains("id=\"to-specs\""), "{body}");
        assert_eq!(body.contains("id=\"to-notes\""), notes_on, "{body}");
    }
}

#[test]
fn specs_js_starts_mermaid_strictly_with_a_theme_for_each_mode() {
    let script = include_str!("../../static/web_app/specs.js");
    for wanted in [
        "securityLevel: \"strict\"",
        "(prefers-color-scheme: dark)",
        "\"dark\" : \"neutral\"",
        "addEventListener(\"change\", draw)",
        "diagram.node.textContent = diagram.source;",
    ] {
        assert!(script.contains(wanted), "specs.js lacks {wanted}");
    }
    for absent in ["eval(", "new Function", "http://", "https://"] {
        assert!(!script.contains(absent), "specs.js holds {absent}");
    }
}

fn settings() -> crate::settings::WebAppSettings {
    crate::settings::WebAppSettings {
        listen: "127.0.0.1:0".into(),
        allowed_login: LOGIN.into(),
        app_name: None,
        icon: None,
        require_pairing: false,
        pairing_file: None,
        specs_listen: Some("127.0.0.1:0".into()),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_specs_listener_opens_only_with_a_pod_registry() {
    let directory = Repo::new();
    let store = store(&directory.root).await;
    let store_path = directory.root.join("store");
    let grpc = crate::web_app::idle_grpc();
    let registry = directory.root.join("products.toml");
    std::fs::write(
        &registry,
        format!(
            "[products.harold]\nrepo = \"{}\"\nbase = \"main\"\n",
            directory.repo().display()
        ),
    )
    .unwrap();
    directory.write("fine.md", "# Fine\n");

    let (shutdown, shutdown_rx) = watch::channel(());
    let without = crate::web_app::start(
        &settings(),
        Arc::clone(&store),
        &store_path,
        shutdown_rx.clone(),
        None,
        crate::web_app::idle_agents(&grpc),
    )
    .await
    .unwrap();
    assert!(without.specs_addr.is_none());

    let mut agents = crate::web_app::idle_agents(&grpc);
    agents.pod_registry = Some(&registry);
    let with = crate::web_app::start(
        &settings(),
        Arc::clone(&store),
        &store_path,
        shutdown_rx,
        None,
        agents,
    )
    .await
    .unwrap();
    let addr = with.specs_addr.expect("the specs listener is open");
    assert_ne!(addr, with.local_addr);
    let response = reqwest::get(format!("http://{addr}/specs/harold/fine"))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert!(response.text().await.unwrap().contains("Fine"));
    drop(shutdown);
    with.handle.await.unwrap();
    without.handle.await.unwrap();
}

#[test]
fn the_share_button_posts_through_datastar_and_not_plain_fetch() {
    let template = include_str!("../../templates/web_app/spec.html");
    assert!(
        template.contains("data-on:click=\"@post('{{ share_path }}'"),
        "the Share button does not use Datastar's @post"
    );
    // Datastar retries a failed request up to 10 times; a repeated Share would ask the chief again.
    assert!(
        template.contains("retryMaxCount: 0"),
        "the Share post may retry"
    );
    for absent in ["fetch(", "XMLHttpRequest", "<form"] {
        assert!(!template.contains(absent), "spec.html holds {absent}");
    }
    let script = include_str!("../../static/web_app/specs.js");
    assert!(!script.contains("fetch("), "specs.js posts with fetch");
    assert!(
        !script.contains("/share"),
        "specs.js posts the share request"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_spec_shared_record_joins_the_phones_trace() {
    crate::telemetry::test_support::install();
    let repo = Repo::new();
    repo.write("traced.md", "# Traced\n");
    let directory = Repo::new();
    let store = store(&directory.root).await;
    let server = web_app(repo.specs(), Arc::clone(&store)).await;
    let trace = "5bf92f3577b34da6a3ce929d0e0e4737";

    let mut headers = SAME_ORIGIN.to_vec();
    let traceparent = format!("00-{trace}-00f067aa0ba902b8-01");
    headers.push(("traceparent", &traceparent));
    let response = share(&server, "/specs/harold/traced/share", &headers, "{}").await;
    assert_eq!(response.status(), 200);

    let joined = crate::telemetry::test_support::logs()
        .into_iter()
        .any(|log| {
            format!("{:?}", log.record.body()).contains("app spec shared")
                && log
                    .record
                    .trace_context()
                    .is_some_and(|context| context.trace_id.to_string() == trace)
        });
    assert!(
        joined,
        "no `app spec shared` record carries the phone's trace"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_specs_listener_that_cannot_bind_is_not_open_and_names_no_address() {
    let directory = Repo::new();
    let store = store(&directory.root).await;
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let mut taken_settings = settings();
    taken_settings.specs_listen = Some(taken.local_addr().unwrap().to_string());
    let registry = directory.root.join("products.toml");
    std::fs::write(&registry, "").unwrap();
    let grpc = crate::web_app::idle_grpc();
    let mut agents = crate::web_app::idle_agents(&grpc);
    agents.pod_registry = Some(&registry);
    let (shutdown, shutdown_rx) = watch::channel(());

    let running = crate::web_app::start(
        &taken_settings,
        store,
        &directory.root.join("store"),
        shutdown_rx,
        None,
        agents,
    )
    .await
    .unwrap();

    assert!(running.specs_addr.is_none());
    drop(shutdown);
    running.handle.await.unwrap();
}
