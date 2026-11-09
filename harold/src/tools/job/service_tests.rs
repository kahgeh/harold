use super::*;
use crate::tools::job::test_support::{FakeProcesses, FakeUnits, Temp};
use crate::tools::job::unit::UnitState;

async fn service() -> (JobToolService, Arc<FakeProcesses>, Temp) {
    let (svc, processes, _units, dir) = service_on(true).await;
    (svc, processes, dir)
}

async fn service_on(systemd: bool) -> (JobToolService, Arc<FakeProcesses>, Arc<FakeUnits>, Temp) {
    let dir = Temp::new();
    let store = dir.store().await;
    let processes = Arc::new(FakeProcesses::default());
    processes.0.lock().unwrap().insert(4121, 1_700_000_000);
    let units = Arc::new(FakeUnits::default());
    let svc = JobToolService::new(
        store,
        processes.clone(),
        units.clone(),
        systemd,
        Arc::new(|| 1_000_000),
        Arc::new(crate::tools::job::feed::JobsFeed::new()),
    );
    (svc, processes, units, dir)
}

fn request(name: &str, pid: u32, for_secs: u32) -> pb::AddRequest {
    pb::AddRequest {
        name: name.into(),
        pid,
        for_secs,
        pane_id: "%4".into(),
        what: String::new(),
        ..Default::default()
    }
}

fn unit_request(name: &str, unit: &str) -> pb::AddRequest {
    pb::AddRequest {
        name: name.into(),
        unit: unit.into(),
        user: true,
        for_secs: 1200,
        pane_id: "%4".into(),
        ..Default::default()
    }
}

fn loaded() -> UnitState {
    UnitState {
        active: "active".into(),
        sub: "running".into(),
        load: "loaded".into(),
        ..Default::default()
    }
}

async fn refusal(svc: &JobToolService, req: pb::AddRequest) -> (tonic::Code, String) {
    let status = svc.add(Request::new(req)).await.expect_err("refused");
    (status.code(), status.message().to_string())
}

#[tokio::test]
async fn add_accepts_a_running_pid_and_list_shows_it() {
    let (svc, _processes, _dir) = service().await;
    let added = svc
        .add(Request::new(request("calibration", 4121, 1200)))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(added.started_at_ms, 1_000_000);
    assert_eq!(added.due_at_ms, 1_000_000 + 1_200_000);
    let listed = svc
        .list(Request::new(pb::ListRequest {}))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(listed.jobs.len(), 1);
    assert_eq!(listed.jobs[0].name, "calibration");
    assert_eq!(listed.jobs[0].state, "running");
    assert_eq!(listed.jobs[0].pid, 4121);
}

#[tokio::test]
async fn add_refuses_a_pid_that_does_not_run_a_bad_name_a_bad_time_and_a_second_name() {
    let (svc, _processes, _dir) = service().await;
    assert_eq!(
        refusal(&svc, request("a", 9999, 60)).await,
        (tonic::Code::NotFound, "no_such_process".into())
    );
    assert_eq!(
        refusal(&svc, request("Bad Name", 4121, 60)).await,
        (tonic::Code::InvalidArgument, "invalid name".into())
    );
    assert_eq!(
        refusal(&svc, request("a", 4121, 0)).await,
        (tonic::Code::InvalidArgument, "invalid for_secs".into())
    );
    svc.add(Request::new(request("a", 4121, 60))).await.unwrap();
    assert_eq!(
        refusal(&svc, request("a", 4121, 60)).await,
        (tonic::Code::FailedPrecondition, "name_in_use".into())
    );
}

#[tokio::test]
async fn remove_takes_the_job_away_and_refuses_an_unknown_name() {
    let (svc, _processes, _dir) = service().await;
    svc.add(Request::new(request("a", 4121, 60))).await.unwrap();
    svc.remove(Request::new(pb::RemoveRequest { name: "a".into() }))
        .await
        .unwrap();
    let listed = svc
        .list(Request::new(pb::ListRequest {}))
        .await
        .unwrap()
        .into_inner();
    assert!(listed.jobs.is_empty());
    let status = svc
        .remove(Request::new(pb::RemoveRequest { name: "a".into() }))
        .await
        .expect_err("unknown");
    assert_eq!(
        (status.code(), status.message()),
        (tonic::Code::NotFound, "unknown_job")
    );
}

const REMAIN_WARNING: &str =
    "the exit code will be unknown: start the unit with --remain-after-exit";

