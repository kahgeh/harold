//! The release tool's gRPC service. It reads the registry, then asks `gather` for the
//! lists of each product.

use std::path::PathBuf;

use harold_api::tools::release::v1 as pb;
use harold_api::tools::release::v1::release_service_server::ReleaseService;
use pb::{NextRequest, NextResponse};
use time::{OffsetDateTime, UtcOffset};
use tonic::{Request, Response, Status};

use super::gather::{next_for, paused_for};
use super::model::{Line, ProductNext};
use crate::pod_stats::{Git, SystemGit, read_registry};

pub(super) struct ReleaseToolService<G = SystemGit> {
    /// The path of `products.toml`; `None` when the host has no registry.
    registry: Option<PathBuf>,
    git: G,
}

impl ReleaseToolService {
    pub(super) fn new(registry: Option<PathBuf>) -> Self {
        Self::with_git(registry, SystemGit::default())
    }
}

impl<G: Git> ReleaseToolService<G> {
    pub(super) fn with_git(registry: Option<PathBuf>, git: G) -> Self {
        Self { registry, git }
    }

    /// The lists of the named product, or of every product when `name` is empty.
    pub(super) async fn products(
        &self,
        name: &str,
        now: OffsetDateTime,
        offset: UtcOffset,
    ) -> Result<Vec<ProductNext>, Status> {
        let path = self
            .registry
            .as_ref()
            .ok_or_else(|| Status::unavailable("registry_unreadable"))?;
        let mut products =
            read_registry(path).map_err(|_| Status::unavailable("registry_unreadable"))?;
        products.sort_by(|a, b| a.name.cmp(&b.name));
        if !name.is_empty() {
            products.retain(|product| product.name == name);
            if products.is_empty() {
                return Err(Status::not_found("unknown_product"));
            }
        }
        let mut lists = Vec::with_capacity(products.len());
        for product in &products {
            lists.push(if product.paused {
                paused_for(&self.git, product).await
            } else {
                next_for(&self.git, product, now, offset).await
            });
        }
        Ok(lists)
    }
}

#[tonic::async_trait]
impl<G: Git + 'static> ReleaseService for ReleaseToolService<G> {
    async fn next(&self, request: Request<NextRequest>) -> Result<Response<NextResponse>, Status> {
        let offset = crate::telemetry::local_offset();
        let now = OffsetDateTime::now_utc();
        let lists = self
            .products(&request.get_ref().product, now, offset)
            .await?;
        Ok(Response::new(NextResponse {
            products: lists.into_iter().map(to_proto).collect(),
        }))
    }
}

fn to_proto(next: ProductNext) -> pb::ProductNext {
    pb::ProductNext {
        name: next.name,
        paused: next.paused,
        release: next.release,
        merged: lines(next.merged),
        in_progress: lines(next.in_progress),
        approved: lines(next.approved),
        parked: lines(next.parked),
        awaiting: lines(next.awaiting),
        not_clear: lines(next.not_clear),
        other_commits: next.other_commits,
        unavailable: next.unavailable.unwrap_or_default().to_string(),
    }
}

fn lines(lines: Vec<Line>) -> Vec<pb::Line> {
    lines
        .into_iter()
        .map(|line| pb::Line {
            id: line.id,
            title: line.title,
            commits: line.commits.unwrap_or(0),
            commits_known: line.commits.is_some(),
        })
        .collect()
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod service_tests;
