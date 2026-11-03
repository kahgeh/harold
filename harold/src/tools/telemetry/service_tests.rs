use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use harold_api::tools::telemetry::v1 as pb;
use harold_api::tools::telemetry::v1::telemetry_service_server::TelemetryService;
use tonic::{Code, Request};

use super::*;
use crate::outbound::fake::CapturedLogs;
use crate::tools::telemetry::guard::Clock;
use crate::tools::telemetry::signals::ExportedSignals;
use crate::tools::telemetry::store::TursoTelemetryStore;
use crate::tools::telemetry::test_support::{FakeChief, FakeFolders, FakeProducts, TempFolder};

const CHIEF_PANE: &str = "%2";
const HAROLD_PANE: &str = "%40";
const TURSO_PANE: &str = "%51";
const ATTEMPTS: &str = "harold.release.attempts";
const MEANING: &str = "Release attempts, by result";

type Service = TelemetryToolService<TursoTelemetryStore>;

/// What the service sent through `Signals`.
#[derive(Debug, Clone, PartialEq)]
enum Sent {
    Counter(String, u64, Vec<(String, String)>),
    Gauge(String, f64, Vec<(String, String)>),
    Event(String, Vec<(String, String)>),
    Forget(String),
    Call(String, String, String),
}

#[derive(Default)]
struct FakeSignals(Mutex<Vec<Sent>>);

impl FakeSignals {
    fn sent(&self) -> Vec<Sent> {
        self.0.lock().unwrap().clone()
    }

    fn clear(&self) {
        self.0.lock().unwrap().clear();
    }

    fn calls(&self) -> Vec<(String, String, String)> {
        self.sent()
            .into_iter()
            .filter_map(|sent| match sent {
                Sent::Call(prefix, kind, outcome) => Some((prefix, kind, outcome)),
                _ => None,
            })
            .collect()
    }

    /// Everything but the calls of the tool.
    fn records(&self) -> Vec<Sent> {
        self.sent()
            .into_iter()
            .filter(|sent| !matches!(sent, Sent::Call(..)))
            .collect()
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

    fn call(&self, prefix: &str, kind: &str, outcome: &str) {
        self.push(Sent::Call(prefix.into(), kind.into(), outcome.into()));
    }
}

struct FakeClock(AtomicI64);

impl Clock for FakeClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// A store in a temp folder and a service over it: products `harold` and
/// `turso`, each with a folder and a pane in it, and the chief at `%2`.
struct Setup {
    tmp: TempFolder,
    store: Arc<TursoTelemetryStore>,
    folders: Arc<FakeFolders>,
    clock: Arc<FakeClock>,
    signals: Arc<FakeSignals>,
}

impl Setup {
    async fn new() -> Self {
        let tmp = TempFolder::new();
        for repo in ["harold", "turso"] {
            std::fs::create_dir_all(tmp.0.join(repo)).unwrap();
        }
        let store = Arc::new(TursoTelemetryStore::open(&tmp.0.join("db")).await.unwrap());
        let folders = Arc::new(FakeFolders::default());
        {
            let mut map = folders.0.lock().unwrap();
            map.insert(HAROLD_PANE.into(), tmp.0.join("harold"));
            map.insert(TURSO_PANE.into(), tmp.0.join("turso"));
        }
        Self {
            tmp,
            store,
            folders,
            clock: Arc::new(FakeClock(AtomicI64::new(1_000_000))),
            signals: Arc::new(FakeSignals::default()),
        }
    }

    fn registry(&self) -> Vec<(String, PathBuf)> {
        ["harold", "turso"]
            .into_iter()
            .map(|name| (name.to_string(), self.tmp.0.join(name)))
            .collect()
    }

    fn guard_with(&self, registry: Result<Vec<(String, PathBuf)>, &'static str>) -> Guard {
        Guard::new(
            self.folders.clone(),
            Arc::new(FakeProducts(registry)),
            Arc::new(FakeChief(Some(CHIEF_PANE.into()))),
            self.clock.clone(),
        )
    }

    fn service_with(&self, registry: Result<Vec<(String, PathBuf)>, &'static str>) -> Service {
        TelemetryToolService::new(
            Arc::clone(&self.store),
            self.guard_with(registry),
            self.signals.clone(),
        )
    }

