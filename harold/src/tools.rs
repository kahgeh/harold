//! Harold's tools: the contract every tool's `start` meets, and the host that
//! runs them.
//!
//! A tool is a module under `tools/` whose public surface is its `NAME` and
//! `start(ToolContext) -> Result<ToolHandle, ToolError>`. It opens its own
//! storage in the data folder it is given, and reaches Harold only through
//! what the context holds.
//!
//! The host, below the contract, is the only place the core touches a tool. It
//! builds each tool's context, starts it, hands its routes to the gRPC server
//! and joins its task on shutdown.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use tokio::task::JoinHandle;
use tonic::service::Routes;
use tracing::warn;

use crate::agent::domain::EffectiveAgentState;
use crate::agent::snapshot::AgentSnapshotHub;
use crate::outbound::left_out::{ChiefLookup, resolve_chief};
use crate::pod_stats::{self, PodSnapshot};
use crate::settings::{AgentProviderSettings, Settings, WatchSettings};
use crate::timed_prompts::{self, AgentPane, InputBox, LineOutcome, Ports, TmuxPorts, Writer};

pub(crate) mod backlog;
pub(crate) mod knowledge;
pub(crate) mod questions;
pub(crate) mod watch;

pub(crate) type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The reason `Products` gives when the registry is set but cannot be read. The error's
/// own text names the registry's path, so it is not passed on.
const REGISTRY_UNREADABLE: &str = "registry_unreadable";

/// How typing one line into a pane went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Typing {
    /// Typed and sent with Enter.
    Typed,
    /// The text is in the input box but Enter failed; typing again would repeat it.
    EnterFailed,
    /// The pane was busy, or its input box held text; nothing was typed.
    PaneBusy,
    /// The pane is gone; nothing was typed.
    NoPane,
}

impl Typing {
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Typing::Typed => "typed",
            Typing::EnterFailed => "enter_failed",
            Typing::PaneBusy => "pane_busy",
            Typing::NoPane => "no_pane",
        }
    }
}

/// Tries once to type one line into a pane, which must be idle with an empty
/// input box, holding the pane against Harold's other writers as the tool. It
/// never waits for a busy pane: the tool tries again on its own next round.
pub(crate) trait PaneTyping: Send + Sync {
    fn type_line<'a>(&'a self, pane_id: &'a str, text: &'a str) -> BoxFuture<'a, Typing>;
}

/// Finds the chief's pane, if a chief is running.
pub(crate) trait ChiefPane: Send + Sync {
    fn chief_pane(&self) -> Option<String>;
}

/// Finds the folder an agent pane is in, if the pane is an agent's.
pub(crate) trait PaneFolder: Send + Sync {
    fn folder_of(&self, pane_id: &str) -> Option<PathBuf>;
}

/// Lists the registry's products as `(name, repo)`. The word in `Err` says why the
/// registry could not be read.
pub(crate) trait Products: Send + Sync {
    fn products(&self) -> Result<Vec<(String, PathBuf)>, &'static str>;

    /// The address of the specs listener, such as `http://127.0.0.1:4331`, where the spec pages
    /// open on this Mac; none when the listener is not opened.
    fn local_specs_url(&self) -> Option<String> {
        None
    }
}

/// One agent pane as the agent list shows it, copied out of Harold's snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentFact {
    pub pane_id: String,
    /// `session:window.pane`, as tmux names the pane.
    pub tmux_target: String,
    pub folder: String,
    pub provider_id: String,
    /// Whether the pane's provider uses the screen adapter whose input box Harold reads.
    pub reads_as_claude: bool,
    pub state: EffectiveAgentState,
    /// Wall-clock milliseconds when the pane entered `state`.
    pub state_since_ms: i64,
    pub work_summary: Option<String>,
}

/// Harold's current agent list, read only. Every call returns a copy.
pub(crate) trait Agents: Send + Sync {
    fn agents(&self) -> Vec<AgentFact>;

    /// A fresh reading of the pane's Claude Code input box. `None` when the screen
    /// could not be read.
    fn input_box<'a>(&'a self, pane_id: &'a str) -> BoxFuture<'a, Option<InputBox>>;
}

/// Pod stats' latest snapshot, read only. Every call returns a copy.
pub(crate) trait Pods: Send + Sync {
    fn pods(&self) -> PodSnapshot;
}

