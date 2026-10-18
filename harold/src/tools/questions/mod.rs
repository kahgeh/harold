#![allow(dead_code)]

//! The question queue, Harold's first tool. Outside this module only `NAME`
//! and `start` are used.

use std::sync::Arc;

use harold_api::tools::questions::v1::question_service_server::QuestionServiceServer;
use tokio::sync::Notify;
use tonic::service::Routes;

use crate::tools::{ToolContext, ToolError, ToolHandle};

mod batch;
mod schema;
mod service;
mod store;
#[cfg(test)]
mod test_support;

pub(crate) const NAME: &str = "questions";

/// Opens `questions.db` in the tool's data folder, then serves the queue and
/// sends its answers to the chief until shutdown.
pub(crate) async fn start(ctx: ToolContext) -> Result<ToolHandle, ToolError> {
    let store = store::TursoQuestionStore::open(&ctx.data_folder)
        .await
        .map_err(|error| ToolError {
            reason: error.to_string(),
        })?;
    let store = Arc::new(store);
    let wake = Arc::new(Notify::new());
    let service = service::QuestionToolService::new(Arc::clone(&store), Arc::clone(&wake));
    let task = tokio::spawn(batch::run(
        store,
        ctx.typing,
        ctx.chief_pane,
        wake,
        ctx.shutdown,
    ));
    Ok(ToolHandle {
        routes: Routes::new(QuestionServiceServer::new(service)),
        task,
    })
}

fn now_ms() -> i64 {
    (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
}
