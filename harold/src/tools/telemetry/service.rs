//! The telemetry tool's gRPC service. Every call parses the name, checks the
//! caller's pane against the prefix, takes one from the rate limit (`count`,
//! `gauge`, `event` and `remove`), and then applies the call's rule. Every call that
//! writes ends with one count on `harold.telemetry.calls`; a refusal also writes
//! the log line `telemetry call refused` with `prefix`, `kind` and `reason` only.

use std::sync::Arc;

use harold_api::tools::telemetry::v1 as pb;
use harold_api::tools::telemetry::v1::telemetry_service_server::TelemetryService;
use tonic::metadata::{MetadataMap, MetadataValue};
use tonic::{Code, Request, Response, Status};
use tracing::info;

use super::guard::{CHIEF, Guard, REGISTRY_UNREADABLE};
use super::name::{Attr, Kind, Refused, ValueList, parse_attr, parse_name};
use super::schema::{Declaration, LastValues, series_key};
use super::signals::Signals;
use super::store::{Declared, StoreError, TelemetryStore};
use crate::telemetry::UNKNOWN_PREFIX;

/// The most a `count` adds at once.
pub(crate) const MAX_COUNT: u64 = 1_000_000;

/// The gRPC metadata key that carries the refusal's reason word.
pub(crate) const REASON_KEY: &str = "reason";

const DECLARE: &str = "declare";
const COUNT: &str = "count";
const GAUGE: &str = "gauge";
const EVENT: &str = "event";
const REMOVE: &str = "remove";

const BAD_KIND: &str = "invalid kind: use counter, gauge or event";
const BAD_COUNT: &str = "invalid count: use a whole number from 1 to 1000000";
const BAD_GAUGE: &str = "invalid gauge value: use a finite number";
const BAD_PAIRS: &str = "invalid attributes: give each declared key once, with one value";

/// Why a call failed: a refusal for the caller, or the tool's own database.
enum Failure {
    Refused(Refused),
    Store,
}

impl From<Refused> for Failure {
    fn from(refused: Refused) -> Self {
        Self::Refused(refused)
    }
}

impl From<StoreError> for Failure {
    fn from(_: StoreError) -> Self {
        Self::Store
    }
}

pub(crate) struct TelemetryToolService<S> {
    store: Arc<S>,
    guard: Guard,
    signals: Arc<dyn Signals>,
}

impl<S: TelemetryStore> TelemetryToolService<S> {
    pub(crate) fn new(store: Arc<S>, guard: Guard, signals: Arc<dyn Signals>) -> Self {
        Self {
            store,
            guard,
            signals,
        }
    }

    /// The prefix as a field: the name's prefix when it is `chief` or a product
    /// in the registry, or `unknown`. A prefix that a caller makes up never
    /// becomes a series or a log field.
    fn known_prefix(&self, raw_name: &str) -> String {
        let Ok(name) = parse_name(raw_name) else {
            return UNKNOWN_PREFIX.into();
        };
        let known = name.prefix == CHIEF || self.guard.registry_prefixes().contains(&name.prefix);
        match known {
            true => name.prefix,
            false => UNKNOWN_PREFIX.into(),
        }
    }

    /// Counts the call, logs a refusal, and gives the reply.
    fn finish<T>(
        &self,
        kind: &'static str,
        raw_name: &str,
        result: Result<T, Failure>,
    ) -> Result<Response<T>, Status> {
        match result {
            Ok(reply) => {
                // The guard let the call through, so the prefix is known.
                let prefix = parse_name(raw_name).map(|n| n.prefix).unwrap_or_default();
                self.signals.call(&prefix, kind, "ok");
                Ok(Response::new(reply))
            }
            Err(Failure::Refused(refused)) => {
                let prefix = self.known_prefix(raw_name);
                let reason = refused.reason();
                self.signals.call(&prefix, kind, reason);
                info!(prefix, kind, reason, "telemetry call refused");
                Err(status(&refused))
            }
            Err(Failure::Store) => Err(unavailable()),
        }
    }

