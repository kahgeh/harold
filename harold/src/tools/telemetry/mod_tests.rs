use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use harold_api::tools::telemetry::v1 as pb;
use harold_api::tools::telemetry::v1::telemetry_service_server::TelemetryService;
use tonic::Request;

use super::*;
use crate::settings::WatchSettings;
use crate::tools::WatchCounts;
use crate::tools::telemetry::test_support::{FakeChief, FakeFolders, FakeProducts, TempFolder};
use crate::tools::tests::{NoAgents, NoPods};

const CHIEF_PANE: &str = "%2";

/// What the tool sent through `Signals`, apart from the calls of the tool.
#[derive(Debug, Clone, PartialEq)]
enum Sent {
    Counter(String, u64, Vec<(String, String)>),
    Gauge(String, f64, Vec<(String, String)>),
    Event(String, Vec<(String, String)>),
    Forget(String),
    SeedCalls(Vec<String>),
}

#[derive(Default)]
struct FakeSignals(Mutex<Vec<Sent>>);

impl FakeSignals {
    fn sent(&self) -> Vec<Sent> {
        self.0.lock().unwrap().clone()
    }

    fn push(&self, sent: Sent) {
        self.0.lock().unwrap().push(sent);
    }
}

impl Signals for FakeSignals {
    fn counter_add(&self, name: &str, n: u64, attrs: &[(String, String)]) {
        self.push(Sent::Counter(name.into(), n, attrs.to_vec()));
    }

    fn gauge_set(&self, name: &str, value: f64, attrs: &[(String, String)]) {
        self.push(Sent::Gauge(name.into(), value, attrs.to_vec()));
    }

    fn event(&self, name: &str, attrs: &[(String, String)]) {
        self.push(Sent::Event(name.into(), attrs.to_vec()));
    }

    fn forget(&self, name: &str) {
        self.push(Sent::Forget(name.into()));
    }

    fn call(&self, _prefix: &str, _kind: &str, _outcome: &str) {}

    fn seed_calls(&self, prefixes: &[String]) {
        self.push(Sent::SeedCalls(prefixes.to_vec()));
    }
}

fn registry(root: &Path) -> Vec<(String, PathBuf)> {
    vec![("harold".into(), root.join("harold"))]
}

/// The context the host gives the tool, with the chief at `CHIEF_PANE`.
fn context(
    data_folder: PathBuf,
    products: Result<Vec<(String, PathBuf)>, &'static str>,
    shutdown: tokio::sync::watch::Receiver<()>,
) -> ToolContext {
    ToolContext {
        data_folder,
        typing: None,
        chief_pane: Arc::new(FakeChief(Some(CHIEF_PANE.into()))),
        pane_folder: Arc::new(FakeFolders::default()),
        products: Arc::new(FakeProducts(products)),
        agents: Arc::new(NoAgents),
        pods: Arc::new(NoPods),
        watch_settings: WatchSettings::default(),
        model_environment: Vec::new(),
        cli_path: "claude".into(),
        grpc_addr: None,
        pod_registry: None,
        watch_counts: Arc::new(tokio::sync::watch::channel(WatchCounts::default()).0),
        shutdown,
    }
}

/// A service over the store in `folder`, as the tool runs it before a restart.
async fn service_in(
    folder: &Path,
    root: &Path,
) -> service::TelemetryToolService<TursoTelemetryStore> {
    let store = Arc::new(TursoTelemetryStore::open(folder).await.unwrap());
    let guard = Guard::new(
        Arc::new(FakeFolders::default()),
        Arc::new(FakeProducts(Ok(registry(root)))),
        Arc::new(FakeChief(Some(CHIEF_PANE.into()))),
        Arc::new(SystemClock),
    );
    service::TelemetryToolService::new(store, guard, Arc::new(FakeSignals::default()))
}

fn attr(key: &str, values: &[&str]) -> pb::Attr {
    pb::Attr {
        key: key.into(),
        values: values.iter().map(|v| v.to_string()).collect(),
    }
}

fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

async fn declare<S: TelemetryStore>(
    service: &service::TelemetryToolService<S>,
    name: &str,
    kind: pb::Kind,
    attrs: Vec<pb::Attr>,
) {
    service
        .declare(Request::new(pb::DeclareRequest {
            caller_pane: CHIEF_PANE.into(),
            name: name.into(),
            kind: kind as i32,
            meaning: "A meaning".into(),
            attrs,
        }))
        .await
        .unwrap();
}

async fn gauge<S: TelemetryStore>(
    service: &service::TelemetryToolService<S>,
    name: &str,
    value: f64,
    list: &[(&str, &str)],
) {
    service
        .gauge(Request::new(pb::GaugeRequest {
            caller_pane: CHIEF_PANE.into(),
            name: name.into(),
            value,
            attrs: list
                .iter()
                .map(|(k, v)| pb::Pair {
                    key: k.to_string(),
                    value: v.to_string(),
                })
                .collect(),
        }))
        .await
        .unwrap();
}

async fn remove<S: TelemetryStore>(service: &service::TelemetryToolService<S>, name: &str) {
    service
        .remove(Request::new(pb::RemoveRequest {
            caller_pane: CHIEF_PANE.into(),
            name: name.into(),
        }))
        .await
        .unwrap();
}

/// Starts the tool on `folder` with a fake `Signals`, and gives what it sent at start.
async fn restart(folder: &Path, root: &Path) -> Vec<Sent> {
    let signals = Arc::new(FakeSignals::default());
    let (shutdown, shutdown_rx) = tokio::sync::watch::channel(());
    let handle = start_with(
        context(folder.to_path_buf(), Ok(registry(root)), shutdown_rx),
        signals.clone(),
    )
    .await
    .unwrap();
    drop(shutdown);
    tokio::time::timeout(Duration::from_secs(5), handle.task)
        .await
        .expect("the tool stops at shutdown")
        .unwrap();
    signals.sent()
}

