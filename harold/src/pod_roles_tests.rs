use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::*;
use crate::outbound::fake::CapturedLogs;
use crate::tools::job::process::ProcessPort;

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
        let path = std::env::temp_dir().join(format!("harold-pod-roles-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn roles(folder: &Path, pids: &[(u32, i64)]) -> PodRoles {
    let processes = FakeProcesses::default();
    processes.0.lock().unwrap().extend(pids.iter().copied());
    PodRoles::open(folder, Arc::new(processes))
}

fn start(role: &str, product: &str, pid: u32, session: &str) -> PodStart {
    PodStart {
        role: role.into(),
        product: product.into(),
        pane_id: format!("%{pid}"),
        session_id: session.into(),
        transcript_path: format!("/t/{session}.jsonl"),
        folder: "/pod".into(),
        agent_pid: pid,
        start_file: String::new(),
        source: "startup".into(),
        spec: String::new(),
    }
}

#[test]
fn a_second_start_of_one_role_replaces_the_first_and_logs_replaced_true() {
    let logs = CapturedLogs::start();
    let dir = Temp::new();
    let roles = roles(dir.0.as_path(), &[(10, 1000), (11, 1100)]);
    assert_eq!(
        roles.record(start("pnem", "harold", 10, "s-1"), 5),
        Ok(false)
    );
    assert_eq!(
        roles.record(start("pnem", "harold", 11, "s-2"), 9),
        Ok(true)
    );
    let record = roles.get("harold", "pnem").unwrap();
    assert_eq!(
        (
            record.agent_pid,
            record.session_id.as_str(),
            record.started_at_ms
        ),
        (11, "s-2", 9)
    );
    let text = logs.text();
    assert!(text.contains("pod role recorded"), "{text}");
    assert!(
        text.contains("replaced=false") && text.contains("replaced=true"),
        "{text}"
    );
    assert!(
        text.contains("session.id=s-2") && text.contains("pane_id=%11"),
        "{text}"
    );
}

#[test]
fn the_record_keeps_the_process_start_time_and_every_field() {
    let dir = Temp::new();
    let roles = roles(dir.0.as_path(), &[(10, 1000)]);
    let mut request = start("pnem", "harold", 10, "s-1");
    request.start_file = "/pod/.superpowers/pod/start/pnem.json".into();
    roles.record(request, 77).unwrap();
    let record = roles.get("harold", "pnem").unwrap();
    assert_eq!(record.agent_proc_start, 1000);
    assert_eq!(record.pane_id, "%10");
    assert_eq!(record.folder, "/pod");
    assert_eq!(record.transcript_path, "/t/s-1.jsonl");
    assert_eq!(record.start_file, "/pod/.superpowers/pod/start/pnem.json");
}

#[test]
fn two_roles_in_one_folder_get_two_records() {
    let dir = Temp::new();
    let roles = roles(dir.0.as_path(), &[(10, 1), (11, 2)]);
    roles.record(start("pnem", "harold", 10, "s-1"), 1).unwrap();
    roles
        .record(start("pod_coordination", "harold", 11, "s-2"), 2)
        .unwrap();
    assert_eq!(roles.get("harold", "pnem").unwrap().session_id, "s-1");
    assert_eq!(
        roles.get("harold", "pod_coordination").unwrap().session_id,
        "s-2"
    );
}

#[test]
fn a_clear_source_updates_the_session_id_only() {
    let dir = Temp::new();
    let roles = roles(dir.0.as_path(), &[(10, 1)]);
    roles.record(start("pnem", "harold", 10, "s-1"), 1).unwrap();
    let mut clear = start("pnem", "harold", 99, "s-9");
    clear.source = "clear".into();
    clear.pane_id = "%other".into();
    assert_eq!(roles.record(clear, 50), Ok(false));
    let record = roles.get("harold", "pnem").unwrap();
    assert_eq!((record.session_id.as_str(), record.agent_pid), ("s-9", 10));
    assert_eq!((record.pane_id.as_str(), record.started_at_ms), ("%10", 1));
    assert_eq!(record.transcript_path, "/t/s-1.jsonl");
}

#[test]
fn a_compact_source_before_any_record_writes_no_record() {
    let dir = Temp::new();
    let roles = roles(dir.0.as_path(), &[(10, 1)]);
    let mut compact = start("pnem", "harold", 10, "s-1");
    compact.source = "compact".into();
    assert_eq!(roles.record(compact, 1), Ok(false));
    assert!(roles.get("harold", "pnem").is_none());
}

#[test]
fn an_unknown_role_and_an_empty_product_are_refused() {
    let dir = Temp::new();
    let roles = roles(dir.0.as_path(), &[(10, 1)]);
    assert_eq!(
        roles.record(start("surveyor", "harold", 10, "s"), 1),
        Err(RecordError::UnknownRole)
    );
    assert_eq!(
        roles.record(start("pnem", "", 10, "s"), 1),
        Err(RecordError::BadProduct)
    );
    assert!(roles.get("harold", "build_worker").is_none());
}

fn worker(pid: u32, spec: &str) -> PodStart {
    PodStart {
        spec: spec.into(),
        ..start("build_worker", "harold", pid, &format!("s-{spec}"))
    }
}

#[test]
fn two_build_workers_of_one_product_get_two_records_keyed_by_spec_id() {
    let dir = Temp::new();
    let roles = roles(dir.0.as_path(), &[(10, 1), (11, 2)]);
    assert_eq!(roles.record(worker(10, "spec-a"), 1), Ok(false));
    assert_eq!(roles.record(worker(11, "spec-b"), 2), Ok(false));
    assert_eq!(
        roles
            .get_spec("harold", "build_worker", "spec-a")
            .unwrap()
            .agent_pid,
        10
    );
    assert_eq!(
        roles
            .get_spec("harold", "build_worker", "spec-b")
            .unwrap()
            .agent_pid,
        11
    );
    assert_eq!(roles.record(worker(12, "spec-a"), 3), Ok(true));
}

#[test]
fn a_build_worker_with_no_spec_id_or_a_bad_one_is_refused_and_a_pod_role_ignores_the_spec() {
    let dir = Temp::new();
    let roles = roles(dir.0.as_path(), &[(10, 1)]);
    assert_eq!(roles.record(worker(10, ""), 1), Err(RecordError::BadSpec));
    assert_eq!(
        roles.record(worker(10, "a/b"), 1),
        Err(RecordError::BadSpec)
    );
    let pnem = PodStart {
        spec: "x".into(),
        ..start("pnem", "harold", 10, "s")
    };
    assert_eq!(roles.record(pnem, 1), Ok(false));
    assert!(roles.get("harold", "pnem").is_some());
}

#[test]
fn an_old_record_file_with_a_two_part_key_still_reads() {
    let dir = Temp::new();
    let first = roles(dir.0.as_path(), &[(10, 1)]);
    first.record(start("pnem", "harold", 10, "s-1"), 1).unwrap();
    let text = std::fs::read_to_string(dir.0.join(RECORD_FILE)).unwrap();
    assert!(text.contains("\"harold/pnem\""));
    let again = roles(dir.0.as_path(), &[(10, 1)]);
    assert_eq!(again.get("harold", "pnem").unwrap().session_id, "s-1");
}

#[test]
fn an_agent_pid_of_zero_is_refused_for_every_source() {
    let dir = Temp::new();
    let roles = roles(dir.0.as_path(), &[]);
    for source in ["startup", "resume", "clear", "compact"] {
        let mut request = start("pnem", "harold", 0, "s");
        request.source = source.into();
        assert_eq!(
            roles.record(request, 1),
            Err(RecordError::BadPid),
            "{source}"
        );
    }
    assert!(roles.get("harold", "pnem").is_none());
}

#[test]
fn the_chief_has_the_product_dash_whatever_the_hook_sends() {
    let dir = Temp::new();
    let roles = roles(dir.0.as_path(), &[(10, 1)]);
    roles.record(start("chief", "", 10, "s-c"), 1).unwrap();
    assert_eq!(roles.get("-", "chief").unwrap().session_id, "s-c");
}

#[test]
fn a_record_survives_a_new_open_and_a_corrupt_file_reads_as_no_record() {
    let dir = Temp::new();
    roles(dir.0.as_path(), &[(10, 1)])
        .record(start("pnem", "harold", 10, "s-1"), 1)
        .unwrap();
    let again = roles(dir.0.as_path(), &[]);
    assert_eq!(again.get("harold", "pnem").unwrap().session_id, "s-1");
    std::fs::write(dir.0.as_path().join(RECORD_FILE), "{ not json").unwrap();
    assert!(roles(dir.0.as_path(), &[]).get("harold", "pnem").is_none());
}

#[test]
fn chief_pane_gives_the_installed_chief_pane_and_skips_a_record_with_no_pane() {
    reset();
    assert_eq!(chief_pane(), None);
    let dir = Temp::new();
    let roles = Arc::new(roles(dir.0.as_path(), &[(10, 1)]));
    install(Arc::clone(&roles));
    assert_eq!(chief_pane(), None);
    let mut chief = start("chief", "", 10, "s-c");
    chief.pane_id = String::new();
    roles.record(chief, 1).unwrap();
    assert_eq!(chief_pane(), None);
    roles.record(start("chief", "", 10, "s-c"), 2).unwrap();
    assert_eq!(chief_pane(), Some("%10".to_string()));
    reset();
}
