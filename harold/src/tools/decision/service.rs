//! The decision tool's gRPC service. It trusts local callers (ADR 0002) and checks every
//! field itself. Only the chief's pane writes.

use std::sync::Arc;

use harold_api::tools::decision::v1 as pb;
use harold_api::tools::decision::v1::decision_service_server::DecisionService;
use tonic::{Request, Response, Status};

use super::details::DetailsPort;
use super::obs;
use super::store::{
    DecisionRow, DecisionStore, Filter, NewDecision, Refusal, Status as RecordStatus, StoreError,
};
use super::validate;
use crate::settings::DecisionSettings;
use crate::telemetry::DECISION_ROLES;
use crate::tools::ChiefPane;

/// Milliseconds since the epoch, now.
pub(super) type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

const DAY_MS: i64 = 86_400_000;

pub(super) struct DecisionToolService {
    store: Arc<DecisionStore>,
    chief_pane: Arc<dyn ChiefPane>,
    details: Arc<dyn DetailsPort>,
    now_ms: Clock,
    settings: DecisionSettings,
}

impl DecisionToolService {
    pub(super) fn new(
        store: Arc<DecisionStore>,
        chief_pane: Arc<dyn ChiefPane>,
        details: Arc<dyn DetailsPort>,
        now_ms: Clock,
        settings: DecisionSettings,
    ) -> Self {
        Self {
            store,
            chief_pane,
            details,
            now_ms,
            settings,
        }
    }

    /// Only the chief's pane writes. A refusal is logged.
    fn may_write(&self, pane: &str) -> Result<(), Status> {
        if pane.is_empty() {
            return Err(refuse(Status::permission_denied, "no_pane"));
        }
        if self.chief_pane.chief_pane().as_deref() != Some(pane) {
            return Err(refuse(Status::permission_denied, "not_chief"));
        }
        Ok(())
    }

    /// Sets the gauge from the store.
    async fn refresh_gauge(&self) {
        if let Ok((open, due)) = self.store.counts().await {
            crate::telemetry::set_decisions_open(&self.settings.organisation, open, due);
        }
    }

    /// Checks every field of an add request and gives the record to store and its id.
    fn checked_new(&self, request: &pb::AddRequest) -> Result<(String, NewDecision), Status> {
        let bad = |reason: &'static str| refuse(Status::invalid_argument, reason);
        if !DECISION_ROLES.contains(&request.role.as_str()) {
            return Err(bad("bad_role"));
        }
        if !self.settings.kinds.iter().any(|kind| *kind == request.kind) {
            return Err(bad("unknown_kind"));
        }
        if !validate::details_owner(&request.details_owner) {
            return Err(bad("bad_details_owner"));
        }
        if !DECISION_ROLES.contains(&request.accountable.as_str()) {
            return Err(bad("bad_accountable"));
        }
        if !validate::title(&request.title) {
            return Err(bad("bad_title"));
        }
        if !validate::link(&request.link) {
            return Err(bad("bad_link"));
        }
        if validate::date(&request.check_on).is_none() {
            return Err(bad("bad_date"));
        }
        let asked_at_ms = match request.asked_at.as_str() {
            "" => 0,
            text => validate::time_ms(text).ok_or_else(|| bad("bad_date"))?,
        };
        let id = self.anchor_id(&request.details)?;
        Ok((
            id,
            NewDecision {
                organisation: self.settings.organisation.clone(),
                role: request.role.clone(),
                kind: request.kind.clone(),
                details_owner: request.details_owner.clone(),
                accountable: request.accountable.clone(),
                title: request.title.clone(),
                link: request.link.clone(),
                check_on: request.check_on.clone(),
                details: request.details.clone(),
                asked_at_ms,
            },
        ))
    }

    /// The id in the anchor of `details`. It is well-formed, and its date is not after
    /// the add date (UTC). An earlier date is allowed: the owner may write the entry a day
    /// before the chief adds the record.
    fn anchor_id(&self, details: &str) -> Result<String, Status> {
        let bad = || refuse(Status::invalid_argument, "details_missing");
        let id = validate::anchor_id(details).ok_or_else(bad)?;
        let id_day = validate::id_date(id).ok_or_else(bad)?;
        let today = time::OffsetDateTime::from_unix_timestamp((self.now_ms)().div_euclid(1000))
            .map_err(|_| bad())?
            .date();
        if id_day > today {
            return Err(bad());
        }
        Ok(id.to_owned())
    }
}

/// Logs a refusal and gives the status with the reason as its message.
fn refuse(make: fn(String) -> Status, reason: &'static str) -> Status {
    obs::refused(reason);
    make(reason.to_owned())
}

fn unavailable(_: StoreError) -> Status {
    Status::unavailable("store_failed")
}

fn unknown() -> Status {
    refuse(Status::not_found, "unknown_decision")
}

fn record_refusal(refusal: Refusal) -> Status {
    match refusal {
        Refusal::Unknown => unknown(),
        Refusal::AlreadyEvaluated => refuse(Status::failed_precondition, "already_evaluated"),
    }
}

fn to_pb(row: &DecisionRow) -> pb::Decision {
    pb::Decision {
        id: row.id.clone(),
        decided_on: row.decided_on.clone(),
        organisation: row.organisation.clone(),
        role: row.role.clone(),
        kind: row.kind.clone(),
        details_owner: row.details_owner.clone(),
        accountable: row.accountable.clone(),
        title: row.title.clone(),
        link: row.link.clone(),
        check_on: row.check_on.clone(),
        status: row.status.as_str().to_owned(),
        process_grade: row.process_grade.clone(),
        outcome_grade: row.outcome_grade.clone(),
        details: row.details.clone(),
        asked_at_ms: row.asked_at_ms,
        added_at_ms: row.added_at_ms,
        evaluated_at_ms: row.evaluated_at_ms,
    }
}

