//! The knowledge tool: Harold's index of the vault by meaning. It reads the organisation's
//! vault of Markdown notes, embeds their passages with a local model run by a worker
//! process, keeps them in a tursodb database that buckwal makes durable in a bucket, and
//! answers searches, related notes and suggested links over gRPC. Outside this module only
//! `NAME`, `Launch` and `start` are used.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use harold_api::tools::knowledge::v1::knowledge_service_server::KnowledgeServiceServer;
use tonic::service::Routes;
use tracing::warn;

use crate::settings::KnowledgeSettings;
use crate::tools::{ToolError, ToolHandle};

mod embed;
mod guard;
mod index;
#[cfg(test)]
mod live_tests;
mod search;
mod service;
mod store;
#[cfg(test)]
mod test_support;
mod text;

pub(crate) const NAME: &str = "knowledge";

/// The reason `start` gives when a secret file is refused, and the one the open reports when
/// its thread fails.
const STORE_FAILED: &str = "store_failed";

/// What the host gives the knowledge tool: its settings and this Mac's name.
#[derive(Clone)]
pub(crate) struct Launch {
    pub settings: KnowledgeSettings,
    pub machine_name: String,
}

/// Opens the index and serves it, and keeps it current until shutdown.
pub(crate) async fn start(
    launch: Launch,
    data_folder: PathBuf,
    shutdown: tokio::sync::watch::Receiver<()>,
) -> Result<ToolHandle, ToolError> {
    let settings = &launch.settings;
    let (Some(vault), Some(command), Some(model), Some(bucket)) = (
        settings.vault.as_deref(),
        settings.command.as_deref(),
        settings.model.as_deref(),
        settings.bucket.as_ref(),
    ) else {
        return Err(ToolError {
            reason: "not_configured",
        });
    };
    let vault = Path::new(vault);
    let credentials_file = Path::new(&bucket.credentials_file);
    let key_file = Path::new(&bucket.encryption_key_file);
    for file in [credentials_file, key_file] {
        if let Err(refusal) = guard::check_secret_file(file, vault) {
            warn!(
                reason = "store_failed",
                file = %file.display(),
                refused = refusal.as_str(),
                "knowledge index failed"
            );
            return Err(ToolError {
                reason: STORE_FAILED,
            });
        }
    }
    let Some((access_key, secret_key)) = guard::read_credentials(credentials_file) else {
        warn!(
            reason = "store_failed",
            file = %credentials_file.display(),
            refused = "no_keys",
            "knowledge index failed"
        );
        return Err(ToolError {
            reason: STORE_FAILED,
        });
    };
    let bucket_config = store::BucketConfig {
        endpoint: bucket.endpoint.clone(),
        bucket: bucket.bucket.clone(),
        prefix: settings.prefix(&launch.machine_name),
        access_key,
        secret_key,
        encryption_key_file: key_file.to_path_buf(),
    };
    let db_folder = data_folder.join("index");
    let embedder = embed::WorkerEmbedder::new(
        embed::WorkerCommand {
            python: command.to_string(),
            args: settings.args.clone(),
            model_path: PathBuf::from(model),
            model_sha256: settings.model_sha256.clone(),
            folder: data_folder.join("worker"),
        },
        embed::IDLE_LIMIT,
    );
    let (vault, exclude) = (vault.to_path_buf(), settings.exclude.clone());
    let opening = Arc::new(service::Opening::default());
    let service = service::KnowledgeToolService::new(Arc::clone(&opening));
    let task = tokio::spawn(open_and_run(Opening {
        opening,
        db_folder,
        bucket_config,
        embedder,
        vault,
        exclude,
        shutdown,
    }));
    Ok(ToolHandle {
        routes: Routes::new(KnowledgeServiceServer::new(service)),
        task,
    })
}

/// How long the open waits before it tries again when the database could not open, as when
/// the bucket is down while Harold starts.
const OPEN_RETRY: Duration = Duration::from_secs(30);

/// Everything the background task needs to open the database and keep the index current.
struct Opening {
    opening: Arc<service::Opening>,
    db_folder: PathBuf,
    bucket_config: store::BucketConfig,
    embedder: Arc<embed::WorkerEmbedder>,
    vault: PathBuf,
    exclude: Vec<String>,
    shutdown: tokio::sync::watch::Receiver<()>,
}

/// Opens the database, trying again every `OPEN_RETRY` until it opens, then serves the index
/// and keeps it current until shutdown. Until it opens, every call answers `Unavailable`.
async fn open_and_run(mut task: Opening) {
    let store = loop {
        let (folder, config) = (task.db_folder.clone(), task.bucket_config.clone());
        let opened = tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&folder)
                .map_err(|_| store::StoreError::StoreFailed)
                .and_then(|()| store::Store::open(&folder, &config))
        })
        .await;
        match opened {
            Ok(Ok(store)) => break Arc::new(store),
            Ok(Err(error)) => {
                warn!(reason = error.reason(), "knowledge index failed");
                task.opening.failed(error.reason());
            }
            Err(_) => task.opening.failed(STORE_FAILED),
        }
        tokio::select! {
            () = tokio::time::sleep(OPEN_RETRY) => {}
            _ = task.shutdown.changed() => return,
        }
    };
    let index = index::Index::new(
        store,
        task.embedder.clone(),
        task.vault.clone(),
        &task.exclude,
    );
    task.opening.opened(Arc::clone(&index));
    let reaper = task.embedder.spawn_reaper(task.shutdown.clone());
    index::run(Arc::clone(&index), task.shutdown.clone()).await;
    let _ = reaper.await;
    // Dropping the last handle closes the database, which blocks while buckwal ends.
    let _ = tokio::task::spawn_blocking(move || drop(index)).await;
}

#[cfg(test)]
#[path = "start_tests.rs"]
mod start_tests;