    fn service(&self) -> Service {
        self.service_with(Ok(self.registry()))
    }
}

fn attr(key: &str, values: &[&str]) -> pb::Attr {
    pb::Attr {
        key: key.into(),
        values: values.iter().map(|v| v.to_string()).collect(),
    }
}

fn pair(key: &str, value: &str) -> pb::Pair {
    pb::Pair {
        key: key.into(),
        value: value.into(),
    }
}

fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn declare_request(
    pane: &str,
    name: &str,
    kind: pb::Kind,
    meaning: &str,
    attrs: Vec<pb::Attr>,
) -> pb::DeclareRequest {
    pb::DeclareRequest {
        caller_pane: pane.into(),
        name: name.into(),
        kind: kind as i32,
        meaning: meaning.into(),
        attrs,
    }
}

async fn declare(
    service: &Service,
    pane: &str,
    name: &str,
    kind: pb::Kind,
    attrs: Vec<pb::Attr>,
) -> Result<(), tonic::Status> {
    declare_meaning(service, pane, name, kind, MEANING, attrs).await
}

async fn declare_meaning(
    service: &Service,
    pane: &str,
    name: &str,
    kind: pb::Kind,
    meaning: &str,
    attrs: Vec<pb::Attr>,
) -> Result<(), tonic::Status> {
    service
        .declare(Request::new(declare_request(
            pane, name, kind, meaning, attrs,
        )))
        .await
        .map(|_| ())
}

/// Declares `harold.release.attempts` as a counter with `result=ok,failed`.
async fn declare_attempts(service: &Service) {
    declare(
        service,
        HAROLD_PANE,
        ATTEMPTS,
        pb::Kind::Counter,
        vec![attr("result", &["ok", "failed"])],
    )
    .await
    .unwrap();
}

async fn count(
    service: &Service,
    pane: &str,
    name: &str,
    n: u64,
    attrs: Vec<pb::Pair>,
) -> Result<(), tonic::Status> {
    service
        .count(Request::new(pb::CountRequest {
            caller_pane: pane.into(),
            name: name.into(),
            n,
            attrs,
        }))
        .await
        .map(|_| ())
}

async fn gauge(
    service: &Service,
    pane: &str,
    name: &str,
    value: f64,
    attrs: Vec<pb::Pair>,
) -> Result<(), tonic::Status> {
    service
        .gauge(Request::new(pb::GaugeRequest {
            caller_pane: pane.into(),
            name: name.into(),
            value,
            attrs,
        }))
        .await
        .map(|_| ())
}

async fn event(
    service: &Service,
    pane: &str,
    name: &str,
    attrs: Vec<pb::Pair>,
) -> Result<(), tonic::Status> {
    service
        .event(Request::new(pb::EventRequest {
            caller_pane: pane.into(),
            name: name.into(),
            attrs,
        }))
        .await
        .map(|_| ())
}

async fn remove(service: &Service, pane: &str, name: &str) -> Result<(), tonic::Status> {
    service
        .remove(Request::new(pb::RemoveRequest {
            caller_pane: pane.into(),
            name: name.into(),
        }))
        .await
        .map(|_| ())
}

async fn list(service: &Service, prefix: &str) -> Vec<pb::Signal> {
    service
        .list(Request::new(pb::ListRequest {
            caller_pane: HAROLD_PANE.into(),
            prefix: prefix.into(),
        }))
        .await
        .unwrap()
        .into_inner()
        .signals
}

/// The code, the reason word and the text of a refusal.
fn refusal(result: Result<(), tonic::Status>) -> (Code, String, String) {
    let status = result.expect_err("refused");
    let reason = status
        .metadata()
        .get(REASON_KEY)
        .expect("a reason word")
        .to_str()
        .unwrap()
        .to_string();
    (status.code(), reason, status.message().to_string())
}

fn refused(code: Code, reason: &str, text: &str) -> (Code, String, String) {
    (code, reason.into(), text.into())
}

fn call(prefix: &str, kind: &str, outcome: &str) -> (String, String, String) {
    (prefix.into(), kind.into(), outcome.into())
}

// ---------------------------------------------------------------------------
// Prefix rules
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_chief_declares_under_chief_and_any_product() {
    let s = Setup::new().await;
    let service = s.service();
    for name in ["chief.digest", "harold.a", "turso.b"] {
        declare(&service, CHIEF_PANE, name, pb::Kind::Event, vec![])
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn a_pane_in_a_product_folder_writes_that_prefix_only() {
    let s = Setup::new().await;
    let service = s.service();
    declare_attempts(&service).await;
    assert_eq!(
        refusal(declare(&service, HAROLD_PANE, "turso.b", pb::Kind::Event, vec![]).await),
        refused(
            Code::PermissionDenied,
            "wrong_prefix",
            "turso. is for panes in turso's folder and the chief"
        )
    );
    assert_eq!(
        refusal(declare(&service, TURSO_PANE, "harold.b", pb::Kind::Event, vec![]).await),
        refused(
            Code::PermissionDenied,
            "wrong_prefix",
            "harold. is for panes in harold's folder and the chief"
        )
    );
    assert_eq!(
        refusal(declare(&service, HAROLD_PANE, "chief.b", pb::Kind::Event, vec![]).await),
        refused(
            Code::PermissionDenied,
            "wrong_prefix",
            "chief. is for the chief's pane"
        )
    );
}

#[tokio::test]
async fn a_call_with_no_pane_is_no_pane() {
    let s = Setup::new().await;
    let service = s.service();
    let expected = refused(
        Code::PermissionDenied,
        "no_pane",
        "run this from a pane: no TMUX_PANE or REX_BLOCK",
    );
    assert_eq!(
        refusal(declare(&service, "", ATTEMPTS, pb::Kind::Counter, vec![]).await),
        expected
    );
    assert_eq!(
        refusal(count(&service, "", ATTEMPTS, 1, vec![]).await),
        expected
    );
    assert_eq!(refusal(remove(&service, "", ATTEMPTS).await), expected);
}

#[tokio::test]
async fn an_unknown_prefix_is_refused_and_never_becomes_a_series() {
    let s = Setup::new().await;
    let service = s.service();
    let logs = CapturedLogs::start();
    assert_eq!(
        refusal(
            declare(
                &service,
                CHIEF_PANE,
                "web.visits",
                pb::Kind::Counter,
                vec![]
            )
            .await
        ),
        refused(
            Code::PermissionDenied,
            "unknown_prefix",
            "unknown prefix web.: use chief. or a product name"
        )
    );
    assert_eq!(
        s.signals.calls(),
        vec![call("", "declare", "unknown_prefix")]
    );
    assert!(!logs.text().contains("web"), "{}", logs.text());
}

#[tokio::test]
async fn an_unreadable_registry_is_failed_precondition() {
    let s = Setup::new().await;
    let service = s.service_with(Err("broken"));
    let (code, reason, _) =
        refusal(declare(&service, HAROLD_PANE, ATTEMPTS, pb::Kind::Counter, vec![]).await);
    assert_eq!(
        (code, reason.as_str()),
        (Code::FailedPrecondition, "invalid")
    );
    let (code, reason, _) =
        refusal(declare(&service, CHIEF_PANE, ATTEMPTS, pb::Kind::Counter, vec![]).await);
    assert_eq!(
        (code, reason.as_str()),
        (Code::FailedPrecondition, "invalid")
    );
    // The chief's own prefix needs no registry.
    declare(
        &service,
        CHIEF_PANE,
        "chief.digest",
        pb::Kind::Event,
        vec![],
    )
    .await
    .unwrap();
}

// ---------------------------------------------------------------------------
// Refusal texts and caps
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_51st_name_of_a_prefix_is_too_many_names() {
    let s = Setup::new().await;
    let service = s.service();
    for i in 0..50 {
        declare(
            &service,
            HAROLD_PANE,
            &format!("harold.n{i}"),
            pb::Kind::Counter,
            vec![],
        )
        .await
        .unwrap();
    }
    assert_eq!(
        refusal(
            declare(
                &service,
                HAROLD_PANE,
                "harold.n50",
                pb::Kind::Counter,
                vec![]
            )
            .await
        ),
        refused(
            Code::FailedPrecondition,
            "too_many_names",
            "harold. has 50 names: remove one first"
        )
    );
    // The same declaration again is still accepted at the cap.
    declare(
        &service,
        HAROLD_PANE,
        "harold.n0",
        pb::Kind::Counter,
        vec![],
    )
    .await
    .unwrap();
    // Another prefix has its own cap.
    declare(&service, CHIEF_PANE, "turso.n0", pb::Kind::Counter, vec![])
        .await
        .unwrap();
}

#[tokio::test]
async fn the_21st_event_of_a_prefix_is_too_many_names() {
    let s = Setup::new().await;
    let service = s.service();
    for i in 0..20 {
        declare(
            &service,
            HAROLD_PANE,
            &format!("harold.e{i}"),
            pb::Kind::Event,
            vec![],
        )
        .await
        .unwrap();
    }
    assert_eq!(
        refusal(declare(&service, HAROLD_PANE, "harold.e20", pb::Kind::Event, vec![]).await),
        refused(
            Code::FailedPrecondition,
            "too_many_names",
            "harold. has 20 events: remove one first"
        )
    );
    declare(&service, HAROLD_PANE, "harold.c", pb::Kind::Counter, vec![])
        .await
        .unwrap();
}

#[tokio::test]
async fn a_fourth_attribute_key_is_invalid() {
    let s = Setup::new().await;
    let service = s.service();
    let keys = ["a", "b", "c", "d"].map(|k| attr(k, &["x"]));
    let (code, reason, _) = refusal(
        declare(
            &service,
            HAROLD_PANE,
            "harold.k",
            pb::Kind::Counter,
            keys.to_vec(),
        )
        .await,
    );
    assert_eq!((code, reason.as_str()), (Code::InvalidArgument, "invalid"));
    declare(
        &service,
        HAROLD_PANE,
        "harold.k",
        pb::Kind::Counter,
        keys[..3].to_vec(),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn a_counter_over_100_series_is_too_many_series() {
    let s = Setup::new().await;
    let service = s.service();
    let attrs = vec![attr("result", &["ok", "failed"]), attr("step", &["0-59"])];
    assert_eq!(
        refusal(declare(&service, HAROLD_PANE, ATTEMPTS, pb::Kind::Counter, attrs).await),
        refused(
            Code::InvalidArgument,
            "too_many_series",
            "harold.release.attempts would have 120 series: the most is 100"
        )
    );
    // 100 series exactly is accepted, and each is seeded.
    let attrs = vec![attr("result", &["ok", "failed"]), attr("step", &["0-49"])];
    declare(&service, HAROLD_PANE, ATTEMPTS, pb::Kind::Counter, attrs)
        .await
        .unwrap();
    assert_eq!(s.signals.records().len(), 100);
}

#[tokio::test]
async fn an_event_has_no_series_cap() {
    let s = Setup::new().await;
    let service = s.service();
    declare(
        &service,
        HAROLD_PANE,
        "harold.big",
        pb::Kind::Event,
        vec![attr("n", &["0-1000"])],
    )
    .await
    .unwrap();
    assert!(s.signals.records().is_empty());
}

#[tokio::test]
async fn a_meaning_of_95_characters_is_meaning_too_long() {
    let s = Setup::new().await;
    let service = s.service();
    let meaning = "m".repeat(95);
    assert_eq!(
        refusal(
            declare_meaning(
                &service,
                HAROLD_PANE,
                ATTEMPTS,
                pb::Kind::Counter,
                &meaning,
                vec![]
            )
            .await
        ),
        refused(
            Code::InvalidArgument,
            "meaning_too_long",
            "the meaning has 95 characters: the most is 80"
        )
    );
    declare_meaning(
        &service,
        HAROLD_PANE,
        ATTEMPTS,
        pb::Kind::Counter,
        &"m".repeat(80),
        vec![],
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn an_undeclared_value_is_refused_with_the_declared_ones() {
    let s = Setup::new().await;
    let service = s.service();
    declare_attempts(&service).await;
    s.signals.clear();
    assert_eq!(
        refusal(
            count(
                &service,
                HAROLD_PANE,
                ATTEMPTS,
                1,
                vec![pair("result", "maybe")]
            )
            .await
        ),
        refused(
            Code::InvalidArgument,
            "undeclared_value",
            "result=maybe is not declared: use ok or failed"
        )
    );
    assert_eq!(
        refusal(
            count(
                &service,
                HAROLD_PANE,
                ATTEMPTS,
                1,
                vec![pair("colour", "red")]
            )
            .await
        ),
        refused(
            Code::InvalidArgument,
            "undeclared_value",
            "colour=red is not declared: use result"
        )
    );
    assert!(s.signals.records().is_empty());
}

#[tokio::test]
async fn a_missing_or_repeated_key_is_invalid() {
    let s = Setup::new().await;
    let service = s.service();
    declare_attempts(&service).await;
    s.signals.clear();
    let (code, reason, _) = refusal(count(&service, HAROLD_PANE, ATTEMPTS, 1, vec![]).await);
    assert_eq!((code, reason.as_str()), (Code::InvalidArgument, "invalid"));
    let twice = vec![pair("result", "ok"), pair("result", "failed")];
    let (code, reason, _) = refusal(count(&service, HAROLD_PANE, ATTEMPTS, 1, twice).await);
    assert_eq!((code, reason.as_str()), (Code::InvalidArgument, "invalid"));
    assert!(s.signals.records().is_empty());
}

#[tokio::test]
async fn a_write_of_another_kind_is_wrong_kind() {
    let s = Setup::new().await;
    let service = s.service();
    declare_attempts(&service).await;
    assert_eq!(
        refusal(
            gauge(
                &service,
                HAROLD_PANE,
                ATTEMPTS,
                1.0,
                vec![pair("result", "ok")]
            )
            .await
        ),
        refused(
            Code::FailedPrecondition,
            "wrong_kind",
            "harold.release.attempts is a counter, not a gauge"
        )
    );
    assert_eq!(
        refusal(event(&service, HAROLD_PANE, ATTEMPTS, vec![pair("result", "ok")]).await),
        refused(
            Code::FailedPrecondition,
            "wrong_kind",
            "harold.release.attempts is a counter, not an event"
        )
    );
}

#[tokio::test]
async fn a_write_of_an_undeclared_name_is_undeclared() {
    let s = Setup::new().await;
    let service = s.service();
    let expected = refused(
        Code::FailedPrecondition,
        "undeclared",
        "harold.release.attempts is not declared: declare it first",
    );
    assert_eq!(
        refusal(count(&service, HAROLD_PANE, ATTEMPTS, 1, vec![]).await),
        expected
    );
    assert_eq!(
        refusal(gauge(&service, HAROLD_PANE, ATTEMPTS, 1.0, vec![]).await),
        expected
    );
    assert_eq!(
        refusal(event(&service, HAROLD_PANE, ATTEMPTS, vec![]).await),
        expected
    );
    assert_eq!(
        refusal(remove(&service, HAROLD_PANE, ATTEMPTS).await),
        expected
    );
}

#[tokio::test]
async fn the_61st_write_in_a_minute_is_rate_limited() {
    let s = Setup::new().await;
    let service = s.service();
    declare_attempts(&service).await;
    for _ in 0..60 {
        count(
            &service,
            HAROLD_PANE,
            ATTEMPTS,
            1,
            vec![pair("result", "ok")],
        )
        .await
        .unwrap();
    }
    assert_eq!(
        refusal(
            count(
                &service,
                HAROLD_PANE,
                ATTEMPTS,
                1,
                vec![pair("result", "ok")]
            )
            .await
        ),
        refused(
            Code::ResourceExhausted,
            "rate_limited",
            "harold. wrote 60 times this minute: wait"
        )
    );
    // A declaration is not a write, and another prefix has its own limit.
    declare(
        &service,
        HAROLD_PANE,
        "harold.other",
        pb::Kind::Event,
        vec![],
    )
    .await
    .unwrap();
    declare(
        &service,
        CHIEF_PANE,
        "chief.digest",
        pb::Kind::Event,
        vec![],
    )
    .await
    .unwrap();
    event(&service, CHIEF_PANE, "chief.digest", vec![])
        .await
        .unwrap();
    s.clock.0.fetch_add(60_000, Ordering::SeqCst);
    count(
        &service,
        HAROLD_PANE,
        ATTEMPTS,
        1,
        vec![pair("result", "ok")],
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn an_unspecified_kind_is_invalid() {
    let s = Setup::new().await;
    let service = s.service();
    let (code, reason, text) = refusal(
        declare(
            &service,
            HAROLD_PANE,
            ATTEMPTS,
            pb::Kind::Unspecified,
            vec![],
        )
        .await,
    );
    assert_eq!((code, reason.as_str()), (Code::InvalidArgument, "invalid"));
    assert_eq!(text, "invalid kind: use counter, gauge or event");
    assert_eq!(
        s.signals.calls(),
        vec![call("harold", "declare", "invalid")]
    );
}

#[tokio::test]
async fn a_bad_name_is_invalid_with_no_prefix() {
    let s = Setup::new().await;
    let service = s.service();
    for name in [
        "Harold.x",
        "harold",
        ".x",
        "harold..x",
        &format!("harold.{}", "x".repeat(58)),
    ] {
        let (code, reason, _) =
            refusal(declare(&service, HAROLD_PANE, name, pb::Kind::Counter, vec![]).await);
        assert_eq!(
            (code, reason.as_str()),
            (Code::InvalidArgument, "invalid"),
            "{name}"
        );
    }
    assert!(
        s.signals
            .calls()
            .iter()
            .all(|c| *c == call("", "declare", "invalid"))
    );
    assert!(list(&service, "").await.is_empty());
}

// ---------------------------------------------------------------------------
// Declarations
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_counter_is_seeded_at_0_for_each_series_at_declare() {
    let s = Setup::new().await;
    let service = s.service();
    declare(
        &service,
        HAROLD_PANE,
        ATTEMPTS,
        pb::Kind::Counter,
        vec![attr("result", &["ok", "failed"]), attr("step", &["1-2"])],
    )
    .await
    .unwrap();
    let seeded = |r: &str, step: &str| {
        Sent::Counter(ATTEMPTS.into(), 0, pairs(&[("result", r), ("step", step)]))
    };
    assert_eq!(
        s.signals.records(),
        vec![
            seeded("ok", "1"),
            seeded("ok", "2"),
            seeded("failed", "1"),
            seeded("failed", "2"),
        ]
    );
}

#[tokio::test]
async fn a_counter_with_no_attributes_is_seeded_once() {
    let s = Setup::new().await;
    let service = s.service();
    declare(
        &service,
        HAROLD_PANE,
        "harold.runs",
        pb::Kind::Counter,
        vec![],
    )
    .await
    .unwrap();
    assert_eq!(
        s.signals.records(),
        vec![Sent::Counter("harold.runs".into(), 0, vec![])]
    );
}

#[tokio::test]
async fn a_gauge_or_an_event_is_not_seeded() {
    let s = Setup::new().await;
    let service = s.service();
    declare(
        &service,
        HAROLD_PANE,
        "harold.depth",
        pb::Kind::Gauge,
        vec![attr("result", &["ok"])],
    )
    .await
    .unwrap();
    declare(
        &service,
        HAROLD_PANE,
        "harold.done",
        pb::Kind::Event,
        vec![attr("result", &["ok"])],
    )
    .await
    .unwrap();
    assert!(s.signals.records().is_empty());
}

#[tokio::test]
async fn the_same_declaration_twice_is_ok_and_changes_nothing() {
    let s = Setup::new().await;
    let service = s.service();
    declare_attempts(&service).await;
    let before = s.store.get(ATTEMPTS).await.unwrap().unwrap();
    s.signals.clear();
    // Another meaning and another value order are the same declaration.
    declare_meaning(
        &service,
        HAROLD_PANE,
        ATTEMPTS,
        pb::Kind::Counter,
        "Another meaning",
        vec![attr("result", &["failed", "ok"])],
    )
    .await
    .unwrap();
    assert_eq!(s.store.get(ATTEMPTS).await.unwrap().unwrap(), before);
    assert!(s.signals.records().is_empty());
    assert_eq!(s.signals.calls(), vec![call("harold", "declare", "ok")]);
}

#[tokio::test]
async fn two_declarations_at_once_of_one_name_let_exactly_one_win() {
    let s = Setup::new().await;
    let service = s.service();
    let counter = declare(
        &service,
        HAROLD_PANE,
        ATTEMPTS,
        pb::Kind::Counter,
        vec![attr("result", &["ok", "failed"])],
    );
    let gauge = declare(&service, HAROLD_PANE, ATTEMPTS, pb::Kind::Gauge, vec![]);
    let (counter, gauge) = tokio::join!(counter, gauge);
    let stored = s.store.get(ATTEMPTS).await.unwrap().unwrap();
    let (winner, loser, kind) = match counter.is_ok() {
        true => (counter, gauge, Kind::Counter),
        false => (gauge, counter, Kind::Gauge),
    };
    winner.unwrap();
    assert_eq!(refusal(loser).1, "declaration_differs");
    assert_eq!(stored.kind, kind);
}

#[tokio::test]
async fn a_different_declaration_is_declaration_differs() {
    let s = Setup::new().await;
    let service = s.service();
    declare_attempts(&service).await;
    let before = s.store.get(ATTEMPTS).await.unwrap().unwrap();
    let expected = refused(
        Code::FailedPrecondition,
        "declaration_differs",
        "harold.release.attempts is declared with other values: remove it and declare it again",
    );
    assert_eq!(
        refusal(
            declare(
                &service,
                HAROLD_PANE,
                ATTEMPTS,
                pb::Kind::Gauge,
                vec![attr("result", &["ok", "failed"])]
            )
            .await
        ),
        expected
    );
    assert_eq!(
        refusal(
            declare(
                &service,
                HAROLD_PANE,
                ATTEMPTS,
                pb::Kind::Counter,
                vec![attr("result", &["ok", "failed", "skipped"])]
            )
            .await
        ),
        expected
    );
    assert_eq!(s.store.get(ATTEMPTS).await.unwrap().unwrap(), before);
}

// ---------------------------------------------------------------------------
// Writes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_count_adds_n_with_the_attributes_in_declared_order() {
    let s = Setup::new().await;
    let service = s.service();
    declare(
        &service,
        HAROLD_PANE,
        ATTEMPTS,
        pb::Kind::Counter,
        vec![attr("result", &["ok", "failed"]), attr("step", &["1-3"])],
    )
    .await
    .unwrap();
    s.signals.clear();
    count(
        &service,
        HAROLD_PANE,
        ATTEMPTS,
        3,
        vec![pair("step", "2"), pair("result", "failed")],
    )
    .await
    .unwrap();
    assert_eq!(
        s.signals.records(),
        vec![Sent::Counter(
            ATTEMPTS.into(),
            3,
            pairs(&[("result", "failed"), ("step", "2")])
        )]
    );
}

#[tokio::test]
async fn a_count_of_0_is_1_and_the_most_is_1000000() {
    let s = Setup::new().await;
    let service = s.service();
    declare(
        &service,
        HAROLD_PANE,
        "harold.runs",
        pb::Kind::Counter,
        vec![],
    )
    .await
    .unwrap();
    s.signals.clear();
    count(&service, HAROLD_PANE, "harold.runs", 0, vec![])
        .await
        .unwrap();
    count(&service, HAROLD_PANE, "harold.runs", 1_000_000, vec![])
        .await
        .unwrap();
    let (code, reason, _) =
        refusal(count(&service, HAROLD_PANE, "harold.runs", 1_000_001, vec![]).await);
    assert_eq!((code, reason.as_str()), (Code::InvalidArgument, "invalid"));
    assert_eq!(
        s.signals.records(),
        vec![
            Sent::Counter("harold.runs".into(), 1, vec![]),
            Sent::Counter("harold.runs".into(), 1_000_000, vec![]),
        ]
    );
}

#[tokio::test]
async fn a_gauge_sets_its_value_and_keeps_the_last_one() {
    let s = Setup::new().await;
    let service = s.service();
    declare(
        &service,
        HAROLD_PANE,
        "harold.depth",
        pb::Kind::Gauge,
        vec![],
    )
    .await
    .unwrap();
    gauge(&service, HAROLD_PANE, "harold.depth", 3.5, vec![])
        .await
        .unwrap();
    gauge(&service, HAROLD_PANE, "harold.depth", 4.25, vec![])
        .await
        .unwrap();
    let (code, reason, _) =
        refusal(gauge(&service, HAROLD_PANE, "harold.depth", f64::NAN, vec![]).await);
    assert_eq!((code, reason.as_str()), (Code::InvalidArgument, "invalid"));
    assert_eq!(
        s.store
            .get("harold.depth")
            .await
            .unwrap()
            .unwrap()
            .last_values,
        LastValues::from([(String::new(), 4.25)])
    );
    assert_eq!(
        s.signals.records(),
        vec![
            Sent::Gauge("harold.depth".into(), 3.5, vec![]),
            Sent::Gauge("harold.depth".into(), 4.25, vec![]),
        ]
    );
}

#[tokio::test]
async fn a_gauge_with_two_series_keeps_both_values_after_reopen() {
    let s = Setup::new().await;
    let service = s.service();
    declare(
        &service,
        HAROLD_PANE,
        "harold.depth",
        pb::Kind::Gauge,
        vec![attr("queue", &["now", "parked"]), attr("host", &["a", "b"])],
    )
    .await
    .unwrap();
    for (queue, host, value) in [("now", "a", 1.0), ("parked", "b", 2.5), ("now", "a", 3.0)] {
        gauge(
            &service,
            HAROLD_PANE,
            "harold.depth",
            value,
            vec![pair("host", host), pair("queue", queue)],
        )
        .await
        .unwrap();
    }
    drop(service);
    let folder = s.tmp.0.join("db");
    let Setup { store, .. } = s;
    drop(store);
    let reopened = TursoTelemetryStore::open(&folder).await.unwrap();
    assert_eq!(
        reopened
            .get("harold.depth")
            .await
            .unwrap()
            .unwrap()
            .last_values,
        LastValues::from([
            ("host=a,queue=now".to_string(), 3.0),
            ("host=b,queue=parked".to_string(), 2.5),
        ])
    );
}

#[tokio::test]
async fn an_event_with_a_number_range_sends_declared_values_only() {
    let s = Setup::new().await;
    let service = s.service();
    declare_meaning(
        &service,
        CHIEF_PANE,
        "chief.digest",
        pb::Kind::Event,
        "A digest was sent",
        vec![attr("items", &["0-50"])],
    )
    .await
    .unwrap();
    event(
        &service,
        CHIEF_PANE,
        "chief.digest",
        vec![pair("items", "4")],
    )
    .await
    .unwrap();
    for value in ["51", "04", "-1", "four"] {
        let (code, reason, text) = refusal(
            event(
                &service,
                CHIEF_PANE,
                "chief.digest",
                vec![pair("items", value)],
            )
            .await,
        );
        assert_eq!(
            (code, reason.as_str()),
            (Code::InvalidArgument, "undeclared_value")
        );
        assert_eq!(text, format!("items={value} is not declared: use 0-50"));
    }
    assert_eq!(
        s.signals.records(),
        vec![Sent::Event("chief.digest".into(), pairs(&[("items", "4")]))]
    );
}

// ---------------------------------------------------------------------------
// Remove and list
// ---------------------------------------------------------------------------

#[tokio::test]
async fn remove_forgets_the_name_and_a_later_count_is_undeclared() {
    let s = Setup::new().await;
    let service = s.service();
    declare_attempts(&service).await;
    s.signals.clear();
    remove(&service, HAROLD_PANE, ATTEMPTS).await.unwrap();
    assert_eq!(s.signals.records(), vec![Sent::Forget(ATTEMPTS.into())]);
    let (_, reason, _) = refusal(
        count(
            &service,
            HAROLD_PANE,
            ATTEMPTS,
            1,
            vec![pair("result", "ok")],
        )
        .await,
    );
    assert_eq!(reason, "undeclared");
    let (_, reason, _) = refusal(remove(&service, HAROLD_PANE, ATTEMPTS).await);
    assert_eq!(reason, "undeclared");
    assert_eq!(s.signals.records(), vec![Sent::Forget(ATTEMPTS.into())]);
    // Another pane may not remove it.
    declare_attempts(&service).await;
    let (_, reason, _) = refusal(remove(&service, TURSO_PANE, ATTEMPTS).await);
    assert_eq!(reason, "wrong_prefix");
}

#[tokio::test]
async fn list_shows_the_declarations_with_their_meanings() {
    let s = Setup::new().await;
    let service = s.service();
    declare_attempts(&service).await;
    declare_meaning(
        &service,
        CHIEF_PANE,
        "chief.digest",
        pb::Kind::Event,
        "A digest was sent",
        vec![attr("items", &["0-50"])],
    )
    .await
    .unwrap();
    s.signals.clear();
    let logs = CapturedLogs::start();
    let all = list(&service, "").await;
    assert_eq!(
        all,
        vec![
            pb::Signal {
                name: "chief.digest".into(),
                kind: pb::Kind::Event as i32,
                meaning: "A digest was sent".into(),
                attrs: vec![attr("items", &["0-50"])],
            },
            pb::Signal {
                name: ATTEMPTS.into(),
                kind: pb::Kind::Counter as i32,
                meaning: MEANING.into(),
                attrs: vec![attr("result", &["ok", "failed"])],
            },
        ]
    );
    let harold: Vec<String> = list(&service, "harold")
        .await
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert_eq!(harold, [ATTEMPTS]);
    let harold: Vec<String> = list(&service, "harold.")
        .await
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert_eq!(harold, [ATTEMPTS]);
    assert_eq!(logs.text(), "");
    assert!(s.signals.sent().is_empty());
}

// ---------------------------------------------------------------------------
// The calls counter, the refusal log line, and the meaning
// ---------------------------------------------------------------------------

#[tokio::test]
async fn every_write_call_is_counted_with_its_outcome() {
    let s = Setup::new().await;
    let service = s.service();
    declare_attempts(&service).await;
    count(
        &service,
        HAROLD_PANE,
        ATTEMPTS,
        1,
        vec![pair("result", "ok")],
    )
    .await
    .unwrap();
    let _ = count(
        &service,
        TURSO_PANE,
        ATTEMPTS,
        1,
        vec![pair("result", "ok")],
    )
    .await;
    declare(
        &service,
        HAROLD_PANE,
        "harold.depth",
        pb::Kind::Gauge,
        vec![],
    )
    .await
    .unwrap();
    gauge(&service, HAROLD_PANE, "harold.depth", 1.0, vec![])
        .await
        .unwrap();
    declare(
        &service,
        CHIEF_PANE,
        "chief.digest",
        pb::Kind::Event,
        vec![],
    )
    .await
    .unwrap();
    event(&service, CHIEF_PANE, "chief.digest", vec![])
        .await
        .unwrap();
    remove(&service, HAROLD_PANE, "harold.depth").await.unwrap();
    let _ = remove(&service, HAROLD_PANE, "harold.depth").await;
    assert_eq!(
        s.signals.calls(),
        vec![
            call("harold", "declare", "ok"),
            call("harold", "count", "ok"),
            call("harold", "count", "wrong_prefix"),
            call("harold", "declare", "ok"),
            call("harold", "gauge", "ok"),
            call("chief", "declare", "ok"),
            call("chief", "event", "ok"),
            call("harold", "remove", "ok"),
            call("harold", "remove", "undeclared"),
        ]
    );
    for (_, kind, outcome) in s.signals.calls() {
        assert!(crate::telemetry::TELEMETRY_KINDS.contains(&kind.as_str()));
        assert!(crate::telemetry::TELEMETRY_OUTCOMES.contains(&outcome.as_str()));
    }
}

#[tokio::test]
async fn a_refusal_logs_prefix_kind_and_reason_only() {
    let s = Setup::new().await;
    let service = s.service();
    declare_attempts(&service).await;
    let logs = CapturedLogs::start();
    let _ = count(
        &service,
        HAROLD_PANE,
        ATTEMPTS,
        1,
        vec![pair("result", "maybe")],
    )
    .await;
    let text = logs.text();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 1, "{text}");
    let line = lines[0].trim();
    assert!(line.starts_with("INFO harold::"), "{line}");
    assert!(line.contains("telemetry call refused"), "{line}");
    assert!(line.contains("prefix=\"harold\""), "{line}");
    assert!(line.contains("kind=\"count\""), "{line}");
    assert!(line.contains("reason=\"undeclared_value\""), "{line}");
    assert_eq!(line.matches('=').count(), 3, "{line}");
    assert!(!line.contains("maybe"), "{line}");
    assert!(!line.contains(ATTEMPTS), "{line}");
}

#[tokio::test]
async fn an_ok_call_writes_no_log_line() {
    let s = Setup::new().await;
    let service = s.service();
    let logs = CapturedLogs::start();
    declare_attempts(&service).await;
    count(
        &service,
        HAROLD_PANE,
        ATTEMPTS,
        1,
        vec![pair("result", "ok")],
    )
    .await
    .unwrap();
    assert_eq!(logs.text(), "");
}

#[tokio::test]
async fn no_meaning_reaches_the_signals_or_the_log() {
    let s = Setup::new().await;
    let service = s.service();
    let meaning = "zz secret meaning of the release";
    let logs = CapturedLogs::start();
    for (name, kind) in [
        ("harold.c", pb::Kind::Counter),
        ("harold.g", pb::Kind::Gauge),
        ("harold.e", pb::Kind::Event),
    ] {
        declare_meaning(
            &service,
            HAROLD_PANE,
            name,
            kind,
            meaning,
            vec![attr("result", &["ok"])],
        )
        .await
        .unwrap();
    }
    let ok = || vec![pair("result", "ok")];
    count(&service, HAROLD_PANE, "harold.c", 1, ok())
        .await
        .unwrap();
    gauge(&service, HAROLD_PANE, "harold.g", 2.0, ok())
        .await
        .unwrap();
    event(&service, HAROLD_PANE, "harold.e", ok())
        .await
        .unwrap();
    // A refused declaration with the meaning, and a meaning too long.
    let _ = declare_meaning(
        &service,
        HAROLD_PANE,
        "harold.c",
        pb::Kind::Gauge,
        meaning,
        vec![],
    )
    .await;
    let long = format!("{meaning} {}", "x".repeat(80));
    let _ = declare_meaning(
        &service,
        HAROLD_PANE,
        "harold.l",
        pb::Kind::Event,
        &long,
        vec![],
    )
    .await;
    assert_eq!(list(&service, "harold").await[0].meaning, meaning);
    remove(&service, HAROLD_PANE, "harold.c").await.unwrap();
    let sent = format!("{:?}", s.signals.sent());
    assert!(!sent.contains("secret"), "{sent}");
    assert!(!logs.text().contains("secret"), "{}", logs.text());
    assert!(logs.text().contains("telemetry call refused"));
}

// ---------------------------------------------------------------------------
// Through Harold's exporter
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_exported_signals_carry_the_records_and_never_the_meaning() {
    use crate::telemetry::test_support;
    test_support::install();
    let s = Setup::new().await;
    let service = TelemetryToolService::new(
        Arc::clone(&s.store),
        s.guard_with(Ok(s.registry())),
        Arc::new(ExportedSignals),
    );
    let id = uuid::Uuid::new_v4().simple().to_string();
    let counter = format!("chief.c{id}");
    let digest = format!("chief.e{id}");
    let meaning = format!("meaning {id}");
    let result = || vec![attr("result", &["ok", "failed"])];
    declare_meaning(
        &service,
        CHIEF_PANE,
        &counter,
        pb::Kind::Counter,
        &meaning,
        result(),
    )
    .await
    .unwrap();
    declare_meaning(
        &service,
        CHIEF_PANE,
        &digest,
        pb::Kind::Event,
        &meaning,
        result(),
    )
    .await
    .unwrap();
    let metric = format!("agent.{counter}");
    assert_eq!(
        test_support::exact_counter_value(&metric, &[("agent.attr.result", "failed")]),
        Some(0)
    );
    count(
        &service,
        CHIEF_PANE,
        &counter,
        2,
        vec![pair("result", "failed")],
    )
    .await
    .unwrap();
    event(&service, CHIEF_PANE, &digest, vec![pair("result", "ok")])
        .await
        .unwrap();
    assert_eq!(
        test_support::exact_counter_value(&metric, &[("agent.attr.result", "failed")]),
        Some(2)
    );
    assert_eq!(
        test_support::exact_counter_value(&metric, &[("agent.attr.result", "ok")]),
        Some(0)
    );
    let calls = test_support::exact_counter_value(
        "harold.telemetry.calls",
        &[("prefix", "chief"), ("kind", "count"), ("outcome", "ok")],
    );
    assert!(calls.is_some_and(|n| n >= 1), "{calls:?}");
    let exported = test_support::exported_text();
    assert!(exported.contains(&digest), "the agent event is exported");
    assert!(!exported.contains(&meaning), "a meaning is exported");
}
