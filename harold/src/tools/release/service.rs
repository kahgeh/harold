//! The release tool's gRPC service. It returns an empty answer for now.

use std::path::PathBuf;

use harold_api::tools::release::v1::release_service_server::ReleaseService;
use harold_api::tools::release::v1::{NextRequest, NextResponse};
use tonic::{Request, Response, Status};

pub(super) struct ReleaseToolService {
    /// The path of `products.toml`; `None` when the host has no registry.
    #[allow(dead_code)]
    registry: Option<PathBuf>,
}

impl ReleaseToolService {
    pub(super) fn new(registry: Option<PathBuf>) -> Self {
        Self { registry }
    }
}

#[tonic::async_trait]
impl ReleaseService for ReleaseToolService {
    async fn next(&self, _request: Request<NextRequest>) -> Result<Response<NextResponse>, Status> {
        Ok(Response::new(NextResponse::default()))
    }
}
