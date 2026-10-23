//! The knowledge tool: Harold's index of the vault by meaning. It reads the organisation's
//! vault of Markdown notes, embeds their passages with a local model run by a worker
//! process, keeps them in a tursodb database that buckwal makes durable in a bucket, and
//! answers searches, related notes and suggested links over gRPC. Outside this module only
//! `NAME`, `Launch` and `start` are used.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use harold_api::tools::knowledge::v1::knowledge_service_server::KnowledgeServiceServer;
use tonic::service::Routes;
use tracing::warn;

use crate::settings::KnowledgeSettings;
use crate::tools::{ToolError, ToolHandle};

mod embed;
mod guard;
mod index;
mod search;
mod service;
mod store;
#[cfg(test)]
mod test_support;
mod text;

pub(crate) const NAME: &str = "knowledge";

/// The reason `start` gives when the database cannot be opened or a secret file is refused.
const STORE_FAILED: &str = "store_failed";
/// The reason `start` gives when the bucket cannot be reached while opening.
const BUCKET_UNREACHABLE: &str = "bucket_unreachable";

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
    let opened = tokio::task::spawn_blocking(move || {
        std::fs::create_dir_all(&db_folder)
            .map_err(|_| store::StoreError::StoreFailed)
            .and_then(|()| store::Store::open(&db_folder, &bucket_config))
    })
    .await;
    let store = match opened {
        Ok(Ok(store)) => Arc::new(store),
        Ok(Err(error)) => {
            warn!(reason = error.reason(), "knowledge index failed");
            return Err(ToolError {
                reason: match error {
                    store::StoreError::BucketUnreachable => BUCKET_UNREACHABLE,
                    store::StoreError::StoreFailed => STORE_FAILED,
                },
            });
        }
        Err(_) => {
            return Err(ToolError {
                reason: STORE_FAILED,
            });
        }
    };
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
    let index = index::Index::new(
        store,
        embedder.clone(),
        vault.to_path_buf(),
        &settings.exclude,
    );
    let service = service::KnowledgeToolService::new(Arc::clone(&index));
    let reaper = embedder.spawn_reaper(shutdown.clone());
    let task = tokio::spawn(async move {
        index::run(Arc::clone(&index), shutdown).await;
        let _ = reaper.await;
        // Dropping the last handle closes the database, which blocks while buckwal ends.
        let _ = tokio::task::spawn_blocking(move || drop(index)).await;
    });
    Ok(ToolHandle {
        routes: Routes::new(KnowledgeServiceServer::new(service)),
        task,
    })
}
