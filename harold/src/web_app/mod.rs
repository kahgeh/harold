//! The home-screen web app Harold serves to the phone through `tailscale serve`: one
//! thread with the chief, on a loopback address, behind the Tailscale login check.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::StatusCode;
use axum::middleware::{from_fn, from_fn_with_state};
use axum::routing::{get, post};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use crate::agent::domain::AgentSnapshot;
use crate::agent::snapshot::AgentSnapshotHub;
use crate::outbound::AppRoute;
use crate::outbound::voice_note::SystemRunner;
use crate::settings::{GrpcSettings, WebAppIcon, WebAppSettings};
use crate::store::HaroldStore;
use call::PhoneCallApi;
use push::{Pusher, VapidKey, vapid_key_path};
use route::{WebAppRoute, now_ms};

pub(crate) const DATASTAR_JS: &[u8] = include_bytes!("../../static/web_app/datastar-1.0.2.js");
/// The released Datastar 1.0.2 bundle's sha256; the tests check the embedded copy against it.
#[cfg(test)]
pub(crate) const DATASTAR_SHA256: &str =
    "2837d87acf6ee0ba8e4e63765926c25a98d63883b02f88be194a86b81d3fd24a";

/// How many days of messages the thread keeps.
pub(crate) const THREAD_DAYS: i64 = 30;

const DAY_MS: i64 = 24 * 60 * 60 * 1000;
/// How long an uploaded photo waits for a reply to name it before the prune deletes it.
const UNUSED_PHOTO_MS: i64 = 60 * 60 * 1000;
/// How often the thread is cut back to `THREAD_DAYS`, after the cut at start.
const PRUNE_EVERY: Duration = Duration::from_secs(60 * 60);
/// How long the server gets to stop after shutdown. A client that holds its connection
/// open is not waited for.
const SERVER_STOP_LIMIT: Duration = Duration::from_secs(1);

#[cfg(test)]
#[path = "assets_tests.rs"]
mod assets_tests;

pub(crate) mod call;
pub(crate) mod chief;
pub(crate) mod echo_test;
pub(crate) mod login;
pub(crate) mod origin;
pub(crate) mod pages;
pub(crate) mod push;
pub(crate) mod replies;
pub(crate) mod route;
pub(crate) mod todos;
pub(crate) mod updates;

#[cfg(test)]
#[path = "flow_tests.rs"]
mod flow_tests;

#[cfg(test)]
#[path = "echo_test_tests.rs"]
mod echo_test_tests;

#[cfg(test)]
#[path = "dictation_tests.rs"]
mod dictation_tests;

#[cfg(test)]
#[path = "photo_tests.rs"]
mod photo_tests;

#[cfg(test)]
pub(crate) mod fake_push;

#[cfg(test)]
#[path = "todos_tests.rs"]
mod todos_tests;

/// What the app is called and looks like on the phone, read once at start.
#[derive(Debug, Clone)]
pub(crate) struct WebAppIdentity {
    pub name: String,
    pub icon: Option<WebAppIcon>,
}

impl Default for WebAppIdentity {
    fn default() -> Self {
        Self {
            name: "Chief".to_owned(),
            icon: None,
        }
    }
}

/// What the web app needs to know of the agents and the question tool, given at start.
#[derive(Clone, Copy)]
pub(crate) struct Agents<'a> {
    /// The agent panes, to find the chief and see whether it is busy.
    pub snapshots: &'a AgentSnapshotHub,
    /// `[notify] chief`, the chief's folder.
    pub chief_folder: Option<&'a str>,
    /// `[notify] chief_session`, the chief's tmux session.
    pub chief_session: Option<&'a str>,
    /// Harold's own gRPC address, where the question tool's service listens.
    pub grpc: &'a GrpcSettings,
}

/// How long a page may go unheard before it stops counting as on screen. The page beats
/// every 30 s, so one beat can go missing.
const PAGE_SEEN_LIMIT: Duration = Duration::from_secs(75);

/// The time `OpenPages` reads; tests swap in their own.
type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