/// What the host gives a tool when it starts.
pub(crate) struct ToolContext {
    /// The tool's own folder, `<store folder>/tools/<name>/`.
    pub(crate) data_folder: PathBuf,
    /// `None` for a tool that types nothing; the question tool refuses to start without it.
    pub(crate) typing: Option<Arc<dyn PaneTyping>>,
    pub(crate) chief_pane: Arc<dyn ChiefPane>,
    pub(crate) pane_folder: Arc<dyn PaneFolder>,
    pub(crate) products: Arc<dyn Products>,
    /// Read only: a tool can see the agent list and cannot change it.
    pub(crate) agents: Arc<dyn Agents>,
    /// Read only: pod stats' latest snapshot.
    pub(crate) pods: Arc<dyn Pods>,
    /// The `[watch]` settings, for the watch tool.
    pub(crate) watch_settings: WatchSettings,
    /// The environment a tool's hosted-model process runs with, and nothing else.
    pub(crate) model_environment: Vec<(std::ffi::OsString, std::ffi::OsString)>,
    pub(crate) cli_path: String,
    pub(crate) shutdown: tokio::sync::watch::Receiver<()>,
}

/// The ports and settings the host reads for tools beyond the typing, chief and
/// folder lookups.
pub(crate) struct Extras {
    /// Types as `Writer::Tool("watch")`, so the pane locks tell the writers apart.
    pub(crate) watch_typing: Arc<dyn PaneTyping>,
    pub(crate) agents: Arc<dyn Agents>,
    pub(crate) pods: Arc<dyn Pods>,
    pub(crate) watch_settings: WatchSettings,
    pub(crate) model_environment: Vec<(std::ffi::OsString, std::ffi::OsString)>,
    pub(crate) cli_path: String,
    /// The knowledge tool's launch; `None` unless `[knowledge] vault` is set.
    pub(crate) knowledge: Option<knowledge::Launch>,
}

/// What a started tool gives back: the gRPC routes the host adds to its
/// server, and the task the host joins on shutdown.
pub(crate) struct ToolHandle {
    pub(crate) routes: tonic::service::Routes,
    pub(crate) task: tokio::task::JoinHandle<()>,
}

/// Why a tool could not start: a fixed word, such as `store_open_failed`, never error
/// text, since `reason` is exported with the log line.
#[derive(Debug)]
pub(crate) struct ToolError {
    pub(crate) reason: &'static str,
}

// ---------------------------------------------------------------------------
// The host
// ---------------------------------------------------------------------------

/// The tools Harold started: the routes the gRPC server serves beside the core
/// service, and the tasks joined on shutdown.
pub(crate) struct Tools {
    pub(crate) routes: Routes,
    tasks: Vec<JoinHandle<()>>,
}

impl Tools {
    /// Waits for every started tool's task to end. Each ends when the shutdown
    /// signal it was given closes.
    pub(crate) async fn stopped(self) {
        for task in self.tasks {
            let _ = task.await;
        }
    }
}

/// Starts each tool. A tool that cannot start is logged and left out, and
/// Harold runs on without its service.
pub(crate) async fn start(
    cfg: &Settings,
    store_path: &str,
    snapshots: AgentSnapshotHub,
    shutdown: tokio::sync::watch::Receiver<()>,
) -> Tools {
    let ports =
        Arc::new(TmuxPorts::new(snapshots.clone(), cfg.agents.0.clone()).with_chief(&cfg.notify));
    let typing = TmuxTyping {
        ports: Arc::clone(&ports),
        writer: Writer::Tool(questions::NAME),
    };
    let chief_pane = SnapshotChiefPane {
        ports: Arc::clone(&ports),
        folder: cfg.notify.chief.clone(),
        session: cfg.notify.chief_session.clone(),
    };
    let agents = SnapshotAgents {
        ports: Arc::clone(&ports),
        snapshots,
        providers: cfg.agents.0.clone(),
    };
    let products = RegistryProducts {
        path: cfg.telemetry.pod_registry_path(),
        // The listener opens only with a registry, and only with the web app.
        specs_url: cfg
            .web_app
            .as_ref()
            .filter(|_| cfg.telemetry.pod_registry.is_some())
            .map(|web_app| format!("http://{}", web_app.specs_listen())),
    };
    let extras = Extras {
        watch_typing: Arc::new(TmuxTyping {
            ports: Arc::clone(&ports),
            writer: Writer::Tool(watch::NAME),
        }),
        agents: Arc::new(agents),
        pods: Arc::new(GaugePods),
        watch_settings: cfg.watch.clone(),
        model_environment: cfg.activity_summary.environment.0.clone(),
        cli_path: cfg.activity_summary.cli_path.clone(),
        knowledge: cfg.knowledge.enabled().then(|| knowledge::Launch {
            settings: cfg.knowledge.clone(),
            machine_name: cfg.machine_name.clone(),
        }),
    };
    let pane_folder = SnapshotPaneFolder { ports };
    start_with(
        store_path,
        Arc::new(typing),
        Arc::new(chief_pane),
        Arc::new(pane_folder),
        Arc::new(products),
        extras,
        shutdown,
    )
    .await
}