    /// Parses the name and checks the pane. A `limited` call also takes one
    /// from the rate limit.
    fn admit(&self, raw_name: &str, pane: &str, limited: bool) -> Result<String, Refused> {
        let name = parse_name(raw_name)?;
        self.guard.check_prefix(pane, &name.prefix)?;
        if limited {
            self.guard.take_write(&name.prefix)?;
        }
        Ok(name.full)
    }

    /// The declaration of a name that is written as `asked`.
    async fn declared(&self, name: &str, asked: Kind) -> Result<Declaration, Failure> {
        let Some(declaration) = self.store.get(name).await? else {
            return Err(Refused::Undeclared { name: name.into() }.into());
        };
        if declaration.kind != asked {
            return Err(Refused::WrongKind {
                name: name.into(),
                declared: declaration.kind,
                asked,
            }
            .into());
        }
        Ok(declaration)
    }

    async fn declare_call(&self, request: pb::DeclareRequest) -> Result<(), Failure> {
        let name = self.admit(&request.name, &request.caller_pane, false)?;
        let kind = kind_from_proto(request.kind)?;
        let attrs = request
            .attrs
            .iter()
            .map(|attr| parse_attr(&attr.key, &attr.values))
            .collect::<Result<Vec<_>, _>>()?;
        let declaration = Declaration {
            name,
            kind,
            meaning: request.meaning,
            attrs,
            created_ms: now_ms(),
            last_values: LastValues::new(),
        };
        declaration.validate()?;
        let prefix = parse_name(&declaration.name)?.prefix;
        // One transaction compares, checks the caps and stores, so two
        // declarations at once cannot both pass.
        match self.store.declare(&declaration).await? {
            Declared::Added => {}
            Declared::Same => return Ok(()),
            Declared::Differs => {
                return Err(Refused::DeclarationDiffers {
                    name: declaration.name,
                }
                .into());
            }
            Declared::TooManyNames => return Err(Refused::TooManyNames { prefix }.into()),
            Declared::TooManyEvents => return Err(Refused::TooManyEvents { prefix }.into()),
        }
        if kind == Kind::Counter {
            seed_counter(self.signals.as_ref(), &declaration);
        }
        Ok(())
    }

    async fn count_call(&self, request: pb::CountRequest) -> Result<(), Failure> {
        let name = self.admit(&request.name, &request.caller_pane, true)?;
        let declaration = self.declared(&name, Kind::Counter).await?;
        let pairs = checked_pairs(&declaration, &request.attrs)?;
        let n = match request.n {
            0 => 1,
            n if n <= MAX_COUNT => n,
            _ => return Err(Refused::Invalid(BAD_COUNT).into()),
        };
        // A `remove` that ends between the `declared` read and this line is the
        // one window: the count then makes the forgotten name again until the
        // next start. The store has no cheaper check than a second read.
        self.signals.counter_add(&name, n, &pairs);
        Ok(())
    }

    async fn gauge_call(&self, request: pb::GaugeRequest) -> Result<(), Failure> {
        let name = self.admit(&request.name, &request.caller_pane, true)?;
        let declaration = self.declared(&name, Kind::Gauge).await?;
        let pairs = checked_pairs(&declaration, &request.attrs)?;
        if !request.value.is_finite() {
            return Err(Refused::Invalid(BAD_GAUGE).into());
        }
        // The name was removed after the `declared` read: send nothing.
        if !self
            .store
            .set_last_value(&name, &series_key(&pairs), request.value)
            .await?
        {
            return Err(Refused::Undeclared { name }.into());
        }
        self.signals.gauge_set(&name, request.value, &pairs);
        Ok(())
    }

    async fn event_call(&self, request: pb::EventRequest) -> Result<(), Failure> {
        let name = self.admit(&request.name, &request.caller_pane, true)?;
        let declaration = self.declared(&name, Kind::Event).await?;
        let pairs = checked_pairs(&declaration, &request.attrs)?;
        // The same window as in `count_call`, but an event keeps no state: a
        // line written just after a `remove` is the only effect.
        self.signals.event(&name, &pairs);
        Ok(())
    }