#[derive(Default)]
struct PageState {
    /// The number the next stream gets.
    next: u64,
    /// The open streams, by number.
    open: BTreeSet<u64>,
    /// Streams numbered up to this one were open when the page last reported itself hidden.
    hidden_through: u64,
    /// When the page last said it is on screen; `None` after a hidden report.
    last_seen: Option<Instant>,
}

/// The open page streams, which of them the page has since reported hidden, and when the
/// page was last heard from. The push route asks it whether a page is on screen.
pub(crate) struct OpenPages {
    state: Mutex<PageState>,
    clock: Clock,
}

impl Default for OpenPages {
    fn default() -> Self {
        Self::with_clock(Arc::new(Instant::now))
    }
}

impl OpenPages {
    pub(crate) fn with_clock(clock: Clock) -> Self {
        Self {
            state: Mutex::default(),
            clock,
        }
    }

    fn state(&self) -> MutexGuard<'_, PageState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Gives a stream the next number; it leaves the open set when the guard drops, for any
    /// reason. Opening a stream does not count as hearing from the page.
    pub(crate) fn open(self: &Arc<Self>) -> OpenPage {
        let mut state = self.state();
        state.next += 1;
        let number = state.next;
        state.open.insert(number);
        OpenPage {
            pages: Arc::clone(self),
            number,
        }
    }

    /// The page said it is hidden: the streams open now stop counting, later ones don't.
    pub(crate) fn report_hidden(&self) {
        let mut state = self.state();
        state.hidden_through = state.next;
        state.last_seen = None;
    }

    /// The page said it is on screen.
    pub(crate) fn report_seen(&self) {
        let now = (self.clock)();
        self.state().last_seen = Some(now);
    }

    /// The open streams of pages that are on screen: those opened since the last hidden
    /// report, and none unless the page was heard from within `PAGE_SEEN_LIMIT`.
    pub(crate) fn visible(&self) -> usize {
        let now = (self.clock)();
        let state = self.state();
        let heard = state
            .last_seen
            .is_some_and(|seen| now.saturating_duration_since(seen) <= PAGE_SEEN_LIMIT);
        if !heard {
            return 0;
        }
        state.open.range(state.hidden_through + 1..).count()
    }
}

/// One open page stream, held until this drops.
pub(crate) struct OpenPage {
    pages: Arc<OpenPages>,
    number: u64,
}

impl Drop for OpenPage {
    fn drop(&mut self) {
        self.pages.state().open.remove(&self.number);
    }
}

/// `POST /page/hidden`: the page left the screen, so a push is wanted again.
async fn post_page_hidden(State(state): State<AppState>) -> StatusCode {
    state.pages.report_hidden();
    StatusCode::NO_CONTENT
}

/// `POST /page/seen`: the page is on screen, which holds pushes back for another 75 s.
async fn post_page_seen(State(state): State<AppState>) -> StatusCode {
    state.pages.report_seen();
    StatusCode::NO_CONTENT
}

/// What every route can reach.
#[derive(Clone)]
pub(crate) struct AppState {
    pub store: Arc<HaroldStore>,
    /// The one Tailscale login the app answers.
    pub allowed_login: Arc<str>,
    /// Harold's VAPID public key, base64url.
    pub public_key: Arc<str>,
    /// The loopback Hosts the app answers to, besides the tailnet's `.ts.net` names:
    /// `origin::loopback_hosts`.
    pub loopback_hosts: Arc<[String]>,
    /// Changes, or closes, when Harold shuts down.
    pub shutdown: watch::Receiver<()>,
    /// The phone's calls; `None` when Harold has none, and every call route answers `409`.
    pub calls: Option<Arc<dyn PhoneCallApi>>,
    /// The app's name and icon.
    pub identity: Arc<WebAppIdentity>,
    /// The agent panes as the latest snapshot sees them: where the chief is and whether it
    /// is busy.
    pub snapshots: watch::Receiver<AgentSnapshot>,
    /// `[notify] chief`, the folder the chief runs in.
    pub chief_folder: Option<Arc<str>>,
    /// `[notify] chief_session`, the chief's tmux session.
    pub chief_session: Option<Arc<str>>,
    /// The question tool's service, reached through Harold's own gRPC address.
    pub questions: todos::Questions,
    /// Bumped by each todo post, so every open page reads the todos again at once.
    pub todos_changed: Arc<watch::Sender<u64>>,
    /// The echo test's runs, sample and recent silent runs.
    pub echo_test: Arc<echo_test::EchoTest>,
    /// The open page streams, which the route reads before it pushes.
    pub pages: Arc<OpenPages>,
}