/// Starts the tools with the given ports.
async fn start_with(
    store_path: &str,
    typing: Arc<dyn PaneTyping>,
    chief_pane: Arc<dyn ChiefPane>,
    pane_folder: Arc<dyn PaneFolder>,
    products: Arc<dyn Products>,
    extras: Extras,
    shutdown: tokio::sync::watch::Receiver<()>,
) -> Tools {
    let mut tools = Tools {
        routes: Routes::default(),
        tasks: Vec::new(),
    };
    let knowledge_launch = extras.knowledge.clone();
    let knowledge_shutdown = shutdown.clone();
    let [
        (questions_name, questions_ctx),
        (backlog_name, backlog_ctx),
        (watch_name, watch_ctx),
    ] = contexts(
        store_path,
        typing,
        chief_pane,
        pane_folder,
        products,
        extras,
        shutdown,
    );
    start_tool(
        questions_name,
        questions::start(questions_ctx).await,
        &mut tools,
    );
    start_tool(backlog_name, backlog::start(backlog_ctx).await, &mut tools);
    start_tool(watch_name, watch::start(watch_ctx).await, &mut tools);
    if let Some(launch) = knowledge_launch {
        start_tool(
            knowledge::NAME,
            knowledge::start(
                launch,
                data_folder(store_path, knowledge::NAME),
                knowledge_shutdown,
            )
            .await,
            &mut tools,
        );
    }
    tools
}

/// Each tool's name and context, in the order they start. The question tool types as
/// `questions`, the watch tool as `watch` through `extras.watch_typing`, and the backlog tool is
/// given no typing.
fn contexts(
    store_path: &str,
    typing: Arc<dyn PaneTyping>,
    chief_pane: Arc<dyn ChiefPane>,
    pane_folder: Arc<dyn PaneFolder>,
    products: Arc<dyn Products>,
    extras: Extras,
    shutdown: tokio::sync::watch::Receiver<()>,
) -> [(&'static str, ToolContext); 3] {
    let context = |name: &str, typing: Option<Arc<dyn PaneTyping>>| ToolContext {
        data_folder: data_folder(store_path, name),
        typing,
        chief_pane: Arc::clone(&chief_pane),
        pane_folder: Arc::clone(&pane_folder),
        products: Arc::clone(&products),
        agents: Arc::clone(&extras.agents),
        pods: Arc::clone(&extras.pods),
        watch_settings: extras.watch_settings.clone(),
        model_environment: extras.model_environment.clone(),
        cli_path: extras.cli_path.clone(),
        shutdown: shutdown.clone(),
    };
    [
        (questions::NAME, context(questions::NAME, Some(typing))),
        (backlog::NAME, context(backlog::NAME, None)),
        (
            watch::NAME,
            context(watch::NAME, Some(Arc::clone(&extras.watch_typing))),
        ),
    ]
}

/// Adds a started tool's routes and task to `tools`, or logs why it could not
/// start and adds nothing.
fn start_tool(name: &'static str, started: Result<ToolHandle, ToolError>, tools: &mut Tools) {
    let handle = match started {
        Ok(handle) => handle,
        Err(error) => {
            warn!(tool = name, reason = error.reason, "tool failed to start");
            return;
        }
    };
    // Every `Routes` carries tonic's `unimplemented` fallback, and axum refuses
    // to merge two fallbacks. So a call the new tool does not serve falls back
    // to the tools started before it, and in the end to `unimplemented`.
    let earlier = std::mem::take(&mut tools.routes).into_axum_router();
    tools.routes = Routes::from(handle.routes.into_axum_router().fallback_service(earlier));
    tools.tasks.push(handle.task);
}

/// A tool's own folder: `<parent of the store path>/tools/<tool>`.
fn data_folder(store_path: &str, tool: &str) -> PathBuf {
    Path::new(store_path)
        .parent()
        .unwrap_or(Path::new("."))
        .join("tools")
        .join(tool)
}

/// Types a tool's line through `timed_prompts::type_line_once` as `writer`, holding
/// the pane against Harold's other writers with the same pane locks they use.
struct TmuxTyping<P = TmuxPorts> {
    ports: Arc<P>,
    writer: Writer,
}

impl<P: Ports> PaneTyping for TmuxTyping<P> {
    fn type_line<'a>(&'a self, pane_id: &'a str, text: &'a str) -> BoxFuture<'a, Typing> {
        let ports = Arc::clone(&self.ports);
        let writer = self.writer;
        let (pane_id, text) = (pane_id.to_string(), text.to_string());
        // A tool may drop this future on shutdown. The typing runs in its own task,
        // so a dropped wait never cuts between the text and Enter.
        let typing = tokio::spawn(async move {
            timed_prompts::type_line_once(&*ports, writer, &pane_id, &text).await
        });
        Box::pin(async move {
            match typing.await {
                Ok(LineOutcome::Typed) => Typing::Typed,
                Ok(LineOutcome::Failed) => Typing::EnterFailed,
                // A panic in the typing task (`Err(_)`) is reported as `PaneBusy`,
                // which is safe only because `TmuxPorts`' `typing_step` in
                // timed_prompts.rs catches a panic in a typing step before it can end
                // the task (`spawn_blocking` turns it into `Err`). An Enter-step panic
                // then becomes `EnterFailed`, which is marked typed and never released.
                // A `type_text` panic becomes `PaneBusy`, which is released, and that is
                // safe because nothing was typed. Any `Ports` used by a tool must not
                // panic after `type_text`, or a typed batch would be released and sent
                // twice.
                Ok(LineOutcome::PaneBusy) | Err(_) => Typing::PaneBusy,
                Ok(LineOutcome::NoPane) => Typing::NoPane,
            }
        })
    }
}