    async fn remove_call(&self, request: pb::RemoveRequest) -> Result<(), Failure> {
        // A remove takes from the rate limit too: each one makes a new
        // signals provider.
        let name = self.admit(&request.name, &request.caller_pane, true)?;
        if !self.store.remove(&name).await? {
            return Err(Refused::Undeclared { name }.into());
        }
        self.signals.forget(&name);
        Ok(())
    }
}

/// The refusal as a gRPC status: the plain text as the message and the reason
/// word in the `reason` metadata.
fn status(refused: &Refused) -> Status {
    let code = match refused {
        Refused::NoPane | Refused::WrongPrefix { .. } | Refused::UnknownPrefix { .. } => {
            Code::PermissionDenied
        }
        Refused::TooManyNames { .. }
        | Refused::TooManyEvents { .. }
        | Refused::WrongKind { .. }
        | Refused::Undeclared { .. }
        | Refused::DeclarationDiffers { .. } => Code::FailedPrecondition,
        Refused::RateLimited { .. } => Code::ResourceExhausted,
        Refused::Invalid(text) if *text == REGISTRY_UNREADABLE => Code::FailedPrecondition,
        Refused::TooManySeries { .. }
        | Refused::UndeclaredValue { .. }
        | Refused::MeaningTooLong { .. }
        | Refused::Invalid(_) => Code::InvalidArgument,
    };
    let mut metadata = MetadataMap::new();
    metadata.insert(REASON_KEY, MetadataValue::from_static(refused.reason()));
    Status::with_metadata(code, refused.text(), metadata)
}

fn unavailable() -> Status {
    Status::unavailable("the telemetry database is unavailable: try again later")
}

fn kind_from_proto(kind: i32) -> Result<Kind, Refused> {
    match pb::Kind::try_from(kind) {
        Ok(pb::Kind::Counter) => Ok(Kind::Counter),
        Ok(pb::Kind::Gauge) => Ok(Kind::Gauge),
        Ok(pb::Kind::Event) => Ok(Kind::Event),
        Ok(pb::Kind::Unspecified) | Err(_) => Err(Refused::Invalid(BAD_KIND)),
    }
}

fn kind_to_proto(kind: Kind) -> pb::Kind {
    match kind {
        Kind::Counter => pb::Kind::Counter,
        Kind::Gauge => pb::Kind::Gauge,
        Kind::Event => pb::Kind::Event,
    }
}

fn values_to_proto(values: &ValueList) -> Vec<String> {
    match values {
        ValueList::Values(values) => values.clone(),
        ValueList::Range { .. } => vec![values.describe()],
    }
}

fn signal_to_proto(declaration: Declaration) -> pb::Signal {
    pb::Signal {
        name: declaration.name,
        kind: kind_to_proto(declaration.kind) as i32,
        meaning: declaration.meaning,
        attrs: declaration
            .attrs
            .iter()
            .map(|attr| pb::Attr {
                key: attr.key.clone(),
                values: values_to_proto(&attr.values),
            })
            .collect(),
    }
}

/// The written pairs in the declared key order, so a write lands on a seeded
/// series. Each declared key must have one declared value, and no other key
/// may come.
fn checked_pairs(
    declaration: &Declaration,
    pairs: &[pb::Pair],
) -> Result<Vec<(String, String)>, Refused> {
    for (i, pair) in pairs.iter().enumerate() {
        let Some(attr) = declaration.attrs.iter().find(|a| a.key == pair.key) else {
            let keys: Vec<String> = declaration.attrs.iter().map(|a| a.key.clone()).collect();
            let allowed = match keys.is_empty() {
                true => "no attributes".to_string(),
                false => ValueList::Values(keys).describe(),
            };
            return Err(Refused::UndeclaredValue {
                key: pair.key.clone(),
                value: pair.value.clone(),
                allowed,
            });
        };
        if !attr.values.contains(&pair.value) {
            return Err(Refused::UndeclaredValue {
                key: pair.key.clone(),
                value: pair.value.clone(),
                allowed: attr.values.describe(),
            });
        }
        if pairs[..i].iter().any(|p| p.key == pair.key) {
            return Err(Refused::Invalid(BAD_PAIRS));
        }
    }
    declaration
        .attrs
        .iter()
        .map(|attr| {
            pairs
                .iter()
                .find(|p| p.key == attr.key)
                .map(|p| (p.key.clone(), p.value.clone()))
                .ok_or(Refused::Invalid(BAD_PAIRS))
        })
        .collect()
}