#[cfg(test)]
impl AppState {
    /// This state with `echo_test` in place of the one that makes no sample.
    pub(crate) fn with_echo_test(mut self, echo_test: echo_test::EchoTest) -> Self {
        self.echo_test = Arc::new(echo_test);
        self
    }

    /// A state for tests, with an idle agent snapshot, no chief folder and no question
    /// service reachable.
    pub(crate) fn for_tests(
        store: Arc<HaroldStore>,
        allowed_login: &str,
        public_key: &str,
        loopback_hosts: Arc<[String]>,
        shutdown: watch::Receiver<()>,
        calls: Option<Arc<dyn PhoneCallApi>>,
        identity: WebAppIdentity,
    ) -> Self {
        Self {
            store,
            allowed_login: Arc::from(allowed_login),
            public_key: Arc::from(public_key),
            loopback_hosts,
            shutdown,
            calls,
            identity: Arc::new(identity),
            snapshots: idle_snapshots(),
            chief_folder: None,
            chief_session: None,
            questions: todos::Questions::lazy(&idle_grpc()),
            todos_changed: Arc::new(watch::channel(0).0),
            echo_test: Arc::new(echo_test::EchoTest::new(
                std::env::temp_dir().join("harold-echo-test-unused"),
                Arc::new(echo_test::NeverRuns),
            )),
            pages: Arc::default(),
        }
    }
}

/// An agent snapshot hub with no panes, kept alive for the whole test run.
#[cfg(test)]
pub(crate) fn idle_hub() -> &'static AgentSnapshotHub {
    use crate::store::EventVersion;
    Box::leak(Box::new(AgentSnapshotHub::new(AgentSnapshot {
        through_event_version: EventVersion::start(),
        server_time_ms: 0,
        monitor_health: Vec::new(),
        panes: Vec::new(),
        tts_muted: false,
        messaging_paused: false,
    })))
}

#[cfg(test)]
pub(crate) fn idle_snapshots() -> watch::Receiver<AgentSnapshot> {
    idle_hub().subscribe()
}

/// Idle agents: no panes, no chief folder, `grpc` for the question tool.
#[cfg(test)]
pub(crate) fn idle_agents(grpc: &GrpcSettings) -> Agents<'_> {
    Agents {
        snapshots: idle_hub(),
        chief_folder: None,
        chief_session: None,
        grpc,
    }
}

/// A gRPC address nothing listens on.
#[cfg(test)]
pub(crate) fn idle_grpc() -> GrpcSettings {
    GrpcSettings {
        host: "127.0.0.1".to_owned(),
        port: 1,
    }
}

/// The app's routes, every one of them, the 404 fallback included, behind
/// `login::require_login` and then `origin::require_same_origin`, and every response
/// carrying the content security policy.
pub(crate) fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(pages::get_thread))
        .route("/static/{name}", get(pages::get_static))
        .route("/replies", post(replies::post_reply))
        .route(
            "/replies/photo",
            post(replies::post_photo).layer(DefaultBodyLimit::max(replies::PHOTO_BODY_LIMIT)),
        )
        .route("/replies/photo/{id}", get(replies::get_photo))
        .route("/dictation/check", post(replies::post_dictation_check))
        .route("/push/subscription", post(replies::post_subscription))
        .route("/updates", get(updates::get_updates))
        .route("/page/hidden", post(post_page_hidden))
        .route("/page/seen", post(post_page_seen))
        .route("/thread/older", get(pages::get_thread_older))
        .route("/todos/answer", post(todos::post_answer))
        .route("/todos/picks", post(todos::post_picks))
        .route("/todos/later", post(todos::post_later))
        .route("/todos/tick", post(todos::post_tick))
        .route("/todos/done", post(todos::post_done))
        .route("/todos/ignore", post(todos::post_ignore))
        .route("/todos/revise", post(todos::post_revise))
        .route("/todos/reject", post(todos::post_reject))
        .route("/echo-test", get(echo_test::get_page))
        .route("/echo-test/sample", get(echo_test::get_sample))
        .route("/echo-test/runs", post(echo_test::post_run))
        .route("/call", post(call::post_call).delete(call::delete_call))
        .route("/call/audio", post(call::post_audio))
        .route("/call/check", post(call::post_check))
        .route("/call/played", post(call::post_played))
        .route("/call/talk-now", post(call::post_talk_now))
        .route("/call/answer/{id}", get(call::get_answer))
        .fallback(pages::not_found)
        // The last layer added runs first: the login check, then the origin check.
        .layer(from_fn_with_state(
            state.clone(),
            origin::require_same_origin,
        ))
        .layer(from_fn_with_state(state.clone(), login::require_login))
        .layer(from_fn(pages::add_policy))
        .with_state(state)
}

