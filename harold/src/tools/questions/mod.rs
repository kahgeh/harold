//! The question queue, Harold's first tool. Outside this module only `NAME`
//! and `start` are used.

use std::sync::Arc;

use harold_api::tools::questions::v1::question_service_server::QuestionServiceServer;
use tokio::sync::Notify;
use tonic::service::Routes;

use crate::tools::{ToolContext, ToolError, ToolHandle};

mod batch;
mod monitor;
mod schema;
mod service;
mod spec;
mod store;
#[cfg(test)]
mod test_support;

pub(crate) const NAME: &str = "questions";

/// The reason `start` gives when `questions.db` cannot be opened or migrated. The
/// error's own text names the data folder's path, so it is not passed on.
const STORE_OPEN_FAILED: &str = "store_open_failed";

/// The reason `start` gives when the host has no way to type into a pane.
const NO_TYPING: &str = "no_typing";

/// Opens `questions.db` in the tool's data folder, then serves the queue and
/// sends its answers to the chief until shutdown.
pub(crate) async fn start(ctx: ToolContext) -> Result<ToolHandle, ToolError> {
    let typing = ctx.typing.ok_or(ToolError { reason: NO_TYPING })?;
    let store = store::TursoQuestionStore::open(&ctx.data_folder)
        .await
        .map_err(|_| ToolError {
            reason: STORE_OPEN_FAILED,
        })?;
    let store = Arc::new(store);
    let wake = Arc::new(Notify::new());
    let service =
        service::QuestionToolService::new(Arc::clone(&store), Arc::clone(&wake), ctx.products);
    let task = tokio::spawn(async move {
        tokio::join!(
            batch::run(
                Arc::clone(&store),
                typing,
                ctx.chief_pane,
                wake,
                ctx.shutdown.clone(),
            ),
            monitor::run(store, ctx.shutdown),
        );
    });
    Ok(ToolHandle {
        routes: Routes::new(QuestionServiceServer::new(service)),
        task,
    })
}

fn now_ms() -> i64 {
    (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
}