/// Finds the chief's pane in the agent snapshot: the first agent pane of the chief's tmux
/// session among the panes whose folder `[notify] chief` matches. With no entry set or no
/// such pane there is no chief.
struct SnapshotChiefPane<P = TmuxPorts> {
    ports: Arc<P>,
    folder: Option<String>,
    session: Option<String>,
}

impl<P: Ports> ChiefPane for SnapshotChiefPane<P> {
    fn chief_pane(&self) -> Option<String> {
        let view = self.ports.agents();
        let found = resolve_chief(
            view.panes.iter().map(AgentPane::position),
            self.folder.as_deref(),
            self.session.as_deref(),
        );
        match found {
            ChiefLookup::Found(pane_id) => Some(pane_id),
            ChiefLookup::Ambiguous { .. } | ChiefLookup::None => None,
        }
    }
}

/// Finds an agent pane's folder in the agent snapshot.
struct SnapshotPaneFolder<P = TmuxPorts> {
    ports: Arc<P>,
}

impl<P: Ports> PaneFolder for SnapshotPaneFolder<P> {
    fn folder_of(&self, pane_id: &str) -> Option<PathBuf> {
        let pane = self
            .ports
            .agents()
            .panes
            .into_iter()
            .find(|pane| pane.pane_id == pane_id)?;
        Some(PathBuf::from(pane.folder))
    }
}

/// The agent list from Harold's snapshot, with each pane's time in state and work summary.
struct SnapshotAgents<P = TmuxPorts> {
    ports: Arc<P>,
    snapshots: AgentSnapshotHub,
    providers: Vec<AgentProviderSettings>,
}

impl<P: Ports> Agents for SnapshotAgents<P> {
    fn agents(&self) -> Vec<AgentFact> {
        let snapshot = self.snapshots.current();
        let view = timed_prompts::agents_view(&snapshot, &self.providers);
        snapshot
            .panes
            .iter()
            .zip(view.panes)
            .map(|(projection, pane)| AgentFact {
                pane_id: pane.pane_id,
                tmux_target: projection.pane.tmux_target.clone(),
                folder: pane.folder,
                provider_id: projection.pane.incarnation.provider_id.clone(),
                reads_as_claude: pane.reads_as_claude,
                state: pane.state,
                state_since_ms: projection.last_transition_at_ms,
                work_summary: projection.work_summary.clone(),
            })
            .collect()
    }

    fn input_box<'a>(&'a self, pane_id: &'a str) -> BoxFuture<'a, Option<InputBox>> {
        Box::pin(async move {
            let screen = self.ports.read_screen(pane_id).await?;
            Some(timed_prompts::read_input_box(&screen))
        })
    }
}

/// Pod stats' snapshot, the one the pod gauges read.
struct GaugePods;

impl Pods for GaugePods {
    fn pods(&self) -> PodSnapshot {
        pod_stats::gauge_snapshot()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// Reads the pod registry on every call, so an edit to it shows at once. With no
/// registry set there are no products.
struct RegistryProducts {
    path: Option<PathBuf>,
    specs_url: Option<String>,
}

impl Products for RegistryProducts {
    fn products(&self) -> Result<Vec<(String, PathBuf)>, &'static str> {
        let Some(path) = &self.path else {
            return Ok(Vec::new());
        };
        let products = pod_stats::read_registry(path).map_err(|_| REGISTRY_UNREADABLE)?;
        Ok(products
            .into_iter()
            .map(|product| (product.name, product.repo))
            .collect())
    }

    fn local_specs_url(&self) -> Option<String> {
        self.specs_url.clone()
    }
}

#[cfg(test)]
#[path = "tools_tests.rs"]
pub(crate) mod tests;