/// The running app.
pub(crate) struct Running {
    /// The delivery loop's way into the thread and the pushes.
    pub route: Arc<dyn AppRoute>,
    /// Ends when the server and the pruning have stopped, after shutdown.
    pub handle: JoinHandle<()>,
    /// The address the app listens on, which tests read for the port.
    #[cfg(test)]
    pub local_addr: std::net::SocketAddr,
}

/// Binds `settings.listen_addr()`, makes or reads the key, serves until shutdown, and prunes
/// the thread to 30 days at start and every hour. Err when it cannot, and Harold then runs
/// without the app, every message reaching the phone through iMessage; the error names the
/// address, the key's path or the icon's path, never the login, the key or the icon. At shutdown an open phone call
/// ends, with reason `shutdown`, before the handle does.
pub(crate) async fn start(
    settings: &WebAppSettings,
    store: Arc<HaroldStore>,
    store_path: &Path,
    shutdown: watch::Receiver<()>,
    calls: Option<Arc<dyn PhoneCallApi>>,
    agents: Agents<'_>,
) -> Result<Running, String> {
    let addr = settings
        .listen_addr()
        .ok_or_else(|| "web_app.listen is not an address and port".to_owned())?;
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|error| format!("listen on {addr}: {error}"))?;
    let local_addr = listener
        .local_addr()
        .map_err(|error| format!("listen on {addr}: {error}"))?;
    let key = VapidKey::load_or_create(&vapid_key_path(store_path))?;
    // The error names the key and the path only, never the file's bytes.
    let identity = WebAppIdentity {
        name: settings.name().to_owned(),
        icon: settings.load_icon()?,
    };

    prune(&store).await;
    let state = AppState {
        store: Arc::clone(&store),
        allowed_login: Arc::from(settings.allowed_login.as_str()),
        public_key: Arc::from(key.public_key_base64url()),
        loopback_hosts: Arc::from(origin::loopback_hosts(&settings.listen, local_addr)),
        shutdown: shutdown.clone(),
        calls: calls.clone(),
        identity: Arc::new(identity),
        snapshots: agents.snapshots.subscribe(),
        chief_folder: agents.chief_folder.map(Arc::from),
        chief_session: agents.chief_session.map(Arc::from),
        questions: todos::Questions::lazy(agents.grpc),
        todos_changed: Arc::new(watch::channel(0).0),
        echo_test: Arc::new(echo_test::EchoTest::new(
            echo_test::echo_test_path(store_path),
            Arc::new(SystemRunner),
        )),
        pages: Arc::default(),
    };
    let pusher = Arc::new(Pusher::new(key, &settings.allowed_login));
    let route: Arc<dyn AppRoute> =
        Arc::new(WebAppRoute::new(Arc::clone(&store), pusher).with_pages(Arc::clone(&state.pages)));

    let pruning = tokio::spawn(prune_every_hour(store, shutdown.clone()));
    let mut server_shutdown = shutdown;
    let server = axum::serve(listener, router(state)).with_graceful_shutdown(async move {
        let _ = server_shutdown.changed().await;
    });
    let mut server = tokio::spawn(async move { server.await });
    let handle = tokio::spawn(async move {
        // The pruning stops at shutdown, when the server stops taking connections too.
        let _ = pruning.await;
        if let Some(calls) = &calls {
            calls.shutdown().await;
        }
        match tokio::time::timeout(SERVER_STOP_LIMIT, &mut server).await {
            Ok(Ok(Err(error))) => error!(error = %error, "web app stopped"),
            Ok(_) => {}
            Err(_) => server.abort(),
        }
    });
    Ok(Running {
        route,
        handle,
        #[cfg(test)]
        local_addr,
    })
}

