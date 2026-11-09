use super::*;
use crate::tools::job::test_support::{FakeProcesses, Temp};

async fn service() -> (JobToolService, Arc<FakeProcesses>, Temp) {
    let dir = Temp::new();
    let store = dir.store().await;
    let processes = Arc::new(FakeProcesses::default());
    processes.0.lock().unwrap().insert(4121, 1_700_000_000);
    let svc = JobToolService::new(store, processes.clone(), Arc::new(|| 1_000_000));
    (svc, processes, dir)
}

fn request(name: &str, pid: u32, for_secs: u32) -> pb::AddRequest {
    pb::AddRequest {
        name: name.into(),
        pid,
        for_secs,
        pane_id: "%4".into(),
        what: String::new(),
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
    let listed = svc.list(Request::new(pb::ListRequest {})).await.unwrap().into_inner();
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
    let listed = svc.list(Request::new(pb::ListRequest {})).await.unwrap().into_inner();
    assert!(listed.jobs.is_empty());
    let status = svc
        .remove(Request::new(pb::RemoveRequest { name: "a".into() }))
        .await
        .expect_err("unknown");
    assert_eq!((status.code(), status.message()), (tonic::Code::NotFound, "unknown_job"));
}