/// Every value of the list. Called only on a validated counter, so a range
/// holds at most `MAX_SERIES` values.
fn every_value(values: &ValueList) -> Vec<String> {
    match values {
        ValueList::Values(values) => values.clone(),
        ValueList::Range { lo, hi } => (*lo..=*hi).map(|n| n.to_string()).collect(),
    }
}

/// Every series of the attributes: one attribute set for each combination of
/// values, in the declared key order. No attribute is one empty set.
pub(crate) fn every_series(attrs: &[Attr]) -> Vec<Vec<(String, String)>> {
    attrs.iter().fold(vec![Vec::new()], |sets, attr| {
        let values = every_value(&attr.values);
        sets.iter()
            .flat_map(|set| {
                values.iter().map(move |value| {
                    let mut next = set.clone();
                    next.push((attr.key.clone(), value.clone()));
                    next
                })
            })
            .collect()
    })
}

/// Counts 0 on every series of a declared counter, so each shows from the start.
pub(crate) fn seed_counter(signals: &dyn Signals, declaration: &Declaration) {
    for series in every_series(&declaration.attrs) {
        signals.counter_add(&declaration.name, 0, &series);
    }
}

fn now_ms() -> i64 {
    (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
}

#[tonic::async_trait]
impl<S: TelemetryStore> TelemetryService for TelemetryToolService<S> {
    async fn declare(
        &self,
        request: Request<pb::DeclareRequest>,
    ) -> Result<Response<pb::DeclareResponse>, Status> {
        let request = request.into_inner();
        let name = request.name.clone();
        let result = self.declare_call(request).await;
        self.finish(DECLARE, &name, result.map(|()| pb::DeclareResponse {}))
    }

    async fn count(
        &self,
        request: Request<pb::CountRequest>,
    ) -> Result<Response<pb::CountResponse>, Status> {
        let request = request.into_inner();
        let name = request.name.clone();
        let result = self.count_call(request).await;
        self.finish(COUNT, &name, result.map(|()| pb::CountResponse {}))
    }

    async fn gauge(
        &self,
        request: Request<pb::GaugeRequest>,
    ) -> Result<Response<pb::GaugeResponse>, Status> {
        let request = request.into_inner();
        let name = request.name.clone();
        let result = self.gauge_call(request).await;
        self.finish(GAUGE, &name, result.map(|()| pb::GaugeResponse {}))
    }

    async fn event(
        &self,
        request: Request<pb::EventRequest>,
    ) -> Result<Response<pb::EventResponse>, Status> {
        let request = request.into_inner();
        let name = request.name.clone();
        let result = self.event_call(request).await;
        self.finish(EVENT, &name, result.map(|()| pb::EventResponse {}))
    }

    /// Lists the declarations with their meanings. The list is a read: it is
    /// not counted, and its output is never logged.
    async fn list(
        &self,
        request: Request<pb::ListRequest>,
    ) -> Result<Response<pb::ListResponse>, Status> {
        let prefix = request.into_inner().prefix;
        let prefix = prefix.strip_suffix('.').unwrap_or(&prefix);
        let prefix = (!prefix.is_empty()).then_some(prefix);
        let signals = self
            .store
            .list(prefix)
            .await
            .map_err(|_| unavailable())?
            .into_iter()
            .map(signal_to_proto)
            .collect();
        Ok(Response::new(pb::ListResponse { signals }))
    }

    async fn remove(
        &self,
        request: Request<pb::RemoveRequest>,
    ) -> Result<Response<pb::RemoveResponse>, Status> {
        let request = request.into_inner();
        let name = request.name.clone();
        let result = self.remove_call(request).await;
        self.finish(REMOVE, &name, result.map(|()| pb::RemoveResponse {}))
    }
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;