/// None without binding anything when the section is absent; `start`, with an Err logged as
/// `warn!(error, "web app unavailable")` and turned into None, when it is set. main.rs calls
/// only this.
pub(crate) async fn start_if_set(
    settings: Option<&WebAppSettings>,
    store: Arc<HaroldStore>,
    store_path: &Path,
    shutdown: watch::Receiver<()>,
    calls: Option<Arc<dyn PhoneCallApi>>,
    agents: Agents<'_>,
) -> Option<Running> {
    let settings = settings?;
    start(settings, store, store_path, shutdown, calls, agents)
        .await
        .inspect_err(|error| warn!(error = %error, "web app unavailable"))
        .ok()
}

/// When the thread starts: `THREAD_DAYS` days ago, in milliseconds since the Unix epoch.
pub(crate) fn thread_start_ms() -> i64 {
    now_ms().saturating_sub(THREAD_DAYS * DAY_MS)
}

/// Deletes the thread's rows older than `THREAD_DAYS`, with the photos on them, and the
/// photos no reply named within an hour. A failure logs a line without the store's words,
/// and the next hour tries again.
async fn prune(store: &HaroldStore) {
    let photos = store.app_photos_dir();
    let cutoff = thread_start_ms();
    // The rows stay when their photos could not be listed, so the next hour finds them.
    match store.expired_app_photo_files(cutoff).await {
        Ok(files) => {
            log_pruned_photos("expired", delete_photos(store, &photos, files, false).await);
            if store.delete_app_messages_before(cutoff).await.is_err() {
                warn!("web app thread not pruned");
            }
        }
        Err(_) => warn!("web app photos not pruned"),
    }
    match store
        .unused_app_photo_files(now_ms().saturating_sub(UNUSED_PHOTO_MS))
        .await
    {
        Ok(files) => log_pruned_photos("unused", delete_photos(store, &photos, files, true).await),
        Err(_) => warn!("web app photos not pruned"),
    }
}

/// Deletes the photo `files` in `directory` and forgets their uploads. How many were
/// deleted; a file that cannot be deleted stays recorded, for the next hour. With
/// `unused_only`, an upload a reply has taken since the listing is left alone.
async fn delete_photos(
    store: &HaroldStore,
    directory: &Path,
    files: Vec<String>,
    unused_only: bool,
) -> usize {
    let mut deleted = 0;
    for file in files {
        // Only a bare file name: the name is read from the store, never made a path.
        if file.contains('/') || file.contains("..") {
            continue;
        }
        if unused_only {
            match store.delete_unused_app_photo_upload(&file).await {
                Ok(true) => {}
                _ => continue,
            }
        }
        match tokio::fs::remove_file(directory.join(&file)).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => continue,
        }
        if unused_only || store.delete_app_photo_upload(&file).await.is_ok() {
            deleted += 1;
        }
    }
    deleted
}

fn log_pruned_photos(kind: &str, count: usize) {
    if count > 0 {
        info!(kind, count, "app photos pruned");
    }
}

/// Prunes the thread every `PRUNE_EVERY` until shutdown.
async fn prune_every_hour(store: Arc<HaroldStore>, mut shutdown: watch::Receiver<()>) {
    let mut ticks =
        tokio::time::interval_at(tokio::time::Instant::now() + PRUNE_EVERY, PRUNE_EVERY);
    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            _ = ticks.tick() => prune(&store).await,
        }
    }
}

#[cfg(test)]
#[path = "server_tests.rs"]
mod server_tests;