#[tokio::test]
async fn a_restart_seeds_each_counter_series_and_sends_each_gauge_value_again() {
    let tmp = TempFolder::new();
    let folder = tmp.0.join("tools").join(NAME);
    let service = service_in(&folder, &tmp.0).await;
    declare(
        &service,
        "chief.attempts",
        pb::Kind::Counter,
        vec![attr("step", &["1-2"]), attr("result", &["ok", "failed"])],
    )
    .await;
    // The declared key order (`product`, `kind`) is not the sorted order of the series key.
    declare(
        &service,
        "chief.queue",
        pb::Kind::Gauge,
        vec![
            attr("product", &["harold", "turso"]),
            attr("kind", &["spec", "build"]),
        ],
    )
    .await;
    declare(&service, "chief.depth", pb::Kind::Gauge, Vec::new()).await;
    declare(&service, "chief.idle", pb::Kind::Gauge, Vec::new()).await;
    declare(
        &service,
        "chief.done",
        pb::Kind::Event,
        vec![attr("result", &["ok"])],
    )
    .await;
    gauge(
        &service,
        "chief.queue",
        3.0,
        &[("product", "harold"), ("kind", "build")],
    )
    .await;
    gauge(
        &service,
        "chief.queue",
        5.0,
        &[("kind", "spec"), ("product", "turso")],
    )
    .await;
    gauge(
        &service,
        "chief.queue",
        6.0,
        &[("product", "turso"), ("kind", "spec")],
    )
    .await;
    gauge(&service, "chief.depth", 1.5, &[]).await;
    drop(service);

    let sent = restart(&folder, &tmp.0).await;

    let counter = |step: &str, result: &str| {
        Sent::Counter(
            "chief.attempts".into(),
            0,
            pairs(&[("step", step), ("result", result)]),
        )
    };
    assert_eq!(
        sent,
        [
            Sent::SeedCalls(vec!["harold".into(), "chief".into()]),
            // The store lists by name.
            counter("1", "ok"),
            counter("1", "failed"),
            counter("2", "ok"),
            counter("2", "failed"),
            Sent::Gauge("chief.depth".into(), 1.5, Vec::new()),
            // An event and a gauge with no value send nothing.
            Sent::Gauge(
                "chief.queue".into(),
                3.0,
                pairs(&[("product", "harold"), ("kind", "build")])
            ),
            Sent::Gauge(
                "chief.queue".into(),
                6.0,
                pairs(&[("product", "turso"), ("kind", "spec")])
            ),
        ]
    );
    println!("evidence: sent at restart {sent:?}");
}

#[tokio::test]
async fn a_removed_name_is_neither_seeded_nor_sent_at_a_restart() {
    let tmp = TempFolder::new();
    let folder = tmp.0.join("tools").join(NAME);
    let service = service_in(&folder, &tmp.0).await;
    declare(
        &service,
        "chief.old",
        pb::Kind::Counter,
        vec![attr("result", &["ok", "failed"])],
    )
    .await;
    declare(
        &service,
        "chief.gone",
        pb::Kind::Gauge,
        vec![attr("result", &["ok"])],
    )
    .await;
    gauge(&service, "chief.gone", 4.0, &[("result", "ok")]).await;
    declare(&service, "chief.kept", pb::Kind::Counter, Vec::new()).await;
    remove(&service, "chief.old").await;
    remove(&service, "chief.gone").await;
    drop(service);

    let sent = restart(&folder, &tmp.0).await;

    assert_eq!(
        sent,
        [
            Sent::SeedCalls(vec!["harold".into(), "chief".into()]),
            Sent::Counter("chief.kept".into(), 0, Vec::new()),
        ]
    );
}

#[tokio::test]
async fn an_empty_store_seeds_only_the_calls_counter() {
    let tmp = TempFolder::new();
    let sent = restart(&tmp.0.join("tools").join(NAME), &tmp.0).await;
    assert_eq!(
        sent,
        [Sent::SeedCalls(vec!["harold".into(), "chief".into()])]
    );
    assert!(tmp.0.join("tools/telemetry/telemetry.db").is_file());
}

#[tokio::test]
async fn an_unreadable_registry_seeds_the_calls_counter_for_the_chief_only() {
    let tmp = TempFolder::new();
    let signals = Arc::new(FakeSignals::default());
    let (_shutdown, shutdown_rx) = tokio::sync::watch::channel(());
    let started = start_with(
        context(
            tmp.0.join("telemetry"),
            Err("registry_unreadable"),
            shutdown_rx,
        ),
        signals.clone(),
    )
    .await;
    assert!(started.is_ok());
    assert_eq!(signals.sent(), [Sent::SeedCalls(vec!["chief".into()])]);
}

#[tokio::test]
async fn a_store_that_cannot_open_fails_with_store_open_failed() {
    let tmp = TempFolder::new();
    // A file where the data folder must be.
    let folder = tmp.0.join("telemetry");
    std::fs::write(&folder, "not a folder").unwrap();
    let signals = Arc::new(FakeSignals::default());
    let (_shutdown, shutdown_rx) = tokio::sync::watch::channel(());
    let started = start_with(
        context(folder, Ok(Vec::new()), shutdown_rx),
        signals.clone(),
    )
    .await;
    assert_eq!(started.err().map(|e| e.reason), Some("store_open_failed"));
    assert!(signals.sent().is_empty());
}
