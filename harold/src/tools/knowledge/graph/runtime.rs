//! The graph, started on the first graph call. Opening the two logs, starting TypeDB and the
//! catch-up all wait for that call, so the graph costs nothing while nobody asks for it.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::OnceCell;

use super::backend::GraphBackend;
use super::log::{OpenDataLog, RecordLog};
use super::writer::{GraphError, GraphWriter};

const SCHEMA_KEY: &[u8] = b"graph/schema";
const DATA_KEY: &[u8] = b"graph/data";

/// Makes the graph database client. The TypeDB client is the one in production.
#[tonic::async_trait]
pub(crate) trait BackendOpener: Send + Sync {
    async fn open(&self, typedb_folder: &Path) -> Result<Arc<dyn GraphBackend>, GraphError>;

    /// Waits until what the opener started has stopped, once the shutdown has come.
    async fn finished(&self) {}
}

/// Where the logs and TypeDB's data live: `<store>/tools/knowledge/graph/`.
pub(crate) struct GraphFolders {
    pub schema_log: PathBuf,
    pub data_log: PathBuf,
    pub typedb: PathBuf,
}

impl GraphFolders {
    pub(crate) fn under(data_folder: &Path) -> Self {
        let graph = data_folder.join("graph");
        Self {
            schema_log: graph.join("schema-log"),
            data_log: graph.join("data-log"),
            typedb: graph.join("typedb"),
        }
    }
}

pub(crate) struct GraphRuntime {
    folders: Option<GraphFolders>,
    opener: Option<Arc<dyn BackendOpener>>,
    writer: OnceCell<GraphWriter>,
}

impl GraphRuntime {
    /// A graph that is on: it starts on the first call to `writer`.
    pub(crate) fn new(folders: GraphFolders, opener: Arc<dyn BackendOpener>) -> Self {
        Self {
            folders: Some(folders),
            opener: Some(opener),
            writer: OnceCell::new(),
        }
    }

    /// A graph that is off: every call answers `graph_off`.
    pub(crate) fn off() -> Self {
        Self {
            folders: None,
            opener: None,
            writer: OnceCell::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_writer(writer: GraphWriter) -> Self {
        Self {
            folders: None,
            opener: None,
            writer: OnceCell::new_with(Some(writer)),
        }
    }

    pub(crate) fn enabled(&self) -> bool {
        self.opener.is_some() || self.writer.initialized()
    }

    /// Waits for the graph's server to stop. Call it after the shutdown has come.
    pub(crate) async fn finished(&self) {
        if let Some(opener) = &self.opener {
            opener.finished().await;
        }
    }

    /// The graph writer once started, without starting it.
    #[cfg(test)]
    pub(crate) fn started(&self) -> Option<GraphWriter> {
        self.writer.get().cloned()
    }

    /// The graph writer, started now if it has not started. A start that fails is tried
    /// again on the next call.
    pub(crate) async fn writer(&self) -> Result<GraphWriter, GraphError> {
        if let Some(writer) = self.writer.get() {
            return Ok(writer.clone());
        }
        let (Some(folders), Some(opener)) = (&self.folders, &self.opener) else {
            return Err(GraphError::Off);
        };
        self.writer
            .get_or_try_init(|| async {
                let schema_log: Arc<dyn RecordLog> = Arc::new(
                    OpenDataLog::open(&folders.schema_log, SCHEMA_KEY)
                        .await
                        .map_err(|_| GraphError::LogFailed)?,
                );
                let data_log: Arc<dyn RecordLog> = Arc::new(
                    OpenDataLog::open(&folders.data_log, DATA_KEY)
                        .await
                        .map_err(|_| GraphError::LogFailed)?,
                );
                let backend = opener.open(&folders.typedb).await?;
                GraphWriter::start(backend, schema_log, data_log).await
            })
            .await
            .cloned()
    }
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;
