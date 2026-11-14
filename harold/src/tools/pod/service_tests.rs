use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use harold_api::tools::pod::v1 as pb;
use harold_api::tools::pod::v1::pod_service_server::PodService;
use tonic::{Code, Request};

use super::*;
use crate::outbound::fake::CapturedLogs;
use crate::pod_roles::PodStart;
use crate::telemetry::test_support as telemetry_support;

/// A process table that tests change by hand.
#[derive(Default)]
struct FakeProcesses(Mutex<HashMap<u32, i64>>);

impl ProcessPort for FakeProcesses {
    fn start_time(&self, pid: u32) -> Option<i64> {
        self.0.lock().unwrap().get(&pid).copied()
    }
}

/// A folder under the system's temporary folder, removed when dropped.
struct Temp(PathBuf);

impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("harold-pod-tool-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Rig {
    svc: PodToolService,
    processes: Arc<FakeProcesses>,
    roles: Arc<PodRoles>,
    sessions: PathBuf,
    _dir: Temp,
}

fn rig() -> Rig {
    let dir = Temp::new();
    let sessions = dir.0.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let processes = Arc::new(FakeProcesses::default());
    let roles = Arc::new(PodRoles::open(&dir.0, processes.clone()));
    let svc = PodToolService::new(
        Arc::clone(&roles),
        processes.clone(),
        SessionFiles::new(sessions.clone()),
    );
    Rig {
        svc,
        processes,
        roles,
        sessions,
        _dir: dir,
    }
}

impl Rig {
    /// Starts an agent: a live process and a record for `role` of `harold`.
    fn start(&self, role: &str, pid: u32, proc_start: i64) {
        self.processes.0.lock().unwrap().insert(pid, proc_start);
        let product = if role == "chief" { "" } else { "harold" };
        self.roles
            .record(
                PodStart {
                    role: role.into(),
                    product: product.into(),
                    pane_id: format!("%{pid}"),
                    session_id: format!("s-{pid}"),
                    transcript_path: String::new(),
                    folder: "/pod".into(),
                    agent_pid: pid,
                    start_file: String::new(),
                    source: "startup".into(),
                    spec: String::new(),
                },
                1,
            )
            .unwrap();
    }

    fn session_file(&self, pid: u32, text: &str) {
        std::fs::write(self.sessions.join(format!("{pid}.json")), text).unwrap();
    }