#[tokio::test]
async fn add_accepts_a_loaded_unit_and_list_shows_its_unit() {
    let (svc, _processes, units, _dir) = service_on(true).await;
    units.set("cal-1", true, loaded());
    units
        .remain
        .lock()
        .unwrap()
        .insert(("cal-1".into(), true), true);
    let added = svc
        .add(Request::new(unit_request("cal", "cal-1")))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(added.warning, "");
    let listed = svc
        .list(Request::new(pb::ListRequest {}))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        (listed.jobs[0].unit.as_str(), listed.jobs[0].pid),
        ("cal-1", 0)
    );
}

#[tokio::test]
async fn a_unit_without_remain_after_exit_adds_the_job_and_warns() {
    let (svc, _processes, units, _dir) = service_on(true).await;
    units.set("cal-1", true, loaded());
    units
        .remain
        .lock()
        .unwrap()
        .insert(("cal-1".into(), true), false);
    let added = svc
        .add(Request::new(unit_request("cal", "cal-1")))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(added.warning, REMAIN_WARNING);
    let listed = svc
        .list(Request::new(pb::ListRequest {}))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(listed.jobs.len(), 1);
}

#[tokio::test]
async fn add_refuses_a_unit_that_is_not_found_a_bad_unit_name_and_both_pid_and_unit() {
    let (svc, _processes, units, _dir) = service_on(true).await;
    // A `systemctl` that cannot run is not "no such unit".
    assert_eq!(
        refusal(&svc, unit_request("cal", "cal-1")).await,
        (tonic::Code::Unavailable, "systemctl_failed".into())
    );
    units.set(
        "cal-1",
        true,
        UnitState {
            load: "not-found".into(),
            ..Default::default()
        },
    );
    assert_eq!(
        refusal(&svc, unit_request("cal", "cal-1")).await,
        (tonic::Code::NotFound, "no_such_unit".into())
    );
    assert_eq!(
        refusal(&svc, unit_request("cal", "--now")).await,
        (tonic::Code::InvalidArgument, "invalid unit".into())
    );
    assert_eq!(
        refusal(&svc, unit_request("cal", "a b")).await,
        (tonic::Code::InvalidArgument, "invalid unit".into())
    );
    let mut both = unit_request("cal", "cal-1");
    both.pid = 4121;
    assert_eq!(
        refusal(&svc, both).await,
        (tonic::Code::InvalidArgument, "invalid pid".into())
    );
}

#[tokio::test]
async fn a_unit_is_refused_where_there_is_no_systemd() {
    let (svc, _processes, units, _dir) = service_on(false).await;
    units.set("cal-1", true, loaded());
    assert_eq!(
        refusal(&svc, unit_request("cal", "cal-1")).await,
        (tonic::Code::FailedPrecondition, "no_systemd".into())
    );
}

#[tokio::test]
async fn list_shows_keeps_awake_for_a_job_with_its_file_and_remove_deletes_the_file() {
    use crate::settings::JobSettings;
    use crate::tools::job::awake::KeepAwake;
    let (svc, _processes, _units, _dir) = service_on(true).await;
    let folder = Temp::new();
    let awake = Arc::new(
        KeepAwake::new(&JobSettings {
            keep_awake_folder: folder.0.display().to_string(),
            keep_awake_every_s: 300,
        })
        .unwrap(),
    );
    let svc = svc.with_awake(Some(awake.clone()));
    svc.add(Request::new(request("a", 4121, 60))).await.unwrap();
    awake.tick(&["a".to_string()], 0);
    let listed = svc
        .list(Request::new(pb::ListRequest {}))
        .await
        .unwrap()
        .into_inner();
    assert!(listed.jobs[0].keeps_awake);
    svc.remove(Request::new(pb::RemoveRequest { name: "a".into() }))
        .await
        .unwrap();
    assert!(!folder.0.join("harold-job-a").exists());
}

#[tokio::test]
async fn add_writes_the_keep_awake_file_at_once() {
    use crate::settings::JobSettings;
    use crate::tools::job::awake::KeepAwake;
    let (svc, _processes, _units, _dir) = service_on(true).await;
    let folder = Temp::new();
    let awake = Arc::new(
        KeepAwake::new(&JobSettings {
            keep_awake_folder: folder.0.display().to_string(),
            keep_awake_every_s: 300,
        })
        .unwrap(),
    );
    let svc = svc.with_awake(Some(awake));
    svc.add(Request::new(request("a", 4121, 60))).await.unwrap();
    assert!(folder.0.join("harold-job-a").exists());
}
