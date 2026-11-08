//! The evidence tool's gRPC service. It reads the registry and runs the evidence pass.

use std::path::PathBuf;
use std::sync::Arc;

use harold_api::tools::evidence::v1 as pb;
use harold_api::tools::evidence::v1::evidence_service_server::EvidenceService;
use time::{OffsetDateTime, UtcOffset};
use tonic::{Request, Response, Status};

use super::check::{Env, SpecReport, check_product};
use super::chief_line::{ChiefLine, FILES, NoChief, tell_missing};
use crate::clickhouse_read::{ClickhouseSource, ReqwestSource};
use crate::pod_stats::{Git, Product, SystemGit, read_registry};
use crate::settings::FactoryFlowSettings;

pub(super) struct EvidenceToolService<G = SystemGit> {
    /// The path of `products.toml`; `None` when the host has no registry.
    registry: Option<PathBuf>,
    settings: FactoryFlowSettings,
    git: G,
    source: Arc<dyn ClickhouseSource>,
    /// Types the line for a missing spec into the chief's pane.
    chief: Arc<dyn ChiefLine>,
}

impl EvidenceToolService {
    pub(super) fn new(registry: Option<PathBuf>, settings: FactoryFlowSettings) -> Self {
        Self::with_parts(
            registry,
            settings,
            SystemGit::default(),
            Arc::new(ReqwestSource::new()),
        )
    }
}

impl<G: Git> EvidenceToolService<G> {
    pub(super) fn with_parts(
        registry: Option<PathBuf>,
        settings: FactoryFlowSettings,
        git: G,
        source: Arc<dyn ClickhouseSource>,
    ) -> Self {
        Self {
            registry,
            settings,
            git,
            source,
            chief: Arc::new(NoChief),
        }
    }

    /// Sets the port that types the evidence line into the chief's pane.
    pub(super) fn with_chief(mut self, chief: Arc<dyn ChiefLine>) -> Self {
        self.chief = chief;
        self
    }

    fn products(&self) -> Result<Vec<Product>, Status> {
        let path = self
            .registry
            .as_ref()
            .ok_or_else(|| Status::unavailable("registry_unreadable"))?;
        let mut products =
            read_registry(path).map_err(|_| Status::unavailable("registry_unreadable"))?;
        products.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(products)
    }

    /// Checks `product` at once. With an `id` it checks that spec even in a final state.
    pub(super) async fn check_named(
        &self,
        product: &str,
        id: &str,
        now: OffsetDateTime,
        offset: UtcOffset,
    ) -> Result<Vec<SpecReport>, Status> {
        let products = self.products()?;
        let product = products
            .iter()
            .find(|candidate| candidate.name == product)
            .ok_or_else(|| Status::not_found("unknown_product"))?;
        let only = (!id.is_empty()).then_some(id);
        Ok(self.run(product, only, now, offset).await)
    }

    /// The pass: each product that is not paused, each spec that is not final.
    pub(super) async fn pass(&self, now: OffsetDateTime, offset: UtcOffset) -> Vec<SpecReport> {
        let Ok(products) = self.products() else {
            return Vec::new();
        };
        let mut reports = Vec::new();
        for product in products.iter().filter(|product| !product.paused) {
            reports.extend(self.run(product, None, now, offset).await);
        }
        reports
    }

    async fn run(
        &self,
        product: &Product,
        only: Option<&str>,
        now: OffsetDateTime,
        offset: UtcOffset,
    ) -> Vec<SpecReport> {
        let env = Env {
            git: &self.git,
            source: self.source.as_ref(),
            settings: &self.settings,
        };
        let reports = {
            // A rewrite of an evidence file waits for a `tell_missing` that is running.
            let _rewrite = FILES.lock().await;
            check_product(&env, product, only, now, offset).await
        };
        tell_missing(self.chief.as_ref(), product, now, offset).await;
        reports
    }
}

fn to_proto(report: SpecReport) -> pb::SpecEvidence {
    pb::SpecEvidence {
        id: report.id,
        state: report.state.as_str().to_string(),
        results: report
            .results
            .into_iter()
            .map(|result| pb::QueryResult {
                name: result.name,
                present: result.present,
                reason: result.reason,
            })
            .collect(),
        failed: report.failed.unwrap_or_default().to_string(),
    }
}

#[tonic::async_trait]
impl<G: Git + 'static> EvidenceService for EvidenceToolService<G> {
    async fn check(
        &self,
        request: Request<pb::CheckRequest>,
    ) -> Result<Response<pb::CheckResponse>, Status> {
        let request = request.get_ref();
        let reports = self
            .check_named(
                &request.product,
                &request.id,
                OffsetDateTime::now_utc(),
                crate::telemetry::local_offset(),
            )
            .await?;
        Ok(Response::new(pb::CheckResponse {
            specs: reports.into_iter().map(to_proto).collect(),
        }))
    }
}
