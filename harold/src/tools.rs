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

use tokio::sync::watch;
use tokio::task::JoinHandle;
use tonic::service::Routes;
use tracing::warn;

use crate::agent::snapshot::AgentSnapshotHub;
use crate::outbound::left_out::same_folder;
use crate::pod_stats;
use crate::settings::Settings;
use crate::timed_prompts::{self, LineOutcome, Ports, TmuxPorts, Writer};

pub(crate) mod backlog;
pub(crate) mod questions;

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
    pub(crate) shutdown: watch::Receiver<()>,
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
    shutdown: watch::Receiver<()>,
) -> Tools {
    let ports = Arc::new(TmuxPorts::new(snapshots, cfg.agents.0.clone()));
    let typing = TmuxTyping {
        ports: Arc::clone(&ports),
        writer: Writer::Tool(questions::NAME),
    };
    let chief_pane = SnapshotChiefPane {
        ports: Arc::clone(&ports),
        folder: cfg.notify.chief.clone(),
    };
    let pane_folder = SnapshotPaneFolder { ports };
    let products = RegistryProducts {
        path: cfg.telemetry.pod_registry_path(),
    };
    start_with(
        store_path,
        Arc::new(typing),
        Arc::new(chief_pane),
        Arc::new(pane_folder),
        Arc::new(products),
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
    shutdown: watch::Receiver<()>,
) -> Tools {
    let mut tools = Tools {
        routes: Routes::default(),
        tasks: Vec::new(),
    };
    let [(questions_name, questions_ctx), (backlog_name, backlog_ctx)] = contexts(
        store_path,
        typing,
        chief_pane,
        pane_folder,
        products,
        shutdown,
    );
    start_tool(
        questions_name,
        questions::start(questions_ctx).await,
        &mut tools,
    );
    start_tool(backlog_name, backlog::start(backlog_ctx).await, &mut tools);
    tools
}

/// Each tool's name and context, in the order they start. Only the question tool
/// types, so the backlog tool is given no typing.
fn contexts(
    store_path: &str,
    typing: Arc<dyn PaneTyping>,
    chief_pane: Arc<dyn ChiefPane>,
    pane_folder: Arc<dyn PaneFolder>,
    products: Arc<dyn Products>,
    shutdown: watch::Receiver<()>,
) -> [(&'static str, ToolContext); 2] {
    let context = |name: &str, typing: Option<Arc<dyn PaneTyping>>| ToolContext {
        data_folder: data_folder(store_path, name),
        typing,
        chief_pane: Arc::clone(&chief_pane),
        pane_folder: Arc::clone(&pane_folder),
        products: Arc::clone(&products),
        shutdown: shutdown.clone(),
    };
    [
        (questions::NAME, context(questions::NAME, Some(typing))),
        (backlog::NAME, context(backlog::NAME, None)),
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

/// Finds the chief's pane in the agent snapshot: the one agent pane whose folder
/// `[notify] chief` matches. With no entry set, no such pane, or more than one, there is no
/// chief.
struct SnapshotChiefPane<P = TmuxPorts> {
    ports: Arc<P>,
    folder: Option<String>,
}

impl<P: Ports> ChiefPane for SnapshotChiefPane<P> {
    fn chief_pane(&self) -> Option<String> {
        let folder = self.folder.as_deref()?;
        let mut in_folder = self
            .ports
            .agents()
            .panes
            .into_iter()
            .filter(|pane| same_folder(&pane.folder, folder));
        let chief = in_folder.next()?;
        let several = in_folder.next().is_some();
        if several {
            tracing::info!(count = 2 + in_folder.count(), "chief pane ambiguous");
            return None;
        }
        Some(chief.pane_id)
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

/// Reads the pod registry on every call, so an edit to it shows at once. With no
/// registry set there are no products.
struct RegistryProducts {
    path: Option<PathBuf>,
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
}

#[cfg(test)]
#[path = "tools_tests.rs"]
mod tests;