fn filter_of(request: &pb::ListRequest) -> Filter {
    let statuses = if request.all {
        Vec::new()
    } else if !request.due && !request.open {
        vec!["open".to_owned(), "due".to_owned()]
    } else {
        [(request.open, "open"), (request.due, "due")]
            .into_iter()
            .filter(|(wanted, _)| *wanted)
            .map(|(_, status)| status.to_owned())
            .collect()
    };
    let some = |text: &str| (!text.is_empty()).then(|| text.to_owned());
    Filter {
        statuses,
        role: some(&request.role),
        kind: some(&request.kind),
        details_owner: some(&request.details_owner),
        accountable: some(&request.accountable),
    }
}

#[tonic::async_trait]
impl DecisionService for DecisionToolService {
    async fn add(
        &self,
        request: Request<pb::AddRequest>,
    ) -> Result<Response<pb::AddResponse>, Status> {
        let request = request.into_inner();
        self.may_write(&request.pane_id)?;
        let (id, new) = self.checked_new(&request)?;
        let found = self
            .details
            .check(&request.details, &request.details_owner)
            .await
            .map_err(|error| refuse(Status::invalid_argument, error.reason))?;
        let now = (self.now_ms)();
        let row = self
            .store
            .add(&id, &new, now)
            .await
            .map_err(unavailable)?
            .ok_or_else(|| refuse(Status::invalid_argument, "details_missing"))?;
        let decide_s = (row.asked_at_ms > 0).then(|| ((now - row.asked_at_ms) / 1000).max(0));
        obs::added(&row, &found.place, decide_s);
        crate::telemetry::record_decision_added(
            &row.organisation,
            &row.role,
            &row.kind,
            &row.accountable,
        );
        self.refresh_gauge().await;
        Ok(Response::new(pb::AddResponse {
            decision: Some(to_pb(&row)),
        }))
    }

    async fn list(
        &self,
        request: Request<pb::ListRequest>,
    ) -> Result<Response<pb::ListResponse>, Status> {
        let rows = self
            .store
            .list(&filter_of(&request.into_inner()))
            .await
            .map_err(unavailable)?;
        Ok(Response::new(pb::ListResponse {
            decisions: rows.iter().map(to_pb).collect(),
        }))
    }

    async fn show(
        &self,
        request: Request<pb::ShowRequest>,
    ) -> Result<Response<pb::ShowResponse>, Status> {
        let row = self
            .store
            .get(&request.into_inner().id)
            .await
            .map_err(unavailable)?
            .ok_or_else(unknown)?;
        let details_path = self
            .details
            .path_of(&row.details)
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        Ok(Response::new(pb::ShowResponse {
            decision: Some(to_pb(&row)),
            details_path,
        }))
    }

    async fn evaluate(
        &self,
        request: Request<pb::EvaluateRequest>,
    ) -> Result<Response<pb::EvaluateResponse>, Status> {
        let request = request.into_inner();
        self.may_write(&request.pane_id)?;
        let grades = ["good", "poor"];
        if !grades.contains(&request.process_grade.as_str())
            || !grades.contains(&request.outcome_grade.as_str())
        {
            return Err(refuse(Status::invalid_argument, "bad_grade"));
        }
        let stored = self
            .store
            .get(&request.id)
            .await
            .map_err(unavailable)?
            .ok_or_else(unknown)?;
        if stored.status == RecordStatus::Evaluated {
            return Err(record_refusal(Refusal::AlreadyEvaluated));
        }
        // The details owner writes the entry before the evaluation too: read it again.
        self.details
            .check(&stored.details, &stored.details_owner)
            .await
            .map_err(|error| refuse(Status::invalid_argument, error.reason))?;
        let now = (self.now_ms)();
        let row = self
            .store
            .evaluate(
                &request.id,
                &request.process_grade,
                &request.outcome_grade,
                now,
            )
            .await
            .map_err(unavailable)?
            .map_err(record_refusal)?;
        obs::evaluated(&row, (row.evaluated_at_ms - row.added_at_ms) / DAY_MS);
        crate::telemetry::record_decision_evaluated(
            &row.organisation,
            &row.role,
            &row.kind,
            &row.accountable,
            &row.process_grade,
            &row.outcome_grade,
        );
        crate::telemetry::feature_used("decision-tool");
        self.refresh_gauge().await;
        Ok(Response::new(pb::EvaluateResponse {
            decision: Some(to_pb(&row)),
        }))
    }

    async fn recheck(
        &self,
        request: Request<pb::RecheckRequest>,
    ) -> Result<Response<pb::RecheckResponse>, Status> {
        let request = request.into_inner();
        self.may_write(&request.pane_id)?;
        if validate::date(&request.check_on).is_none() {
            return Err(refuse(Status::invalid_argument, "bad_date"));
        }
        let row = self
            .store
            .recheck(&request.id, &request.check_on)
            .await
            .map_err(unavailable)?
            .map_err(record_refusal)?;
        self.refresh_gauge().await;
        Ok(Response::new(pb::RecheckResponse {
            decision: Some(to_pb(&row)),
        }))
    }

    async fn link(
        &self,
        request: Request<pb::LinkRequest>,
    ) -> Result<Response<pb::LinkResponse>, Status> {
        let request = request.into_inner();
        self.may_write(&request.pane_id)?;
        if !validate::link(&request.link) {
            return Err(refuse(Status::invalid_argument, "bad_link"));
        }
        let row = self
            .store
            .set_link(&request.id, &request.link)
            .await
            .map_err(unavailable)?
            .ok_or_else(unknown)?;
        Ok(Response::new(pb::LinkResponse {
            decision: Some(to_pb(&row)),
        }))
    }
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;
