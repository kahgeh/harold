//! Harold's tools: the contract every tool's `start` meets.
//!
//! A tool is a module under `tools/` whose public surface is its `NAME` and
//! `start(ToolContext) -> Result<ToolHandle, ToolError>`. It opens its own
//! storage in the data folder it is given, and reaches Harold only through
//! what the context holds.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::watch;

pub(crate) mod questions;

pub(crate) type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// How typing one line into a pane went.
// The host's `PaneTyping` builds these; until the host runs a tool, only tests do.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Typing {
    /// Typed and sent with Enter.
    Typed,
    /// The text is in the input box but Enter failed; typing again would repeat it.
    EnterFailed,
    /// The pane stayed busy, or its input box held text; nothing was typed.
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

/// Types one line into a pane once it is idle with an empty input box,
/// holding the pane against Harold's other writers as the tool.
pub(crate) trait PaneTyping: Send + Sync {
    fn type_line<'a>(&'a self, pane_id: &'a str, text: &'a str) -> BoxFuture<'a, Typing>;
}

/// Finds the chief's pane, if a chief is running.
pub(crate) trait ChiefPane: Send + Sync {
    fn chief_pane(&self) -> Option<String>;
}

/// What the host gives a tool when it starts.
pub(crate) struct ToolContext {
    /// The tool's own folder, `<store folder>/tools/<name>/`.
    pub(crate) data_folder: PathBuf,
    pub(crate) typing: Arc<dyn PaneTyping>,
    pub(crate) chief_pane: Arc<dyn ChiefPane>,
    pub(crate) shutdown: watch::Receiver<()>,
}

/// What a started tool gives back: the gRPC routes the host adds to its
/// server, and the task the host joins on shutdown.
// Read by the host; until the host runs a tool, only tests read it.
#[allow(dead_code)]
pub(crate) struct ToolHandle {
    pub(crate) routes: tonic::service::Routes,
    pub(crate) task: tokio::task::JoinHandle<()>,
}

/// Why a tool could not start.
// Logged by the host; until the host runs a tool, only tests read it.
#[allow(dead_code)]
#[derive(Debug)]
pub(crate) struct ToolError {
    pub(crate) reason: String,
}