    fn named(&self, pid: u32, name: &str) {
        self.session_file(
            pid,
            &format!(r#"{{"pid":{pid},"procStart":"Fri Nov  6 13:10:57 2026","name":"{name}","other":[1]}}"#),
        );
    }

    async fn who(&self, product: &str, role: &str) -> Result<pb::WhoResponse, tonic::Status> {
        self.svc
            .who(Request::new(pb::WhoRequest {
                product: product.into(),
                role: role.into(),
            }))
            .await
            .map(tonic::Response::into_inner)
    }
}

fn reason(result: Result<pb::WhoResponse, tonic::Status>) -> (Code, String) {
    let status = result.unwrap_err();
    (status.code(), status.message().to_owned())
}

#[tokio::test]
async fn a_live_agent_gives_its_session_name_and_record() {
    let rig = rig();
    rig.start("pnem", 10, 1000);
    rig.named(10, "main-b8");
    let answer = rig.who("harold", "pnem").await.unwrap();
    assert_eq!(answer.session_name, "main-b8");
    let record = answer.record.unwrap();
    assert_eq!((record.agent_pid, record.pane_id.as_str()), (10, "%10"));
    assert_eq!(record.agent_proc_start, 1000);
}

#[tokio::test]
async fn the_chief_is_found_with_the_product_dash() {
    let rig = rig();
    rig.start("chief", 11, 1100);
    rig.named(11, "chief-of-pods-a1");
    assert_eq!(
        rig.who("-", "chief").await.unwrap().session_name,
        "chief-of-pods-a1"
    );
}

#[tokio::test]
async fn an_ended_process_gives_agent_gone() {
    let rig = rig();
    rig.start("pnem", 10, 1000);
    rig.named(10, "main-b8");
    rig.processes.0.lock().unwrap().clear();
    assert_eq!(
        reason(rig.who("harold", "pnem").await),
        (Code::FailedPrecondition, "agent_gone".into())
    );
}

#[tokio::test]
async fn a_pid_reused_with_another_start_time_gives_agent_gone() {
    let rig = rig();
    rig.start("pnem", 10, 1000);
    rig.named(10, "main-b8");
    rig.processes.0.lock().unwrap().insert(10, 2000);
    assert_eq!(
        reason(rig.who("harold", "pnem").await),
        (Code::FailedPrecondition, "agent_gone".into())
    );
}

#[tokio::test]
async fn no_record_gives_no_record() {
    let rig = rig();
    assert_eq!(
        reason(rig.who("harold", "pnem").await),
        (Code::NotFound, "no_record".into())
    );
}

#[tokio::test]
async fn a_role_that_is_not_a_pod_role_is_refused() {
    let rig = rig();
    assert_eq!(
        reason(rig.who("harold", "build_worker").await),
        (Code::InvalidArgument, "bad_role".into())
    );
}

#[tokio::test]
async fn a_session_file_that_is_missing_has_no_name_or_is_not_json_is_unreadable() {
    let rig = rig();
    rig.start("pnem", 10, 1000);
    let unreadable = (Code::Unavailable, "session_file_unreadable".to_owned());
    assert_eq!(reason(rig.who("harold", "pnem").await), unreadable);
    for text in [
        "not json",
        r#"{"pid":10,"procStart":"x"}"#,
        r#"{"pid":10,"procStart":"x","name":""}"#,
        r#"{"pid":10,"procStart":"x","name":7}"#,
        r#"{"pid":11,"procStart":"x","name":"other"}"#,
        r#"{"pid":10,"name":"main-b8"}"#,
        "[]",
    ] {
        rig.session_file(10, text);
        assert_eq!(
            reason(rig.who("harold", "pnem").await),
            unreadable,
            "{text}"
        );
    }
}

#[tokio::test]
async fn each_call_logs_one_lookup_line_with_its_outcome() {
    let logs = CapturedLogs::start();
    let rig = rig();
    let _ = rig.who("harold", "pnem").await;
    rig.start("pnem", 10, 1000);
    let _ = rig.who("harold", "pnem").await;
    rig.named(10, "main-b8");
    rig.who("harold", "pnem").await.unwrap();
    let text = logs.text();
    assert_eq!(text.matches("pod role lookup").count(), 3, "{text}");
    for outcome in ["no_record", "session_file_unreadable", "found"] {
        assert!(text.contains(&format!("outcome={outcome}")), "{text}");
    }
    assert!(
        text.contains("product=harold") && text.contains("role=pnem"),
        "{text}"
    );
    assert!(
        !text.contains("main-b8"),
        "the name stays out of the log: {text}"
    );
}

#[tokio::test]
async fn the_counter_counts_a_lookup_by_role_outcome_and_caller() {
    telemetry_support::install();
    let rig = rig();
    let attrs = [
        ("role", "pod_coordination"),
        ("outcome", "no_record"),
        ("caller", "who"),
    ];
    let before =
        telemetry_support::exact_counter_value("harold.pod_role.lookups", &attrs).unwrap_or(0);
    let _ = rig.who("harold", "pod_coordination").await;
    let after = telemetry_support::exact_counter_value("harold.pod_role.lookups", &attrs);
    assert_eq!(after, Some(before + 1));
}

#[tokio::test]
async fn check_refuses_a_bad_role_a_build_worker_with_no_spec_and_a_daemon_with_no_check() {
    let rig = rig();
    let check = |role: &str, spec: &str| {
        rig.svc.check(Request::new(pb::CheckRequest {
            product: "harold".into(),
            role: role.into(),
            spec: spec.into(),
        }))
    };
    let status = check("surveyor", "").await.unwrap_err();
    assert_eq!(
        (status.code(), status.message()),
        (Code::InvalidArgument, "bad_role")
    );
    let status = check("build_worker", "").await.unwrap_err();
    assert_eq!(
        (status.code(), status.message()),
        (Code::InvalidArgument, "bad_spec")
    );
    let status = check("pnem", "").await.unwrap_err();
    assert_eq!(
        (status.code(), status.message()),
        (Code::Unavailable, "restart_check_off")
    );
}
